#!/usr/bin/env python3
"""Check a LehrNAS app catalog before a box has to.

    python3 tools/check.py official.toml [other.toml ...]

Reads each catalog file the way a LehrNAS box does (the same rules, so a
file that passes here is a file a box accepts), prints one line per
finding, `OK | <file> | <n> apps` when there is none, and exits 1 when any
file fails. Standard library only (Python 3.11 or newer for `tomllib`).

When `cosign` is on the PATH and the catalog has a signature beside it,
the signature is verified with the key the catalog names. When PyYAML is
installed, a compose file's services are checked against the entry's
image rows; without it the services are read with a simple line parser.
"""

from __future__ import annotations

import hashlib
import re
import shutil
import subprocess
import sys
import tomllib
from datetime import date
from pathlib import Path

ID_RE = re.compile(r"^[a-z0-9][a-z0-9-]{0,31}$")
SHA_RE = re.compile(r"^sha256:[0-9a-f]{64}$")
HEX_RE = re.compile(r"^[0-9a-f]{64}$")
# A published port as the box reads it for the one-image shorthand: the LAN
# port and the container's, HOST:CONTAINER[/tcp|udp]; a compose entry's rows
# are the author's words and may also be the short `<port>/tcp`.
PORT_RE = re.compile(r"^(\d{1,5}):(\d{1,5})(/(tcp|udp))?$")
PORT_SHORT_RE = re.compile(r"^(\d{1,5})(/(tcp|udp))?$")
COMPOSE_MAX = 64 * 1024
USER_KINDS = ("default app user", "dedicated")
MODES = ("read-write", "read-only")


class Findings:
    def __init__(self, file: Path) -> None:
        self.file = file
        self.lines: list[str] = []

    def fail(self, where: str, what: str) -> None:
        self.lines.append(f"FAIL | {self.file} | {where} | {what}")

    def note(self, where: str, what: str) -> None:
        self.lines.append(f"NOTE | {self.file} | {where} | {what}")

    @property
    def failed(self) -> bool:
        return any(line.startswith("FAIL") for line in self.lines)


def _str(f: Findings, where: str, table: dict, key: str, required: bool = True) -> str | None:
    v = table.get(key)
    if v is None:
        if required:
            f.fail(where, f"no {key}")
        return None
    if not isinstance(v, str):
        f.fail(where, f"{key}: a string")
        return None
    if required and not v.strip():
        f.fail(where, f"{key}: empty")
        return None
    return v


def image_ref(r: object) -> str | None:
    """The box's rule for an image reference: non-empty, at most 255, no
    whitespace; a digest suffix, when given, is sha256:<64 hex>. A tag is
    allowed: the digests are what the box pulls."""
    if not isinstance(r, str) or not r or len(r) > 255 or any(c.isspace() for c in r):
        return "isn't an image reference"
    if "@" in r:
        d = r.split("@", 1)[1]
        if not SHA_RE.match(d):
            return "a digest is sha256:<64 hex>"
    return None


def _digest(f: Findings, where: str, v: object, required: bool) -> None:
    if v is None:
        if required:
            f.fail(where, "no digest")
        return
    if not isinstance(v, str) or not SHA_RE.match(v):
        f.fail(where, "a digest is sha256: and 64 hex digits")


def compose_services(text: str) -> list[str] | None:
    """The service names of a compose file: PyYAML when it is there, else
    the two-space keys under a top-level `services:` line."""
    try:
        import yaml  # type: ignore[import-not-found]

        doc = yaml.safe_load(text)
        if not isinstance(doc, dict) or not isinstance(doc.get("services"), dict):
            return None
        return [str(k) for k in doc["services"]]
    except ImportError:
        pass
    names: list[str] = []
    inside = False
    for line in text.splitlines():
        if re.match(r"^services:\s*(#.*)?$", line):
            inside = True
            continue
        if inside and re.match(r"^\S", line):
            break
        m = re.match(r"^  ([A-Za-z0-9_.-]+):\s*(#.*)?$", line) if inside else None
        if m:
            names.append(m.group(1))
    return names or None


