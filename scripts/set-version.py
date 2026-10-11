#!/usr/bin/env python3

from __future__ import annotations

import pathlib
import re
import sys


def main() -> int:
    if len(sys.argv) != 2:
        raise SystemExit("usage: scripts/set-version.py X.Y.Z")

    version = sys.argv[1]
    if not re.fullmatch(r"\d+\.\d+\.\d+", version):
        raise SystemExit(f"invalid version: {version}")

    repo_root = pathlib.Path(__file__).resolve().parents[1]
    update_package_version(repo_root / "Cargo.toml", "[package]", version)
    update_package_version(repo_root / "Cargo.lock", "[[package]]", version)

    print(f"Updated crate version to {version}")

    return 0


def update_package_version(path: pathlib.Path, header: str, version: str) -> None:
    contents = path.read_text()
    match = re.search(
        "(" + re.escape(header) + r'\s+name = "cfmpeg"\s+version = ")([^"]+)(")',
        contents,
        flags=re.MULTILINE,
    )

    if match is None:
        raise SystemExit(f"failed to find cfmpeg package version in {path.name}")

    updated = contents[: match.start()] + match.group(1) + version + match.group(3) + contents[match.end() :]
    path.write_text(updated)


if __name__ == "__main__":
    raise SystemExit(main())
