#!/usr/bin/env bash

set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <output-prefix>" >&2
  exit 1
fi

PREFIX="$1"
VERSION="${FFMPEG_HELPER_VERSION:-8.0}"
BUILD_ROOT="$(mktemp -d)"
ARCHIVE_URL="https://ffmpeg.org/releases/ffmpeg-${VERSION}.tar.xz"

cleanup() {
  rm -rf "$BUILD_ROOT"
}

trap cleanup EXIT

mkdir -p "$PREFIX/bin"

curl -fsSL "$ARCHIVE_URL" -o "$BUILD_ROOT/ffmpeg.tar.xz"
tar -xf "$BUILD_ROOT/ffmpeg.tar.xz" -C "$BUILD_ROOT"

SOURCE_DIR="$BUILD_ROOT/ffmpeg-${VERSION}"
if [ ! -d "$SOURCE_DIR" ]; then
  echo "expected source directory $SOURCE_DIR to exist after extracting $ARCHIVE_URL" >&2
  exit 1
fi

if command -v getconf >/dev/null 2>&1; then
  JOBS="$(getconf _NPROCESSORS_ONLN 2>/dev/null || echo 4)"
elif command -v sysctl >/dev/null 2>&1; then
  JOBS="$(sysctl -n hw.ncpu 2>/dev/null || echo 4)"
else
  JOBS=4
fi

pushd "$SOURCE_DIR" >/dev/null

./configure \
  --prefix="$PREFIX" \
  --disable-autodetect \
  --disable-debug \
  --disable-doc \
  --disable-ffplay \
  --disable-shared \
  --enable-static

make -j"$JOBS" ffmpeg ffprobe
install -m 755 ffmpeg "$PREFIX/bin/ffmpeg"
install -m 755 ffprobe "$PREFIX/bin/ffprobe"

popd >/dev/null
