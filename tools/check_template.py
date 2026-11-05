#!/usr/bin/env python3
"""Check an app's entry template by the box's rules before a release fills it.

    python3 tools/check_template.py apps/<app>/channel-entry.toml

Fills the blanks the release command fills (the version, the day, the digests)
with placeholders, then runs `check.py` over the result. Standard library only.
"""

from __future__ import annotations

import sys
import tempfile
from pathlib import Path

import check

ZERO = "sha256:" + "0" * 64


def filled(template: str) -> str:
    lines = []
    for line in template.splitlines():
        key = line.split("=", 1)[0].strip() if "=" in line else ""
        if key == "version":
            line = 'version = "template-2026.0101.1"'
        elif key == "released":
            line = 'released = "2026-01-01"'
        elif key in ("list", "amd64", "arm64"):
            line = f'{key} = "{ZERO}"'
        lines.append(line.replace("RELEASE_TAG", "template-2026.0101.1"))
    return "\n".join(lines) + "\n"


def main(argv: list[str]) -> int:
    rc = 0
    for arg in argv:
        with tempfile.TemporaryDirectory() as d:
            out = Path(d) / "catalog.toml"
            out.write_text(filled(Path(arg).read_text()))
            f = check.check_file(out)
            print("\n".join(line.replace(str(out), arg) for line in f.lines))
            rc |= int(f.failed)
    return rc


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
