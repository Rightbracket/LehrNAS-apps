#!/usr/bin/env python3
"""Check every app's entry by the box's rules before a release fills it.

    python3 tools/check_template.py [apps/<id>/entry.toml ...]

Run from the repository's root. With no arguments, every `apps/*/entry.toml`.
Fills the blanks the release command fills (the version, the day, the digests,
the compose file's hash) with placeholders, then runs `check.py` over the result,
and checks that the folder is named by the entry's id. Standard library only.
"""

from __future__ import annotations

import hashlib
import shutil
import sys
import tempfile
import tomllib
from pathlib import Path

import check

ZERO = "sha256:" + "0" * 64


def filled(template: str, compose_sha: str = "") -> str:
    lines = []
    for line in template.splitlines():
        key = line.split("=", 1)[0].strip() if "=" in line else ""
        if key == "version":
            line = 'version = "template-2026.0101.1"'
        elif key == "released":
            line = 'released = "2026-01-01"'
        elif key in ("list", "amd64", "arm64"):
            line = f'{key} = "{ZERO}"'
        elif key == "compose_sha256":
            line = f'compose_sha256 = "{compose_sha}"'
        lines.append(line)
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    paths = argv or [str(p) for p in sorted(Path("apps").glob("*/entry.toml"))]
    if not paths:
        print("FAIL | apps/*/entry.toml | no entries found; run this from the repository's root")
        return 1
    rc = 0
    for arg in paths:
        apps = tomllib.loads(Path(arg).read_text())["apps"]
        folder = Path(arg).resolve().parent.name
        if len(apps) != 1 or apps[0].get("id") != folder:
            print(f"FAIL | {arg} | one entry, whose id is its folder's name ({folder})")
            rc = 1
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "catalog.toml"
            text = Path(arg).read_text()
            # A template's compose_path is from the repository's root, as the catalog's is.
            rel = tomllib.loads(text)["apps"][0].get("compose_path")
            sha = ""
            if rel:
                sha = hashlib.sha256(Path(rel).read_bytes()).hexdigest()
                (Path(d) / rel).parent.mkdir(parents=True)
                shutil.copy(rel, Path(d) / rel)
            out.write_text(filled(text, sha))
            f = check.check_file(out)
            print("\n".join(line.replace(str(out), arg) for line in f.lines))
            rc |= int(f.failed)
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
