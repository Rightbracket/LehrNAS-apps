#!/usr/bin/env python3
"""Release one app from this repository: build its image, push it, sign it,
and write its entry into a catalog file (docs/spec-runner-app.md §18.8 in
LehrNAS, #259). It's made for a workflow on a self-hosted runner, but runs
anywhere that has the tools.

    python3 tools/release.py apps/<app> --catalog official.toml --tag next
        [--as <dir>/<app>.toml] [--no-push] [--commit]

What it does:
1. **Refuses** before building anything:
   - a dirty tree, or a tag that isn't `<id>-YYYY.MMDD.N` or is taken;
   - an image `official.toml` names, released into any other catalog;
   - a key whose public half isn't the file the catalog's `[channel] key`
     names, and, for any catalog but `official.toml`, a key whose public half
     is `app-channel.pub`;
   - with `--as`, an identity that collides with the app's own: the same id,
     an id that isn't `<app id>-…`, the same port, or an image that isn't
     `docker.io/<namespace>/<id>` or is an official one.
2. **Builds** the app's static broker for both architectures, with this
   machine's paths remapped, and scans the binaries for any that are left. Then
   buildx builds both architectures into one OCI archive.
3. **Pushes** to the entry's one repository and checks it landed as built and
   reads without a login. Then it signs the list and each image with cosign, in
   the legacy layout podman's policy reads, with no transparency log.
4. **Writes** the compose with the tag (and, with `--as`, the identity's image
   and port, at `<dir>/<id>/compose.yaml`). Then it fills the entry, writes it
   into the catalog and signs the catalog (`<catalog>.sig`).
5. **With `--commit`,** commits those files and tags the commit. Nothing is
   pushed: the caller pushes.

`--as <dir>/<app>.toml` releases the app under another identity: `app`, `id`,
`name`, `image` and `port`, every key and nothing else. The app's own files
are never changed.

Its inputs from the environment:
- `SIGNING_KEY`: the cosign private key's text, read by cosign as
  `env://SIGNING_KEY` and never written to disk, with `COSIGN_PASSWORD`.
- `REGISTRY_AUTH_FILE`: the registry login, a `config.json` that skopeo reads
  and cosign reads as `$DOCKER_CONFIG`. The build never sees it, or the key.
  The two look a Docker Hub login up under different keys: skopeo's is
  `docker.io` (what `skopeo login` writes), cosign's `https://index.docker.io/v1/`
  (what `cosign login docker.io` writes). A file without cosign's is refused
  before anything is pushed, so no tag goes up that can't be signed.
- `BUILDX_BUILDER` (optional): the builder buildx uses.

`--no-push` is a rehearsal: it builds and fills, with no registry and no tag,
and signs the catalog only when a key is given.

It prints one line per step (`IMAGE |`, `PUSHED |`, `SIGNED |`, `CHANNEL |`,
`TAGGED |`). A refusal is `REFUSED | RELEASE | <why>` and exit 2. Standard
library only (Python 3.11 or newer), like `tools/check.py`.
"""

from __future__ import annotations

import argparse
import datetime
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import tomllib
from dataclasses import dataclass
from pathlib import Path

OFFICIAL = "official.toml"
APP_CHANNEL_PUB = "app-channel.pub"
KEY_REF = "env://SIGNING_KEY"
TRIPLES = {"amd64": "x86_64-unknown-linux-musl", "arm64": "aarch64-unknown-linux-musl"}
INDEX_TYPES = (
    "application/vnd.oci.image.index.v1+json",
    "application/vnd.docker.distribution.manifest.list.v2+json",
)
#: cosign 3's flags for the legacy signature (`.sig` beside the image, a bare
#: signature file for a catalog) with no transparency log.
LEGACY = ["--new-bundle-format=false", "--use-signing-config=false", "--tlog-upload=false"]
#: A Docker Hub repository's name, which an identity's id becomes.
NAME_RE = re.compile(r"[a-z0-9]+(?:[._-][a-z0-9]+)*")
#: A built machine's own folders, which a published binary must not name.
LOCAL_PATH = re.compile(rb"/(?:Users|Volumes)/[^\x00\s\"']{1,80}")


