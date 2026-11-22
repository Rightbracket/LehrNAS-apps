#!/usr/bin/env python3
"""Release one app from this repository: build its image, push it, sign it,
and write its entry into a catalog file (docs/spec-runner-app.md §18.8 in
LehrNAS, #259). It's made for a workflow on a self-hosted runner, but runs
anywhere that has the tools.

    python3 tools/release.py apps/<app> --catalog official.toml --tag next
        [--as <dir>/<app>.toml] [--no-push] [--commit]

In a workflow it runs as two steps, so the step that compiles third-party
code holds no secret:

    tools/release.py apps/<app> --catalog … --build-only --out DIR    # no secrets
    tools/release.py apps/<app> --catalog … --from DIR --commit       # key, login

What it does:
1. **Refuses** before building anything:
   - a dirty tree, or a tag that isn't `<id>-YYYY.MMDD.N` or is taken, in git
     or on the registry (`next` skips both);
   - a catalog anywhere but the repository's root;
   - an image `official.toml` names, released into any other catalog, and any
     `--as` release into `official.toml`. Which file is `official.toml` is
     decided by its resolved path, never its name;
   - a key whose public half isn't the file the catalog's `[channel] key`
     names; for `official.toml`, a key whose public half isn't
     `app-channel.pub`'s, and for any other catalog, one whose public half is;
   - with `--as`, an identity that collides with the app's own: the same id,
     an id that isn't `<app id>-…`, the same port, or an image that isn't
     `docker.io/<namespace>/<id>` or is an official one;
   - with `--build-only`, a step that holds any secret.
2. **Builds** the app's static broker for both architectures, with this
   machine's paths remapped, and scans the binaries for any that are left. Then
   buildx builds both architectures into one OCI archive. No child of the
   build gets the key, its password, the login or a token.
3. **Pushes** to the entry's one repository, after asking it, logged in,
   that the tag isn't there, and checks it landed as built and reads without
   a login. Then it signs the list and each image with cosign, in the legacy
   layout podman's policy reads, with no transparency log.
4. **Writes** the compose with the tag (and, with `--as`, the identity's image
   and port, at `<dir>/<id>/compose.yaml`). Then it fills the entry, writes it
   into the catalog and signs the catalog (`<catalog>.sig`).
5. **With `--commit`,** commits those files and tags the commit. Nothing is
   pushed: the caller pushes. git runs with no secret and no hooks.

`--as <dir>/<app>.toml` releases the app under another identity: `app`, `id`,
`name`, `image` and `port`, every key and nothing else. The app's own files
are never changed.

Its inputs from the environment, all for the publish step:
- `SIGNING_KEY`: the cosign private key's text, read by cosign as
  `env://SIGNING_KEY` and never written to disk, with `COSIGN_PASSWORD`. Only
  cosign gets them.
- `REGISTRY_AUTH_FILE`: the registry login, a `config.json` that skopeo reads
  and cosign reads as `$DOCKER_CONFIG`. Only those two get it. They look a
  Docker Hub login up under different keys: skopeo's is `docker.io` (what
  `skopeo login` writes), cosign's `https://index.docker.io/v1/` (what
  `cosign login docker.io` writes). A file without cosign's is refused before
  anything is pushed, so no tag goes up that can't be signed.
- `BUILDX_BUILDER` (optional, the build step's): the builder buildx uses.

`--no-push` is a rehearsal: it builds and fills, with no registry and no tag,
and signs the catalog only when a key is given.

It prints one line per step (`PLAN |`, `IMAGE |`, `BUILT |`, `PUSHED |`, `SIGNED |`,
`CHANNEL |`, `TAGGED |`). A refusal is `REFUSED | RELEASE | <why>` and exit 2. Standard
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
    hub = re.match(r"(?:docker\.io|index\.docker\.io|registry-1\.docker\.io)(?::443)?/", name)
    if hub:
        name = name[hub.end() :]
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


def is_official(catalog: Path, root: Path) -> bool:
    """The repository root's `official.toml`, by resolved path: a file of that
    name anywhere else, or a link to it, is judged as what it is."""
    return catalog.resolve() == (root / OFFICIAL).resolve()


def catalog_refusals(catalog: Path, root: Path, identity_given: bool) -> list[str]:
    """Where a catalog may be, and what may go into `official.toml`."""
    out = []
    if catalog.resolve().parent != root.resolve():
        out.append(f"{catalog}: a catalog lives at the repository's root, beside its key")
    if identity_given and is_official(catalog, root):
        out.append(f"an identity release (--as) never goes into {OFFICIAL}")
    return out


def key_refusals(
    official: bool, catalog_name: str, derived: bytes, named: bytes, app_pub: bytes | None
) -> list[str]:
    """The signing key against the catalog: its public half must be the file
    the catalog names; the app-channel key signs `official.toml` and
    `official.toml` is signed with nothing else."""
    out = []
    if derived.strip() != named.strip():
        out.append("the signing key's public half isn't the key the catalog names")
    if app_pub is None:
        out.append(f"no {APP_CHANNEL_PUB} at the repository's root to check the key against")
    elif official and derived.strip() != app_pub.strip():
        out.append(f"{OFFICIAL} is signed with the app-channel key alone")
    elif not official and derived.strip() == app_pub.strip():
        out.append(f"the app-channel key signs {OFFICIAL} alone, never {catalog_name}")
    return out


def official_image_refusal(official: bool, repo: str, official_text: str) -> str | None:
    """An image `official.toml` names goes into `official.toml` alone."""
    if not official and repository_name(repo) in official_repositories(official_text):
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

#: What a child process never inherits unless it signs or logs in: the key, its
#: password, the registry login and any token the job holds.
SECRET_ENV = (
    "SIGNING_KEY", "COSIGN_PASSWORD", "REGISTRY_AUTH_FILE", "DOCKER_CONFIG",
    "DOCKERHUB_TOKEN", "GITHUB_TOKEN", "GH_TOKEN",
)  # fmt: skip
#: The key cosign (go-containerregistry) looks a Docker Hub login up under.
COSIGN_HUB_KEY = "https://index.docker.io/v1/"
#: The build step's record for the publish step.
BUILT = "build.json"


def scrubbed_env(env: dict[str, str] | None = None) -> dict[str, str]:
    """An environment with no secret in it: the build's, git's and skopeo's base."""
    env = dict(os.environ if env is None else env)
    return {k: v for k, v in env.items() if k not in SECRET_ENV and not k.startswith("COSIGN_")}


