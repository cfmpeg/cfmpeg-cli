# Licence texts shipped in release archives

`scripts/assemble-licenses.py` copies these files into the `licenses/` directory of the release archives whose binaries have the corresponding code linked into them. They are verbatim copies from the upstream projects; do not edit them.

| Directory | Shipped in | Upstream source |
| --- | --- | --- |
| `musl/` | static Linux (musl) archives | `COPYRIGHT` from https://musl.libc.org/releases/musl-1.2.5.tar.gz |
| `mingw-w64/` | Windows archive | mingw-w64 v11.0.1 (https://github.com/mingw-w64/mingw-w64/tree/v11.0.1): `COPYING`, `COPYING.MinGW-w64-runtime/COPYING.MinGW-w64-runtime.txt`, `DISCLAIMER`, `DISCLAIMER.PD`, and `mingw-w64-libraries/winpthreads/COPYING` (stored here as `COPYING.winpthreads`) |
| `gcc-runtime/` | static Linux (musl) and Windows archives | `COPYING.RUNTIME` and `COPYING3` from GCC 13.2.0 (https://github.com/gcc-mirror/gcc/tree/releases/gcc-13.2.0) |

Everything else in an archive's `licenses/` directory is gathered when the archive is built:

- cfmpeg's own `LICENSE` comes from the repository root.
- The FFmpeg licence files come from the FFmpeg source tarball the helper is built from, and `scripts/build-helper-ffmpeg.sh` writes the accompanying `NOTICE.txt`.
- The Rust crate notices come from the crate sources Cargo downloaded for the build, and the Rust standard library notices come from the toolchain that compiled it.

Refresh a file here when the toolchain that builds the releases moves to a version whose licence text differs.