class Refused(Exception):
    """A release that can't go on, with the fact."""


# ---- names ----


def tag_ok(tag: str, prefix: str) -> bool:
    """`<prefix>-YYYY.MMDD.N`: a day and a count."""
    m = re.match(rf"^{re.escape(prefix)}-(20\d{{2}})\.(\d{{4}})\.(\d+)$", tag)
    if not m:
        return False
    mmdd = int(m.group(2))
    return 101 <= mmdd <= 1231 and 1 <= mmdd % 100 <= 31 and int(m.group(3)) >= 1


def next_tag(prefix: str, tags: list[str], today: datetime.date) -> str:
    """The day's next free `<prefix>-YYYY.MMDD.N`."""
    day = f"{prefix}-{today:%Y.%m%d}."
    taken = [int(t[len(day) :]) for t in tags if t.startswith(day) and t[len(day) :].isdigit()]
    return f"{day}{max(taken, default=0) + 1}"


def repository_name(ref: str) -> str:
    """An image repository as Docker Hub resolves it: `eshork/x`,
    `index.docker.io/eshork/x` and `docker.io/eshork/x` are one repository."""
    name = ref.strip().lower()
    for host in ("docker.io/", "index.docker.io/", "registry-1.docker.io/"):
        if name.startswith(host):
            name = name[len(host) :]
            break
    else:
        first = name.split("/", 1)[0]
        if "/" in name and ("." in first or ":" in first or first == "localhost"):
            return name
    if "/" not in name:
        name = f"library/{name}"
    return f"docker.io/{name}"


def official_repositories(official: str) -> set[str]:
    """Every image repository `official.toml` names, normalized."""
    out = set()
    for app in tomllib.loads(official).get("apps", []):
        if "image" in app:
            out.add(repository_name(app["image"]))
        out |= {repository_name(r["image"]) for r in app.get("images", [])}
    return out


def only_repository(ref: str, repo: str) -> str:
    """Nothing but the entry's repository is ever pushed or signed."""
    if not ref.startswith((f"{repo}:", f"{repo}@")):
        raise Refused(f"{ref} is not in {repo}; this tool writes to that repository only")
    return ref


# ---- the entry and its files ----


def the_app(template: str) -> dict:
    apps = tomllib.loads(template).get("apps", [])
    if len(apps) != 1:
        raise Refused(f"the entry template has {len(apps)} apps, not one")
    return apps[0]


def repository(template: str) -> str:
    """The one repository the entry names: every service pins the same image."""
    repos = {r["image"] for r in the_app(template).get("images", [])}
    if len(repos) != 1:
        raise Refused(f"the entry names {sorted(repos)}; every service pins one image")
    return repos.pop()


def port_pair(template: str) -> tuple[int, int]:
    """The app's one LAN port, `host:container`."""
    ports = the_app(template).get("permissions", {}).get("ports", [])
    if len(ports) != 1:
        raise Refused(f"the entry asks for ports {ports}; another identity moves exactly one")
    m = re.fullmatch(r"(\d+):(\d+)/tcp", ports[0])
    if not m:
        raise Refused(f"the entry's port {ports[0]} isn't host:container/tcp")
    return int(m.group(1)), int(m.group(2))


@dataclass(frozen=True)
class Identity:
    app: str
    id: str
    name: str
    image: str
    port: int


def identity(text: str) -> Identity:
    """An `--as` file: every key and nothing else."""
    try:
        d = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        raise Refused(f"the identity doesn't parse: {e}") from e
    want = {"app", "id", "name", "image", "port"}
    if set(d) != want:
        raise Refused(f"the identity has {sorted(d)}; it wants {sorted(want)}")
    for key in ("app", "id", "name", "image"):
        if not isinstance(d[key], str) or not d[key]:
            raise Refused(f"the identity's {key} {d[key]!r} isn't a name")
    if re.search(r'["\\\x00-\x1f\x7f]', d["name"]):
        raise Refused(f"the identity's name {d['name']!r} has a quote, a backslash or a control")
    for key in ("app", "id"):
        if not NAME_RE.fullmatch(d[key]):
            raise Refused(f"the identity's {key} {d[key]!r} isn't a repository name")
    port = d["port"]
    if isinstance(port, bool) or not isinstance(port, int) or not 1024 <= port <= 65535:
        raise Refused(f"the identity's port {port!r} isn't a port above 1023")
    return Identity(**d)