def secrets_held(env: dict[str, str] | None = None) -> list[str]:
    """The secret variables set in this environment, by name."""
    env = os.environ if env is None else env
    return sorted(k for k in env if env[k] and (k in SECRET_ENV or k.startswith("COSIGN_")))


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


def _git(root: Path | None, *a: str) -> str:
    """git with no secret in its environment and no hooks: the workspace and
    its `.git/hooks` outlive the job on a self-hosted runner."""
    where = ["-C", str(root)] if root else []
    return _sh("git", "-c", "core.hooksPath=/dev/null", *where, *a, env=scrubbed_env()).strip()


def _skopeo_env() -> dict[str, str]:
    """skopeo's: the login file, and nothing else secret."""
    env = scrubbed_env()
    if auth := os.environ.get("REGISTRY_AUTH_FILE"):
        env["REGISTRY_AUTH_FILE"] = auth
    return env


def _cosign_env() -> dict[str, str]:
    """cosign's: the key and its password, and the login file as
    `$DOCKER_CONFIG/config.json`."""
    env = {**os.environ, "COSIGN_YES": "true"}
    auth = Path(env.get("REGISTRY_AUTH_FILE", ""))
    if auth.name == "config.json":
        env["DOCKER_CONFIG"] = str(auth.parent)
    env.pop("REGISTRY_AUTH_FILE", None)
    return env


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


def registry_tags(skopeo: str, repo: str) -> list[str]:
    """The tags the repository shows without a login. A repository that isn't
    there yet, or is private, shows none; the publish step asks again, logged in."""
    r = subprocess.run(
        [skopeo, "list-tags", "--no-creds", f"docker://{repo}"],
        capture_output=True, text=True, env=scrubbed_env(), check=False,
    )  # fmt: skip
    if r.returncode == 0:
        return list(json.loads(r.stdout).get("Tags") or [])
    if "denied" in r.stderr or "not found" in r.stderr.lower():
        return []
    raise Refused(f"can't list {repo}'s tags: {r.stderr.strip()[-300:]}")