def check_entry(f: Findings, base: Path, e: dict, seen: set[str]) -> None:
    app_id = e.get("id")
    where = f"apps[{app_id!r}]" if isinstance(app_id, str) else "apps[?]"
    if not isinstance(app_id, str) or not ID_RE.match(app_id):
        f.fail(where, "id: lower-case letters, digits and dashes, at most 32")
    elif app_id in seen:
        f.fail(where, "id: twice in the catalog")
    else:
        seen.add(app_id)
    _str(f, where, e, "name")
    _str(f, where, e, "summary")
    _str(f, where, e, "group", required=False)
    _str(f, where, e, "version")
    released = _str(f, where, e, "released")
    if released:
        try:
            date.fromisoformat(released)
        except ValueError:
            f.fail(where, "released: a day as YYYY-MM-DD")

    # One of three: image (the one-image shorthand), compose inline, or
    # compose_path with its hash.
    forms = [k for k in ("image", "compose", "compose_path") if e.get(k)]
    if len(forms) != 1:
        f.fail(where, "one of image, compose or compose_path, exactly one")
        return
    (form,) = forms
    if form == "image":
        if (why := image_ref(e["image"])) is not None:
            f.fail(where, f"image: {why}")
        d = e.get("digests")
        if not isinstance(d, dict):
            f.fail(where, "image needs [apps.digests] with the manifest list's digest")
        else:
            _digest(f, f"{where}.digests.list", d.get("list"), required=True)
            for arch in ("amd64", "arm64"):
                _digest(f, f"{where}.digests.{arch}", d.get(arch), required=False)
        if e.get("images"):
            f.fail(where, "images rows belong to a compose entry; image has [apps.digests]")
        return

    if form == "compose":
        text = e["compose"]
        if not isinstance(text, str):
            f.fail(where, "compose: the file as a string")
            return
        if len(text.encode()) > COMPOSE_MAX:
            f.fail(where, "compose: at most 64 KiB")
    else:
        rel = e["compose_path"]
        if not isinstance(rel, str) or rel.startswith(("/", "..")) or "/../" in rel:
            f.fail(where, "compose_path: a relative path beside the catalog file")
            return
        path = base / rel
        if not path.is_file():
            f.fail(where, f"compose_path: no file at {rel}")
            return
        data = path.read_bytes()
        if len(data) > COMPOSE_MAX:
            f.fail(where, "compose_path: the file is over 64 KiB")
        want = e.get("compose_sha256")
        if not isinstance(want, str) or not HEX_RE.match(want):
            f.fail(where, "compose_path needs compose_sha256, 64 hex")
        elif hashlib.sha256(data).hexdigest() != want:
            f.fail(where, f"compose_sha256 doesn't match {rel}")
        text = data.decode(errors="replace")

    rows = e.get("images")
    if not isinstance(rows, list) or not rows:
        f.fail(where, "a compose entry needs one [[apps.images]] row per service")
        return
    by_service: dict[str, dict] = {}
    for r in rows:
        svc = r.get("service") if isinstance(r, dict) else None
        if not isinstance(svc, str) or not svc:
            f.fail(where, "images: a row without a service")
            continue
        by_service[svc] = r
        img = r.get("image")
        if (why := image_ref(img)) is not None:
            f.fail(f"{where}.images.{svc}", f"image: {why}")
        _digest(f, f"{where}.images.{svc}.list", r.get("list"), required=True)
        for arch in ("amd64", "arm64"):
            _digest(f, f"{where}.images.{svc}.{arch}", r.get(arch), required=False)
    services = compose_services(text)
    if services is None:
        f.note(where, "compose: no services found to compare with the image rows")
    else:
        for svc in services:
            if svc not in by_service:
                f.fail(where, f"services.{svc}: no pinned digest in the catalog entry")
        for svc in by_service:
            if svc not in services:
                f.fail(where, f"images.{svc}: no such service in the compose file")


def port_ok(p: object, shorthand: bool) -> bool:
    if not isinstance(p, str):
        return False
    m = PORT_RE.match(p)
    if m:
        return all(0 < int(x) < 65536 for x in (m.group(1), m.group(2)))
    m = PORT_SHORT_RE.match(p)
    return bool(m) and not shorthand and 0 < int(m.group(1)) < 65536