def identity_refusals(ident: Identity, template: str, official: str) -> list[str]:
    """What makes another identity collide with the app's own."""
    app = the_app(template)
    out = []
    if ident.app != Path(app["compose_path"]).parent.name:
        out.append(f"the identity is for {ident.app}, not {Path(app['compose_path']).parent.name}")
    if ident.id == app["id"]:
        out.append(f"the identity's id is the app's own, {app['id']}")
    if not ident.id.startswith(f"{app['id']}-"):
        out.append(f"the identity's id {ident.id} doesn't start with {app['id']}-")
    if not re.fullmatch(rf"docker\.io/[a-z0-9][a-z0-9._-]*/{re.escape(ident.id)}", ident.image):
        out.append(f"the identity's image {ident.image} isn't docker.io/<namespace>/{ident.id}")
    official_names = official_repositories(official) | {repository_name(repository(template))}
    if repository_name(ident.image) in official_names:
        out.append(f"{ident.image} is an official repository")
    if ident.port == port_pair(template)[0]:
        out.append(f"port {ident.port} is the app's own")
    return out


def _once(text: str, old: str, new: str, what: str) -> str:
    n = text.count(old)
    if n != 1:
        raise Refused(f"{what}: {old!r} is there {n} times, not once")
    return text.replace(old, new)


def compose_text(compose: str, version: str, repo: str) -> str:
    """The compose with this release's tag on every service's image."""
    out, n = re.subn(rf"(image: {re.escape(repo)}):\S+", rf"\1:{version}", compose)
    if n == 0:
        raise Refused(f"the compose names no image in {repo}")
    return out


def identity_compose(compose: str, ident: Identity, repo: str, ports: tuple[int, int]) -> str:
    """The app's compose with the identity's image and port (the tag comes after)."""
    host, inside = ports
    out = _once(compose, f'"{host}:{inside}/tcp"', f'"{ident.port}:{inside}/tcp"', "compose port")
    return re.sub(rf"(image: ){re.escape(repo)}(?=:)", rf"\g<1>{ident.image}", out)


def identity_template(template: str, ident: Identity, compose_path: str) -> str:
    """The app's entry template with the identity's id, name, image, compose
    path and port, checked by parsing it."""
    app = the_app(template)
    repo = repository(template)
    host, inside = port_pair(template)
    t = _once(template, f'id = "{app["id"]}"', f'id = "{ident.id}"', "entry id")
    t = _once(t, f'name = "{app["name"]}"', f'name = "{ident.name}"', "entry name")
    t = _once(
        t, f'compose_path = "{app["compose_path"]}"',
        f'compose_path = "{compose_path}"', "entry compose_path",
    )  # fmt: skip
    t = _once(t, f'"{host}:{inside}/tcp"', f'"{ident.port}:{inside}/tcp"', "entry port")
    t = t.replace(f'image = "{repo}"', f'image = "{ident.image}"')
    try:
        got = the_app(t)
    except tomllib.TOMLDecodeError as e:
        raise Refused(f"the entry doesn't parse with the identity put in: {e}") from e
    images = {r["image"] for r in got["images"]}
    if (got["id"], got["name"], images) != (ident.id, ident.name, {ident.image}):
        raise Refused("the entry didn't take the identity's id, name and image")
    return t


def entry_text(
    template: str, version: str, digests: dict[str, str], released: str, compose: str
) -> str:
    """The template with its blanks filled: the version, the day, every
    service's digests and the compose file's hash."""
    fill = {**digests, "compose_sha256": hashlib.sha256(compose.encode()).hexdigest()}
    lines = []
    for line in template.splitlines():
        key = line.split("=", 1)[0].strip() if "=" in line else ""
        if key == "version":
            line = f'version = "{version}"'
        elif key == "released":
            line = f'released = "{released}"'
        elif key in fill:
            line = f'{key} = "{fill[key]}"'
        lines.append(line)
    text = "\n".join(lines) + "\n"
    app = the_app(text)
    for row in app["images"]:
        for key in ("list", "amd64", "arm64"):
            if row[key] != digests[key]:
                raise Refused(f"the entry's {row['service']}.{key} wasn't filled")
    return text