def tag_on_registry(skopeo: str, ref: str) -> bool:
    """Whether the tag is already there, asked with the login."""
    r = subprocess.run(
        [skopeo, "inspect", "--raw", f"docker://{ref}"],
        capture_output=True, text=True, env=_skopeo_env(), check=False,
    )  # fmt: skip
    if r.returncode == 0:
        return True
    if "manifest unknown" in r.stderr:
        return False
    raise Refused(f"can't tell whether {ref} is taken: {r.stderr.strip()[-300:]}")


def _key_given() -> bool:
    return bool(os.environ.get("SIGNING_KEY"))


@dataclass
class Plan:
    root: Path
    app_dir: Path
    catalog_path: Path
    official: bool
    template: str
    catalog: str
    compose: str
    compose_out: Path
    repo: str
    app_id: str
    tag: str
    named: Path


def plan(args: argparse.Namespace, tag: str | None = None, registry: bool = False) -> Plan:
    """The refusals that need no key and no login, and the tag: with `registry`,
    `next` also skips the tags the registry shows."""
    root = Path(_git(None, "rev-parse", "--show-toplevel"))
    if _git(root, "status", "--porcelain"):
        raise Refused(f"{root} has uncommitted changes")
    app_dir = (root / args.app).resolve()
    for need in (app_dir / "entry.toml", app_dir / "compose.yaml", root / OFFICIAL):
        if not need.is_file():
            raise Refused(f"no {need.relative_to(root)}")
    catalog_path = root / args.catalog
    if not catalog_path.is_file():
        raise Refused(f"no catalog {args.catalog}: its [channel] head comes first")
    if bad := catalog_refusals(catalog_path, root, bool(args.as_)):
        raise Refused("; ".join(bad))
    official = is_official(catalog_path, root)
    template = (app_dir / "entry.toml").read_text()
    official_text = (root / OFFICIAL).read_text()
    catalog = catalog_path.read_text()
    repo = repository(template)
    compose = (app_dir / "compose.yaml").read_text()
    compose_out = app_dir / "compose.yaml"
    if args.as_:
        ident_path = (root / args.as_).resolve()
        ident = identity(ident_path.read_text())
        if bad := identity_refusals(ident, template, official_text):
            raise Refused("; ".join(bad))
        compose_out = ident_path.parent / ident.id / "compose.yaml"
        rel = compose_out.relative_to(root).as_posix()
        compose = identity_compose(compose, ident, repo, port_pair(template))
        template = identity_template(template, ident, rel)
        repo = ident.image
    if why := official_image_refusal(official, repo, official_text):
        raise Refused(why)
    app_id = the_app(template)["id"]
    tags = _git(root, "tag", "-l").splitlines()
    if registry and not tag:
        tags += registry_tags(_tool("skopeo"), repo)
    tag = tag or args.tag
    if tag == "next":
        tag = next_tag(app_id, tags, datetime.datetime.now(datetime.UTC).date())
    if not tag_ok(tag, app_id):
        raise Refused(f"{tag}: the tag is {app_id}-YYYY.MMDD.N")
    if tag in tags:
        raise Refused(f"{tag} is already tagged")
    named = root / catalog_key(catalog)
    if not named.is_file():
        raise Refused(f"no {named.name} beside the catalog")
    return Plan(root, app_dir, catalog_path, official, template, catalog, compose,
                compose_out, repo, app_id, tag, named)  # fmt: skip


def check_key(p: Plan) -> str:
    """cosign, with a key whose public half the catalog allows."""
    cosign = _tool("cosign")
    if not _key_given():
        raise Refused("no signing key: SIGNING_KEY is empty")
    derived = _sh(cosign, "public-key", "--key", KEY_REF, env=_cosign_env()).encode()
    app_pub = p.root / APP_CHANNEL_PUB
    app_bytes = app_pub.read_bytes() if app_pub.is_file() else None
    if bad := key_refusals(p.official, p.catalog_path.name, derived, p.named.read_bytes(), app_bytes):
        raise Refused("; ".join(bad))
    return cosign


def check_login() -> str:
    """skopeo and cosign both logged in."""
    skopeo = _tool("skopeo")
    r = subprocess.run(
        [skopeo, "login", "--get-login", "docker.io"],
        capture_output=True, text=True, env=_skopeo_env(), check=False,
    )  # fmt: skip
    if r.returncode != 0 or not r.stdout.strip():
        raise Refused("no registry login for docker.io (REGISTRY_AUTH_FILE)")
    if docker_config := _cosign_env().get("DOCKER_CONFIG"):
        try:
            config = json.loads((Path(docker_config) / "config.json").read_text())
        except (OSError, ValueError) as e:
            raise Refused(f"the auth file can't be read as JSON: {type(e).__name__}") from None
        if why := cosign_login_refusal(config):
            raise Refused(why)
    return skopeo


