#!/bin/sh
#
# Builds the static Linux release bundle: the cfmpeg CLI and its helper ffmpeg,
# both linked against musl so they run on Alpine and other images without glibc.
#
#   build-musl-bundle.sh <rust-target> <bundle-dir>
#
# Run it from the repository root inside an Alpine container whose Rust
# toolchain has <rust-target> as its host target, such as the rust:alpine image
# on a machine of the matching architecture. Alpine's own toolchain targets
# musl, so the Rust code, ring's C code, and ffmpeg are all built natively.
# release-binaries.yml runs this through `docker run`.
#
# Writes <bundle-dir>/bin/cfmpeg, <bundle-dir>/libexec/ffmpeg, and the licence
# notices for both in <bundle-dir>/licenses.
#
# Environment:
#   FFMPEG_HELPER_VERSION  Passed through to build-helper-ffmpeg.sh.
#   HOST_UID, HOST_GID     When both are set, the build output is handed to that
#                          owner, because the container itself runs as root.

set -eu

if [ "$#" -ne 2 ]; then
  echo "usage: $0 <rust-target> <bundle-dir>" >&2
  exit 1
fi

RUST_TARGET="$1"
BUNDLE_DIR="$2"

if [ ! -f /etc/alpine-release ]; then
  echo "$0 must run inside an Alpine container" >&2
  exit 1
fi

apk add --no-cache bash build-base curl linux-headers python3 tar xz

# nasm only assembles x86; ffmpeg needs it there for its optimised routines.
case "$(uname -m)" in
  x86_64) apk add --no-cache nasm ;;
esac

# rustup's musl targets link statically by default, so no extra flags.
cargo build --release --locked --target "$RUST_TARGET"

HELPER_PREFIX="$(mktemp -d)"
FFMPEG_HELPER_STATIC=1 bash scripts/build-helper-ffmpeg.sh "$HELPER_PREFIX"

mkdir -p "$BUNDLE_DIR/bin" "$BUNDLE_DIR/libexec"
install -m 755 "target/$RUST_TARGET/release/cfmpeg" "$BUNDLE_DIR/bin/cfmpeg"
install -m 755 "$HELPER_PREFIX/bin/ffmpeg" "$BUNDLE_DIR/libexec/ffmpeg"
python3 scripts/assemble-licenses.py \
  --bundle "$BUNDLE_DIR" \
  --kind musl \
  --rust-target "$RUST_TARGET" \
  --ffmpeg-licenses "$HELPER_PREFIX/licenses/ffmpeg"
rm -rf "$HELPER_PREFIX"

if [ -n "${HOST_UID:-}" ] && [ -n "${HOST_GID:-}" ]; then
  chown -R "$HOST_UID:$HOST_GID" target "$BUNDLE_DIR"
fi
