#!/usr/bin/env python3
"""Fill <bundle>/licenses with the notices a release archive has to carry.

    assemble-licenses.py --bundle DIR --kind KIND --rust-target TRIPLE \
        --ffmpeg-licenses DIR

KIND says how the binaries in the bundle were linked, which decides the extra
notices that go in:

    dynamic   macOS and glibc Linux; the C library is a shared library
    musl      static Linux; musl and GCC runtime code are linked in
    windows   static CRT; the helper carries mingw-w64 and GCC runtime code

The result is:

    licenses/README.txt        what each directory is for
    licenses/cfmpeg/           cfmpeg's own licence
    licenses/ffmpeg/           copied from --ffmpeg-licenses, which
                               build-helper-ffmpeg.sh writes
    licenses/rust/CRATES.txt   every crate linked into cfmpeg for TRIPLE, with
                               the licence files its published source carries
    licenses/rust/COPYRIGHT-library.html
                               the Rust standard library's notices, when the
                               toolchain ships them
    licenses/musl/ ...         the vendored texts from packaging/licenses that
                               apply to KIND

Run it after `cargo build`, so the crate sources are already downloaded.
"""

from __future__ import annotations

import argparse
import json
import os
import pathlib
import re
import shutil
import subprocess
import sys
from typing import NoReturn

REPO_ROOT = pathlib.Path(__file__).resolve().parents[1]
VENDORED = REPO_ROOT / "packaging" / "licenses"

# Vendored directories that apply to each kind of build.
VENDORED_BY_KIND = {
    "dynamic": [],
    "musl": ["musl", "gcc-runtime"],
    "windows": ["mingw-w64", "gcc-runtime"],
}

FFMPEG_REQUIRED = ["COPYING.LGPLv2.1", "LICENSE.md", "NOTICE.txt"]

LICENSE_FILE = re.compile(r"^(licen[sc]e|copying|copyright|notice|unlicense|patents)", re.IGNORECASE)
LICENSE_DIRS = {"licenses", "license"}

RULE = "=" * 78


def fail(message: str) -> NoReturn:
    raise SystemExit(f"assemble-licenses: {message}")


def write_text(path: pathlib.Path, text: str) -> None:
    # Unix line endings on every platform, Windows runners included.
    with open(path, "w", encoding="utf-8", newline="\n") as handle:
        handle.write(text)


def cargo() -> str:
    return os.environ.get("CARGO", "cargo")


def linked_crates(rust_target: str) -> list[dict]:
    """Packages that end up in the cfmpeg binary built for rust_target."""
    output = subprocess.run(
        [
            cargo(),
            "metadata",
            "--format-version",
            "1",
            "--locked",
            "--filter-platform",
            rust_target,
            "--manifest-path",
            str(REPO_ROOT / "Cargo.toml"),
        ],
        check=True,
        stdout=subprocess.PIPE,
    ).stdout
    metadata = json.loads(output)

    packages = {package["id"]: package for package in metadata["packages"]}
    nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
    root = metadata["resolve"]["root"]
    if root is None:
        fail("cargo metadata reported no root package")

    def is_proc_macro(package: dict) -> bool:
        return any("proc-macro" in target["kind"] for target in package["targets"])

    # Follow normal dependencies only. Build scripts and proc macros run at
    # compile time; neither they nor their own dependencies are linked in.
    seen = {root}
    queue = [root]
    while queue:
        for dependency in nodes[queue.pop()]["deps"]:
            dependency_id = dependency["pkg"]
            if dependency_id in seen:
                continue
            if not any(kind["kind"] is None for kind in dependency["dep_kinds"]):
                continue
            if is_proc_macro(packages[dependency_id]):
                continue
            seen.add(dependency_id)
            queue.append(dependency_id)

    seen.discard(root)
    return sorted((packages[package_id] for package_id in seen), key=lambda p: (p["name"], p["version"]))


def license_files(crate_dir: pathlib.Path) -> list[pathlib.Path]:
    found = []
    for entry in sorted(crate_dir.iterdir(), key=lambda path: path.name.lower()):
        if entry.is_file() and LICENSE_FILE.match(entry.name):
            found.append(entry)
        elif entry.is_dir() and entry.name.lower() in LICENSE_DIRS:
            found.extend(sorted(path for path in entry.rglob("*") if path.is_file()))
    return found