def check_free(p: Plan, skopeo: str) -> None:
    """The tag isn't on the registry yet, asked with the login."""
    if tag_on_registry(skopeo, only_repository(f"{p.repo}:{p.tag}", p.repo)):
        raise Refused(f"{p.repo}:{p.tag} is already on the registry; the next run takes the next count")


def build(p: Plan, out: Path) -> dict[str, str]:
    """The broker and the image, into `out`, with no secret in any child's
    environment. Returns the digests and records them for the publish step."""
    print(f"PLAN | {p.tag} | {p.repo} | into {p.catalog_path.name}")
    broker = p.app_dir / "src/broker"
    env = {**scrubbed_env(), "CARGO_ENCODED_RUSTFLAGS": remap_flags(Path.home(), p.root)}
    built = []
    for arch, triple in TRIPLES.items():
        _sh("cargo", "build", "--locked", "--quiet", "--release", "--target", triple,
            cwd=broker, env=env)  # fmt: skip
        dest = p.app_dir / f"image/broker-bin/{arch}"
        dest.mkdir(parents=True, exist_ok=True)
        shutil.copy2(broker / f"target/{triple}/release/{broker_name(p.app_dir)}", dest)
        built.append(dest / broker_name(p.app_dir))
    if leaks := local_paths(built, [str(p.root), str(Path.home())]):
        raise Refused(f"the broker names this machine's folders: {'; '.join(leaks)}")
    out.mkdir(parents=True, exist_ok=True)
    archive = out / f"{p.tag}.oci.tar"
    cmd = build_command(p.tag, archive, p.app_dir / "image", os.environ.get("BUILDX_BUILDER"))
    if subprocess.run(cmd, env=scrubbed_env(), check=False).returncode != 0:
        raise Refused("the image's build failed (its output is above)")
    digests = archive_digests(archive)
    print(f"IMAGE | {p.tag} | list {digests['list']}")
    record = {"tag": p.tag, "head": _git(p.root, "rev-parse", "HEAD"),
              "catalog": p.catalog_path.name, "digests": digests}  # fmt: skip
    (out / BUILT).write_text(json.dumps(record, indent=1) + "\n")
    return digests


def built_record(out: Path) -> dict:
    """What the build step left, checked against this checkout and its archive."""
    try:
        record = json.loads((out / BUILT).read_text())
    except (OSError, ValueError):
        raise Refused(f"no build in {out}: run the build step (--build-only --out) first") from None
    head = _git(None, "rev-parse", "HEAD")
    if record.get("head") != head:
        raise Refused(f"the build is of {record.get('head')}, this checkout is {head}")
    archive = out / f"{record['tag']}.oci.tar"
    if not archive.is_file():
        raise Refused(f"no image archive for {record['tag']} in {out}")
    if archive_digests(archive) != record.get("digests"):
        raise Refused(f"{archive.name} isn't the image the build recorded")
    return record