def _entry_parts(channel: str) -> tuple[str, list[str]]:
    """The catalog's head and its entries. An entry is an `[[apps]]` table with
    the comment block right above it, so a release that replaces the entry
    replaces its comment too, instead of leaving one more copy behind."""
    lines = channel.splitlines(keepends=True)
    starts = []
    for i, line in enumerate(lines):
        if line.rstrip("\n") == "[[apps]]":
            j = i
            while j > 0 and (lines[j - 1].startswith("#") or not lines[j - 1].strip()):
                j -= 1
            while not lines[j].strip():  # the blank lines above it are the gap, not the entry
                j += 1
            starts.append(max(j, starts[-1] + 1 if starts else 0))
    cuts = [*starts, len(lines)]
    head = "".join(lines[: cuts[0]])
    return head, ["".join(lines[a:b]) for a, b in zip(cuts, cuts[1:])]


def channel_text(channel: str, entry: str) -> str:
    """The catalog with the entry's app replaced (or added at the end): entries
    are `[[apps]]` tables, each with its comment; the catalog's own table stays."""
    app_id = the_app(entry)["id"]
    head, apps = _entry_parts(channel)
    kept = [a for a in apps if not re.search(rf'(?m)^id = "{re.escape(app_id)}"\s*$', a)]
    text = head.rstrip("\n") + "\n\n" + "\n".join(a.rstrip("\n") + "\n" for a in kept)
    text = text.rstrip("\n") + "\n\n" + entry.strip("\n") + "\n"
    tomllib.loads(text)
    return text


def catalog_key(catalog: str) -> str:
    """The public key's file name the catalog's `[channel]` names."""
    key = tomllib.loads(catalog).get("channel", {}).get("key")
    if not isinstance(key, str) or not key or "/" in key:
        raise Refused(f"the catalog names no key file beside it ({key!r})")
    return key


def key_refusals(
    catalog_name: str, derived: bytes, named: bytes, app_pub: bytes | None
) -> list[str]:
    """The signing key against the catalog: its public half must be the file
    the catalog names; the app-channel key signs `official.toml` alone."""
    out = []
    if derived.strip() != named.strip():
        out.append("the signing key's public half isn't the key the catalog names")
    if catalog_name != OFFICIAL and app_pub is not None and derived.strip() == app_pub.strip():
        out.append(f"the app-channel key signs {OFFICIAL} alone, never {catalog_name}")
    return out


def official_image_refusal(catalog_name: str, repo: str, official: str) -> str | None:
    """An image `official.toml` names goes into `official.toml` alone."""
    if catalog_name != OFFICIAL and repository_name(repo) in official_repositories(official):
        return f"{repo} is an official repository; it's released into {OFFICIAL} alone"
    return None


def digests_of_index(index: dict, list_digest: str) -> dict[str, str]:
    """The list's digest and each architecture's, from the manifest list."""
    if index.get("mediaType") not in INDEX_TYPES:
        raise Refused("the image's top manifest isn't a manifest list")
    out = {"list": list_digest}
    for m in index["manifests"]:
        arch = m.get("platform", {}).get("architecture")
        if arch in TRIPLES:
            out[arch] = m["digest"]
    if set(out) != {"list", *TRIPLES}:
        raise Refused(f"the list has {sorted(out)}; it should have both architectures")
    return out


def archive_digests(archive: Path) -> dict[str, str]:
    """An OCI archive's manifest list, by digest and its architectures'."""
    with tarfile.open(archive) as t:
        top = json.loads(t.extractfile("index.json").read())  # type: ignore[union-attr]
        if len(top["manifests"]) != 1:
            raise Refused(f"{archive}: not one image")
        digest = top["manifests"][0]["digest"]
        blob = t.extractfile(f"blobs/sha256/{digest.removeprefix('sha256:')}")
        index = json.loads(blob.read())  # type: ignore[union-attr]
    return digests_of_index(index, digest)