def check_rest(f: Findings, e: dict) -> None:
    where = f"apps[{e.get('id')!r}]"
    shorthand = bool(e.get("image"))
    req = e.get("requirements", {})
    if not isinstance(req, dict):
        f.fail(where, "requirements: a table")
    else:
        for k in ("mem_mib", "cpus"):
            v = req.get(k)
            if v is not None and (not isinstance(v, int) or v <= 0):
                f.fail(where, f"requirements.{k}: a positive whole number")
        if req.get("gpu") is not None and not isinstance(req["gpu"], str):
            f.fail(where, "requirements.gpu: a word (any, nvidia, intel…)")
    perm = e.get("permissions", {})
    if not isinstance(perm, dict):
        f.fail(where, "permissions: a table")
    else:
        for p in perm.get("ports", []):
            if not port_ok(p, shorthand):
                form = "HOST:CONTAINER[/tcp|udp]" if shorthand else "HOST:CONTAINER or <port>/tcp"
                f.fail(where, f"permissions.ports: {p!r} is not {form}")
        for k in ("internet", "lan", "host_network"):
            if perm.get(k) is not None and not isinstance(perm[k], bool):
                f.fail(where, f"permissions.{k}: true or false")
        for k in ("devices", "capabilities"):
            v = perm.get(k, [])
            if not isinstance(v, list) or not all(isinstance(x, str) for x in v):
                f.fail(where, f"permissions.{k}: a list of strings")
        if perm.get("shares") is not None and not isinstance(perm["shares"], str):
            f.fail(where, "permissions.shares: a sentence")
    limits = e.get("limits", {})
    if not isinstance(limits, dict):
        f.fail(where, "limits: a table")
    elif limits.get("cpu_weight") is not None and (
        not isinstance(limits["cpu_weight"], int) or not 1 <= limits["cpu_weight"] <= 100
    ):
        f.fail(where, "limits.cpu_weight: a whole number from 1 to 100")
    names: set[str] = set()
    for d in e.get("data", []):
        if not isinstance(d, dict) or not isinstance(d.get("name"), str) or not d["name"]:
            f.fail(where, "data: a location without a name")
            continue
        if d["name"] in names:
            f.fail(where, f"data.{d['name']}: twice")
        names.add(d["name"])
        b = d.get("bytes")
        if b is not None and (not isinstance(b, int) or b < 0):
            f.fail(where, f"data.{d['name']}.bytes: a whole number")
        if d.get("mode") is not None and d["mode"] not in MODES:
            f.fail(where, f"data.{d['name']}.mode: read-write or read-only")
    user = e.get("user", {})
    if not isinstance(user, dict):
        f.fail(where, "user: a table")
    elif user.get("kind") is not None and not isinstance(user["kind"], str):
        f.fail(where, "user.kind: a string")
    elif user.get("kind") is not None and user["kind"] not in USER_KINDS:
        f.note(where, f"user.kind: {user['kind']!r} is not one the wizard knows ({', '.join(USER_KINDS)})")
    for s in e.get("seed", []):
        if not isinstance(s, dict):
            f.fail(where, "seed: a table per file")
            continue
        for k in ("location", "path", "text"):
            if not isinstance(s.get(k), str):
                f.fail(where, f"seed: {k} missing")
        if isinstance(s.get("location"), str) and s["location"] not in names:
            f.fail(where, f"seed: location {s['location']!r} is not a data location")
    if e.get("note") is not None and not isinstance(e["note"], str):
        f.fail(where, "note: a string")


def verify_signature(f: Findings, file: Path, key_name: str | None) -> None:
    sig = file.with_name(file.name + ".sig")
    if key_name is None:
        if sig.exists():
            f.note("signature", f"{sig.name} is beside the file but [channel] names no key")
        return
    key = file.with_name(key_name)
    if not key.is_file():
        f.fail("channel.key", f"no key file at {key.name} beside the catalog")
        return
    if not sig.is_file():
        f.fail("signature", f"[channel] names a key, so {sig.name} must be beside the file")
        return
    cosign = shutil.which("cosign")
    if cosign is None:
        f.note("signature", "cosign is not on the PATH; the signature was not verified")
        return
    r = subprocess.run(
        [cosign, "verify-blob", "--key", str(key), "--signature", str(sig),
         "--insecure-ignore-tlog", str(file)],
        capture_output=True, text=True, check=False,
    )  # fmt: skip
    if r.returncode != 0:
        f.fail("signature", f"doesn't verify with {key.name}: {r.stderr.strip().splitlines()[-1:]}")


def check_file(file: Path) -> Findings:
    f = Findings(file)
    try:
        doc = tomllib.loads(file.read_text(encoding="utf-8"))
    except (OSError, UnicodeDecodeError, tomllib.TOMLDecodeError) as e:
        f.fail("file", f"not a TOML catalog: {e}")
        return f
    ch = doc.get("channel")
    key_name = None
    if ch is None:
        f.note("channel", "no [channel] table: the box shows the file's URL as the name")
    elif not isinstance(ch, dict):
        f.fail("channel", "a table")
    else:
        _str(f, "channel", ch, "name")
        url = _str(f, "channel", ch, "url", required=False)
        if url and not url.startswith("https://"):
            f.fail("channel.url", "https only")
        key_name = ch.get("key")
        if key_name is not None and (not isinstance(key_name, str) or "/" in key_name):
            f.fail("channel.key", "the key file's name, beside the catalog")
            key_name = None
    apps = doc.get("apps", [])
    if not isinstance(apps, list):
        f.fail("apps", "[[apps]] tables")
        apps = []
    seen: set[str] = set()
    for e in apps:
        if not isinstance(e, dict):
            f.fail("apps", "an entry that is not a table")
            continue
        check_entry(f, file.parent, e, seen)
        check_rest(f, e)
    verify_signature(f, file, key_name)
    if not f.failed:
        f.lines.append(f"OK | {file} | {len(apps)} apps")
    return f


def main(argv: list[str]) -> int:
    if not argv or argv[0] in ("-h", "--help"):
        print(__doc__.strip())
        return 0 if argv else 2
    rc = 0
    for arg in argv:
        f = check_file(Path(arg))
        print("\n".join(f.lines))
        rc |= int(f.failed)
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
