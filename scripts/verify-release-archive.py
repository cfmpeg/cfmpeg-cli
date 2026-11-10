#!/usr/bin/env python3
"""Check that a release archive holds the binaries and every licence notice.

    verify-release-archive.py ARCHIVE --kind KIND

ARCHIVE is a .tar.gz or a .zip. KIND is the one assemble-licenses.py was given
for it: dynamic, musl, or windows. The check fails when a required file is
missing or empty, or when the FFmpeg notice does not name its source.
"""

from __future__ import annotations

import argparse
import sys
import tarfile
import zipfile

COMMON = [
    "licenses/README.txt",
    "licenses/cfmpeg/LICENSE",
    "licenses/ffmpeg/COPYING.LGPLv2.1",
    "licenses/ffmpeg/LICENSE.md",
    "licenses/ffmpeg/NOTICE.txt",
    "licenses/rust/CRATES.txt",
]

GCC_RUNTIME = [
    "licenses/gcc-runtime/COPYING.RUNTIME",
    "licenses/gcc-runtime/COPYING3",
]

REQUIRED = {
    "dynamic": ["bin/cfmpeg", "libexec/ffmpeg"] + COMMON,
    "musl": ["bin/cfmpeg", "libexec/ffmpeg"] + COMMON + ["licenses/musl/COPYRIGHT"] + GCC_RUNTIME,
    "windows": ["bin/cfmpeg.exe", "libexec/ffmpeg.exe"]
    + COMMON
    + [
        "licenses/mingw-w64/COPYING",
        "licenses/mingw-w64/COPYING.MinGW-w64-runtime.txt",
        "licenses/mingw-w64/COPYING.winpthreads",
        "licenses/mingw-w64/DISCLAIMER",
        "licenses/mingw-w64/DISCLAIMER.PD",
    ]
    + GCC_RUNTIME,
}

NOTICE = "licenses/ffmpeg/NOTICE.txt"
NOTICE_MUST_MENTION = ["https://ffmpeg.org/releases/ffmpeg-", "SHA-256:", "./configure", "--disable-autodetect"]


def normalise(name: str) -> str:
    name = name.replace("\\", "/")
    while name.startswith("./"):
        name = name[2:]
    return name.rstrip("/")


def read_archive(path: str) -> tuple[dict[str, int], str | None]:
    """File sizes by archive path, and the text of the FFmpeg notice."""
    sizes = {}
    notice = None

    if path.endswith(".zip"):
        with zipfile.ZipFile(path) as archive:
            for info in archive.infolist():
                if info.is_dir():
                    continue
                name = normalise(info.filename)
                sizes[name] = info.file_size
                if name == NOTICE:
                    notice = archive.read(info).decode("utf-8", errors="replace")
    else:
        with tarfile.open(path, "r:gz") as archive:
            for member in archive:
                if not member.isfile():
                    continue
                name = normalise(member.name)
                sizes[name] = member.size
                if name == NOTICE:
                    notice = archive.extractfile(member).read().decode("utf-8", errors="replace")

    return sizes, notice


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("archive")
    parser.add_argument("--kind", required=True, choices=sorted(REQUIRED))
    args = parser.parse_args()

    sizes, notice = read_archive(args.archive)

    problems = []
    for name in REQUIRED[args.kind]:
        if name not in sizes:
            problems.append(f"missing {name}")
        elif sizes[name] == 0:
            problems.append(f"empty {name}")

    if notice is not None:
        for text in NOTICE_MUST_MENTION:
            if text not in notice:
                problems.append(f"{NOTICE} does not mention {text!r}")

    if problems:
        for problem in problems:
            print(f"::error::{args.archive}: {problem}")
        return 1

    print(f"{args.archive}: binaries and {len(REQUIRED[args.kind]) - 2} licence files present ({args.kind}).")
    return 0


if __name__ == "__main__":
    sys.exit(main())