def local_paths(files: list[Path], roots: list[str]) -> list[str]:
    """The first few local paths in the binaries: a home or volume folder, or
    any of `roots` (the checkout, the home folder)."""
    found = []
    words = [r.encode() for r in roots if r]
    for p in files:
        raw = p.read_bytes()
        found += [f"{p.name}: {m.decode('latin1')}" for m in LOCAL_PATH.findall(raw)]
        found += [f"{p.name}: {w.decode()}" for w in words if w in raw]
    return found[:5]


def remap_flags(home: Path, root: Path) -> str:
    """cargo's encoded rustflags: the toolchain, the crates and the checkout
    become neutral names in the binary."""
    maps = [(home / ".cargo", "/cargo"), (home / ".rustup", "/rustup"), (root, "/src")]
    return "\x1f".join(f"--remap-path-prefix={p}={to}" for p, to in maps)


def build_command(version: str, archive: Path, context: Path, builder: str | None) -> list[str]:
    """Both architectures in one OCI archive, without buildx's attestations,
    which would add manifests a box doesn't know. The tag goes in as
    `APP_RELEASE`, and the image's build fails without it."""
    cmd = ["docker", "buildx", "build"]
    if builder:
        cmd += ["--builder", builder]
    return cmd + [
        "--platform", "linux/amd64,linux/arm64",
        "--provenance=false", "--sbom=false",
        "--build-arg", f"APP_RELEASE={version}",
        "--output", f"type=oci,dest={archive}",
        str(context),
    ]  # fmt: skip


# ---- running it ----


def _sh(*cmd: str, cwd: Path | None = None, env: dict[str, str] | None = None) -> str:
    r = subprocess.run(cmd, capture_output=True, text=True, cwd=cwd, env=env, check=False)
    if r.returncode != 0:
        raise Refused(f"{cmd[0]} {cmd[1] if len(cmd) > 1 else ''} failed: "
                      f"{(r.stderr or r.stdout).strip()[-800:]}")  # fmt: skip
    return r.stdout


def _tool(name: str) -> str:
    p = shutil.which(name)
    if not p:
        raise Refused(f"{name} isn't on PATH")
    return p


def _env() -> dict[str, str]:
    """skopeo's and cosign's environment: skopeo reads `REGISTRY_AUTH_FILE`;
    cosign reads the same file as `$DOCKER_CONFIG/config.json`."""
    env = {**os.environ, "COSIGN_YES": "true"}
    auth = Path(env.get("REGISTRY_AUTH_FILE", ""))
    if auth.name == "config.json":
        env["DOCKER_CONFIG"] = str(auth.parent)
    return env


#: The key cosign (go-containerregistry) looks a Docker Hub login up under.
COSIGN_HUB_KEY = "https://index.docker.io/v1/"


def cosign_login_refusal(config: dict) -> str | None:
    """Why cosign won't find a Docker Hub login in this docker config, or None.
    Only the keys are read: a value is never looked at or printed."""
    if config.get("credsStore"):
        return None
    helpers = config.get("credHelpers") or {}
    if COSIGN_HUB_KEY in helpers or "index.docker.io" in helpers:
        return None
    if COSIGN_HUB_KEY in (config.get("auths") or {}):
        return None
    return (
        f"cosign finds no Docker Hub login in the auth file: it reads the key {COSIGN_HUB_KEY}, "
        "which `cosign login docker.io` writes (skopeo's login is under docker.io)"
    )


def _build_env() -> dict[str, str]:
    """buildx's: the account's own docker config, never the login's folder."""
    return {k: v for k, v in os.environ.items() if k not in ("DOCKER_CONFIG", "SIGNING_KEY")}


def _key_given() -> bool:
    return bool(os.environ.get("SIGNING_KEY"))


def release(args: argparse.Namespace) -> int:
    try:
        return _release(args)
    except Refused as e:
        print(f"REFUSED | RELEASE | {e}")
        return 2