def publish(args: argparse.Namespace, p: Plan, archive: Path, digests: dict[str, str],
            cosign: str, skopeo: str) -> int:  # fmt: skip
    """Push, sign, write the entry and the catalog, and commit."""
    if args.push:
        target = only_repository(f"{p.repo}:{p.tag}", p.repo)
        _sh(skopeo, "copy", "--multi-arch=all", f"oci-archive:{archive}",
            f"docker://{target}", env=_skopeo_env())  # fmt: skip
        raw = _sh(skopeo, "inspect", "--raw", f"docker://{target}", env=_skopeo_env())
        remote = "sha256:" + hashlib.sha256(raw.encode()).hexdigest()
        if remote != digests["list"]:
            raise Refused(f"{target} landed as {remote}, built {digests['list']}")
        print(f"PUSHED | {target} | {remote}")
        anon = subprocess.run(
            [skopeo, "inspect", "--no-creds", "--raw", f"docker://{target}"],
            capture_output=True, text=True, env=scrubbed_env(), check=False,
        )  # fmt: skip
        if anon.returncode != 0:
            raise Refused(
                f"{p.repo} isn't readable without a login: make it public; the next run "
                f"takes the next count ({p.tag} is pushed, not signed, in no catalog)"
            )
        _sh(cosign, "sign", "--key", KEY_REF, *LEGACY, "--recursive", "--yes",
            only_repository(f"{p.repo}@{remote}", p.repo), env=_cosign_env())  # fmt: skip
        print(f"SIGNED | {p.repo}@{remote} | and each arch's image")

    released = _git(p.root, "log", "-1", "--format=%cs")
    compose = compose_text(p.compose, p.tag, p.repo)
    p.compose_out.parent.mkdir(parents=True, exist_ok=True)
    p.compose_out.write_text(compose)
    entry = entry_text(p.template, p.tag, digests, released, compose)
    p.catalog_path.write_text(channel_text(p.catalog, entry))
    sig = p.catalog_path.with_name(p.catalog_path.name + ".sig")
    if _key_given():
        _sh(cosign, "sign-blob", "--key", KEY_REF, *LEGACY, "--yes",
            "--output-signature", str(sig), str(p.catalog_path), env=_cosign_env())  # fmt: skip
        _sh(cosign, "verify-blob", "--key", str(p.named), "--insecure-ignore-tlog",
            "--signature", str(sig), str(p.catalog_path), env=scrubbed_env())  # fmt: skip
    print(f"CHANNEL | {p.catalog_path.name} | {p.app_id} {p.tag}")
    if not args.push:
        print(f"REHEARSAL | {p.tag} | built, the entry filled; not pushed, not tagged")
        return 0
    if args.commit:
        name = the_app(p.template)["name"]
        files = [p.catalog_path, sig, p.compose_out]
        _git(p.root, "add", "--", *(str(f.relative_to(p.root)) for f in files))
        _git(p.root, "commit", "-q", "-m", f"{name} {p.tag}: {digests['list']}")
        _git(p.root, "tag", "-a", p.tag, "-m", f"{name} {p.tag}: {digests['list']}")
        print(f"TAGGED | {p.tag} | committed; push the branch and the tag")
    return 0


def release(args: argparse.Namespace) -> int:
    try:
        return _release(args)
    except Refused as e:
        print(f"REFUSED | RELEASE | {e}")
        return 2


def _release(args: argparse.Namespace) -> int:
    if args.build_only:
        # The step that compiles third-party code holds no secret at all.
        if held := secrets_held():
            raise Refused(f"the build step holds secrets ({', '.join(held)}); give it none")
        p = plan(args, registry=args.push)
        build(p, Path(args.out))
        print(f"BUILT | {p.tag} | in {args.out}; publish with --from")
        return 0
    if args.from_:
        out = Path(args.from_)
        record = built_record(out)
        p = plan(args, tag=record["tag"])
        if record.get("catalog") != p.catalog_path.name:
            raise Refused(f"the build was for {record.get('catalog')}, not {p.catalog_path.name}")
        cosign = check_key(p) if (args.push or _key_given()) else ""
        skopeo = ""
        if args.push:
            skopeo = check_login()
            check_free(p, skopeo)
        print(f"PLAN | {p.tag} | {p.repo} | into {p.catalog_path.name}")
        return publish(args, p, out / f"{p.tag}.oci.tar", record["digests"], cosign, skopeo)
    # Both in one go: a rehearsal, or a release by hand.
    p = plan(args)
    cosign = check_key(p) if (args.push or _key_given()) else ""
    skopeo = ""
    if args.push:
        skopeo = check_login()
        p = plan(args, registry=True)
        check_free(p, skopeo)
    out = Path(tempfile.mkdtemp(prefix="release-", dir=os.environ.get("RUNNER_TEMP")))
    try:
        digests = build(p, out)
        return publish(args, p, out / f"{p.tag}.oci.tar", digests, cosign, skopeo)
    finally:
        shutil.rmtree(out, ignore_errors=True)


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
    step = p.add_mutually_exclusive_group()
    step.add_argument("--build-only", action="store_true",
                      help="build into --out, with no secret held; publish later with --from")  # fmt: skip
    step.add_argument("--from", dest="from_", metavar="DIR", help="publish the build in DIR")
    p.add_argument("--out", metavar="DIR", help="with --build-only: where the build goes")
    args = p.parse_args(argv)
    if args.build_only and not args.out:
        p.error("--build-only needs --out")
    return release(args)


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
