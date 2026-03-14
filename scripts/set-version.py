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
    update_cargo_toml(repo_root / "Cargo.toml", version)
    update_cargo_lock(repo_root / "Cargo.lock", version)

    print(f"Updated crate version to {version}")

    return 0


def update_cargo_toml(path: pathlib.Path, version: str) -> None:
    contents = path.read_text()
    match = re.search(
        r'(\[package\]\s+name = "cfmpeg"\s+version = ")([^"]+)(")',
        contents,
        flags=re.MULTILINE,
    )

    if match is None:
        raise SystemExit("failed to find package version in Cargo.toml")

    updated = contents[: match.start()] + match.group(1) + version + match.group(3) + contents[match.end() :]
    path.write_text(updated)


def update_cargo_lock(path: pathlib.Path, version: str) -> None:
    contents = path.read_text()
    match = re.search(
        r'(\[\[package\]\]\s+name = "cfmpeg"\s+version = ")([^"]+)(")',
        contents,
        flags=re.MULTILINE,
    )

    if match is None:
        raise SystemExit("failed to find root package version in Cargo.lock")

    updated = contents[: match.start()] + match.group(1) + version + match.group(3) + contents[match.end() :]
    path.write_text(updated)


if __name__ == "__main__":
    raise SystemExit(main())