def _release(args: argparse.Namespace) -> int:
    root = Path(_sh("git", "rev-parse", "--show-toplevel").strip())

    def git(*a: str) -> str:
        return _sh("git", "-C", str(root), *a).strip()

    if git("status", "--porcelain"):
        raise Refused(f"{root} has uncommitted changes")
    app_dir = (root / args.app).resolve()
    for need in (app_dir / "entry.toml", app_dir / "compose.yaml", root / OFFICIAL):
        if not need.is_file():
            raise Refused(f"no {need.relative_to(root)}")
    catalog_path = root / args.catalog
    if not catalog_path.is_file():
        raise Refused(f"no catalog {args.catalog}: its [channel] head comes first")
    template = (app_dir / "entry.toml").read_text()
    official = (root / OFFICIAL).read_text()
    catalog = catalog_path.read_text()
    repo = repository(template)
    compose = (app_dir / "compose.yaml").read_text()
    compose_out = app_dir / "compose.yaml"
    ident = None
    if args.as_:
        ident_path = (root / args.as_).resolve()
        ident = identity(ident_path.read_text())
        if bad := identity_refusals(ident, template, official):
            raise Refused("; ".join(bad))
        compose_out = ident_path.parent / ident.id / "compose.yaml"
        rel = compose_out.relative_to(root).as_posix()
        compose = identity_compose(compose, ident, repo, port_pair(template))
        template = identity_template(template, ident, rel)
        repo = ident.image
    if why := official_image_refusal(catalog_path.name, repo, official):
        raise Refused(why)
    app_id = the_app(template)["id"]
    tags = git("tag", "-l").splitlines()
    tag = args.tag
    if tag == "next":
        tag = next_tag(app_id, tags, datetime.datetime.now(datetime.UTC).date())
    if not tag_ok(tag, app_id):
        raise Refused(f"{tag}: the tag is {app_id}-YYYY.MMDD.N")
    if tag in tags:
        raise Refused(f"{tag} is already tagged")
    named = root / catalog_key(catalog)
    if not named.is_file():
        raise Refused(f"no {named.name} beside the catalog")
    cosign = skopeo = ""
    if args.push or _key_given():
        cosign = _tool("cosign")
        if not _key_given():
            raise Refused("no signing key: SIGNING_KEY is empty")
        derived = _sh(cosign, "public-key", "--key", KEY_REF, env=_env()).encode()
        app_pub = root / APP_CHANNEL_PUB
        app_bytes = app_pub.read_bytes() if app_pub.is_file() else None
        if bad := key_refusals(catalog_path.name, derived, named.read_bytes(), app_bytes):
            raise Refused("; ".join(bad))
    if args.push:
        skopeo = _tool("skopeo")
        r = subprocess.run(
            [skopeo, "login", "--get-login", "docker.io"],
            capture_output=True, text=True, env=_env(), check=False,
        )  # fmt: skip
        if r.returncode != 0 or not r.stdout.strip():
            raise Refused("no registry login for docker.io (REGISTRY_AUTH_FILE)")
        if docker_config := _env().get("DOCKER_CONFIG"):
            try:
                config = json.loads((Path(docker_config) / "config.json").read_text())
            except (OSError, ValueError) as e:
                raise Refused(f"the auth file can't be read as JSON: {type(e).__name__}") from None
            if why := cosign_login_refusal(config):
                raise Refused(why)
    print(f"PLAN | {tag} | {repo} | into {catalog_path.name}")

    # 2. the build
    broker = app_dir / "src/broker"
    env = {**_build_env(), "CARGO_ENCODED_RUSTFLAGS": remap_flags(Path.home(), root)}
    built = []
    for arch, triple in TRIPLES.items():
        _sh("cargo", "build", "--locked", "--quiet", "--release", "--target", triple,
            cwd=broker, env=env)  # fmt: skip
        out = app_dir / f"image/broker-bin/{arch}"
        out.mkdir(parents=True, exist_ok=True)
        shutil.copy2(broker / f"target/{triple}/release/{broker_name(app_dir)}", out)
        built.append(out / broker_name(app_dir))
    if leaks := local_paths(built, [str(root), str(Path.home())]):
        raise Refused(f"the broker names this machine's folders: {'; '.join(leaks)}")
    work = Path(tempfile.mkdtemp(prefix="release-", dir=os.environ.get("RUNNER_TEMP")))
    try:
        archive = work / f"{tag}.oci.tar"
        cmd = build_command(tag, archive, app_dir / "image", os.environ.get("BUILDX_BUILDER"))
        if subprocess.run(cmd, env=_build_env(), check=False).returncode != 0:
            raise Refused("the image's build failed (its output is above)")
        digests = archive_digests(archive)
        print(f"IMAGE | {tag} | list {digests['list']}")

        # 3. the registry
        if args.push:
            target = only_repository(f"{repo}:{tag}", repo)
            _sh(skopeo, "copy", "--multi-arch=all", f"oci-archive:{archive}",
                f"docker://{target}", env=_env())  # fmt: skip
            raw = _sh(skopeo, "inspect", "--raw", f"docker://{target}", env=_env())
            remote = "sha256:" + hashlib.sha256(raw.encode()).hexdigest()
            if remote != digests["list"]:
                raise Refused(f"{target} landed as {remote}, built {digests['list']}")
            print(f"PUSHED | {target} | {remote}")
            anon = subprocess.run(
                [skopeo, "inspect", "--no-creds", "--raw", f"docker://{target}"],
                capture_output=True, text=True, check=False,
            )  # fmt: skip
            if anon.returncode != 0:
                raise Refused(
                    f"{repo} isn't readable without a login: make it public, then run "
                    f"this again ({tag} isn't tagged or signed)"
                )
            _sh(cosign, "sign", "--key", KEY_REF, *LEGACY, "--recursive", "--yes",
                only_repository(f"{repo}@{remote}", repo), env=_env())  # fmt: skip
            print(f"SIGNED | {repo}@{remote} | and each arch's image")
    finally:
        shutil.rmtree(work, ignore_errors=True)

    # 4. the files
    released = git("log", "-1", "--format=%cs")
    compose = compose_text(compose, tag, repo)
    compose_out.parent.mkdir(parents=True, exist_ok=True)
    compose_out.write_text(compose)
    entry = entry_text(template, tag, digests, released, compose)
    catalog_path.write_text(channel_text(catalog, entry))
    sig = catalog_path.with_name(catalog_path.name + ".sig")
    if _key_given():
        _sh(cosign, "sign-blob", "--key", KEY_REF, *LEGACY, "--yes",
            "--output-signature", str(sig), str(catalog_path), env=_env())  # fmt: skip
        _sh(cosign, "verify-blob", "--key", str(named), "--insecure-ignore-tlog",
            "--signature", str(sig), str(catalog_path), env=_env())  # fmt: skip
    print(f"CHANNEL | {catalog_path.name} | {app_id} {tag}")
    if not args.push:
        print(f"REHEARSAL | {tag} | built, the entry filled; not pushed, not tagged")
        return 0

    # 5. the commit and the tag
    if args.commit:
        name = the_app(template)["name"]
        files = [catalog_path, sig, compose_out]
        git("add", "--", *(str(f.relative_to(root)) for f in files))
        git("commit", "-q", "-m", f"{name} {tag}: {digests['list']}")
        git("tag", "-a", tag, "-m", f"{name} {tag}: {digests['list']}")
        print(f"TAGGED | {tag} | committed; push the branch and the tag")
    return 0


def broker_name(app_dir: Path) -> str:
    """The broker binary's name, from its Cargo.toml's package."""
    cargo = tomllib.loads((app_dir / "src/broker/Cargo.toml").read_text())
    return cargo["package"]["name"]


def main(argv: list[str]) -> int:
    if hasattr(sys.stdout, "reconfigure"):  # one line per step, even through tee
        sys.stdout.reconfigure(line_buffering=True)
    p = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    p.add_argument("app", help="the app's folder, apps/<app>")
    p.add_argument("--catalog", required=True, help="the catalog file the entry goes into")
    p.add_argument("--as", dest="as_", help="an identity file to release the app as")
    p.add_argument("--tag", default="next", help="<id>-YYYY.MMDD.N, or next (the default)")
    p.add_argument("--no-push", dest="push", action="store_false", help="a rehearsal")
    p.add_argument("--commit", action="store_true", help="commit the files and tag the commit")
    return release(p.parse_args(argv))


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