def write_crate_notices(rust_target: str, destination: pathlib.Path) -> tuple[int, list[str]]:
    crates = linked_crates(rust_target)
    if not crates:
        fail(f"cargo metadata listed no dependencies for {rust_target}")

    without_files = []
    sections = []
    for crate in crates:
        label = f"{crate['name']} {crate['version']}"
        crate_dir = pathlib.Path(crate["manifest_path"]).parent
        files = license_files(crate_dir)
        source = crate.get("repository") or f"https://crates.io/crates/{crate['name']}/{crate['version']}"

        lines = [
            RULE,
            label,
            f"Licence: {crate.get('license') or 'see the files below'}",
            f"Source:  {source}",
            RULE,
            "",
        ]
        if not files:
            without_files.append(label)
            lines += [
                "The published crate contains no licence file. Its licence is the one",
                "named above; the source link has the authors.",
                "",
            ]
        for path in files:
            lines += [
                f"--- {path.relative_to(crate_dir).as_posix()} ---",
                "",
                path.read_text(encoding="utf-8", errors="replace").rstrip(),
                "",
            ]
        sections.append("\n".join(lines))

    header = [
        "Rust crates linked into cfmpeg",
        "==============================",
        "",
        f"The cfmpeg binary in this archive was built for {rust_target}.",
        f"It contains code from the {len(crates)} crates below. Each entry gives the crate's",
        "licence as its authors declare it, where its source lives, and every",
        "licence file its published source carries, copied verbatim.",
        "",
        "Crates that only run while compiling (build scripts and procedural macros)",
        "are not part of the binary and are not listed.",
        "",
        "",
    ]
    write_text(destination, "\n".join(header) + "\n".join(sections))
    return len(crates), without_files


def copy_standard_library_notices(destination: pathlib.Path) -> bool:
    """The toolchain ships the notices for std and what std itself bundles."""
    try:
        sysroot = subprocess.run(
            [os.environ.get("RUSTC", "rustc"), "--print", "sysroot"],
            check=True,
            stdout=subprocess.PIPE,
            text=True,
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return False

    notices = pathlib.Path(sysroot) / "share" / "doc" / "rust" / "COPYRIGHT-library.html"
    if not notices.is_file():
        return False

    shutil.copyfile(notices, destination / notices.name)
    return True


def write_readme(destination: pathlib.Path, kind: str, crate_count: int, has_std_notices: bool) -> None:
    lines = [
        "Licences for the software in this archive",
        "=========================================",
        "",
        "cfmpeg/",
        "    The licence of cfmpeg itself (MIT).",
        "",
        "rust/",
        f"    CRATES.txt lists the {crate_count} Rust crates compiled into cfmpeg, with",
        "    their licence texts.",
    ]
    if has_std_notices:
        lines += [
            "    COPYRIGHT-library.html covers the Rust standard library, which is",
            "    also compiled in, and the code the standard library bundles.",
        ]
    else:
        lines += [
            "    The Rust standard library is also compiled in. It is licensed under",
            "    MIT or Apache-2.0 at your option; see https://www.rust-lang.org/policies/licenses.",
        ]
    lines += [
        "",
        "ffmpeg/",
        "    The bundled helper is FFmpeg, under the GNU Lesser General Public",
        "    License version 2.1 or later. NOTICE.txt says which release it is,",
        "    where its source is, and how it was configured.",
    ]
    if kind == "musl":
        lines += [
            "",
            "musl/",
            "    Both binaries are statically linked against the musl C library.",
            "",
            "gcc-runtime/",
            "    Statically linked binaries include support routines from GCC's",
            "    runtime library (libgcc), which is under the GPL version 3 with the",
            "    GCC Runtime Library Exception (COPYING.RUNTIME).",
        ]
    elif kind == "windows":
        lines += [
            "",
            "mingw-w64/",
            "    ffmpeg.exe is statically linked against the mingw-w64 runtime, and",
            "    depending on the toolchain its winpthreads library.",
            "",
            "gcc-runtime/",
            "    ffmpeg.exe includes support routines from GCC's runtime library",
            "    (libgcc), which is under the GPL version 3 with the GCC Runtime",
            "    Library Exception (COPYING.RUNTIME).",
        ]
    lines.append("")
    write_text(destination / "README.txt", "\n".join(lines))


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bundle", required=True, type=pathlib.Path)
    parser.add_argument("--kind", required=True, choices=sorted(VENDORED_BY_KIND))
    parser.add_argument("--rust-target", required=True)
    parser.add_argument("--ffmpeg-licenses", required=True, type=pathlib.Path)
    args = parser.parse_args()

    for name in FFMPEG_REQUIRED:
        if not (args.ffmpeg_licenses / name).is_file():
            fail(f"{args.ffmpeg_licenses / name} is missing; build the helper with scripts/build-helper-ffmpeg.sh")

    destination = args.bundle / "licenses"
    if destination.exists():
        shutil.rmtree(destination)
    (destination / "cfmpeg").mkdir(parents=True)
    (destination / "rust").mkdir()

    shutil.copyfile(REPO_ROOT / "LICENSE", destination / "cfmpeg" / "LICENSE")
    shutil.copytree(args.ffmpeg_licenses, destination / "ffmpeg")
    for name in VENDORED_BY_KIND[args.kind]:
        shutil.copytree(VENDORED / name, destination / name)

    crate_count, without_files = write_crate_notices(args.rust_target, destination / "rust" / "CRATES.txt")
    has_std_notices = copy_standard_library_notices(destination / "rust")
    write_readme(destination, args.kind, crate_count, has_std_notices)

    print(f"Wrote {destination} ({args.kind}, {crate_count} crates for {args.rust_target}).")
    if without_files:
        print(f"Crates whose published source has no licence file: {', '.join(without_files)}")
    if not has_std_notices:
        print("::warning::The Rust toolchain has no share/doc/rust/COPYRIGHT-library.html; README.txt links to the Rust licence page instead.")

    return 0


if __name__ == "__main__":
    sys.exit(main())
