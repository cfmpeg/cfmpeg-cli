#!/usr/bin/env bash
#
# Builds the helper ffmpeg that cfmpeg releases bundle, from ffmpeg.org source.
#
#   build-helper-ffmpeg.sh <output-prefix>
#
# The binary lands at <output-prefix>/bin/ffmpeg (ffmpeg.exe for Windows). The
# licence texts from the source tarball, and a NOTICE.txt recording the source
# and the configure flags of this build, land in <output-prefix>/licenses/ffmpeg
# so that release archives can carry them next to the binary.
#
# Environment:
#   FFMPEG_HELPER_VERSION  ffmpeg release to build. Default: 8.0.
#   FFMPEG_HELPER_TARGET   "native" (default) builds for the host.
#                          "windows-x64" cross-compiles with the
#                          x86_64-w64-mingw32 toolchain and links statically.
#   FFMPEG_HELPER_STATIC   "1" links the helper fully statically. Use it where
#                          the C library supports that, such as musl on Alpine.

set -euo pipefail

if [ "$#" -ne 1 ]; then
  echo "usage: $0 <output-prefix>" >&2
  exit 1
fi

PREFIX="$1"
VERSION="${FFMPEG_HELPER_VERSION:-8.0}"
TARGET="${FFMPEG_HELPER_TARGET:-native}"
STATIC="${FFMPEG_HELPER_STATIC:-0}"
EXE_SUFFIX=""
TARGET_FLAGS=()

case "$TARGET" in
  native)
    ;;
  windows-x64)
    CROSS_PREFIX="x86_64-w64-mingw32-"
    if ! command -v "${CROSS_PREFIX}gcc" >/dev/null 2>&1; then
      echo "${CROSS_PREFIX}gcc is required to build the $TARGET helper" >&2
      exit 1
    fi
    EXE_SUFFIX=".exe"
    STATIC=1
    TARGET_FLAGS=(
      "--arch=x86_64"
      "--target-os=mingw32"
      "--cross-prefix=$CROSS_PREFIX"
    )
    ;;
  *)
    echo "unsupported FFMPEG_HELPER_TARGET: $TARGET (expected native or windows-x64)" >&2
    exit 1
    ;;
esac

BUILD_ROOT="$(mktemp -d)"
ARCHIVE_URL="https://ffmpeg.org/releases/ffmpeg-${VERSION}.tar.xz"

cleanup() {
  rm -rf "$BUILD_ROOT"
}

trap cleanup EXIT

sha256_of() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | cut -d ' ' -f 1
  else
    shasum -a 256 "$1" | cut -d ' ' -f 1
  fi
}

mkdir -p "$PREFIX/bin"

curl -fsSL "$ARCHIVE_URL" -o "$BUILD_ROOT/ffmpeg.tar.xz"
ARCHIVE_SHA256="$(sha256_of "$BUILD_ROOT/ffmpeg.tar.xz")"
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

CONFIGURE_FLAGS=(
  "--prefix=$PREFIX"
  "--disable-autodetect"
  "--disable-debug"
  "--disable-doc"
  "--disable-ffplay"
  "--disable-shared"
  "--enable-static"
)

if [ "${#TARGET_FLAGS[@]}" -gt 0 ]; then
  CONFIGURE_FLAGS+=("${TARGET_FLAGS[@]}")
fi

if [ "$STATIC" = "1" ]; then
  CONFIGURE_FLAGS+=("--extra-ldflags=-static")
fi

if ! command -v nasm >/dev/null 2>&1; then
  CONFIGURE_FLAGS+=("--disable-x86asm")
fi

./configure "${CONFIGURE_FLAGS[@]}"

MAKE_ARGS=("-j$JOBS" "ffmpeg${EXE_SUFFIX}")
LINK_MAP="$BUILD_ROOT/ffmpeg.map"

# A static link pulls code from the toolchain's own libraries into the binary.
# The linker map records which ones, so that NOTICE.txt can list them.
if [ "$STATIC" = "1" ]; then
  MAKE_ARGS+=("LDFLAGS-ffmpeg=-Wl,-Map,$LINK_MAP")
fi

make "${MAKE_ARGS[@]}"
install -m 755 "ffmpeg${EXE_SUFFIX}" "$PREFIX/bin/ffmpeg${EXE_SUFFIX}"

LICENSE_DIR="$PREFIX/licenses/ffmpeg"
mkdir -p "$LICENSE_DIR"
install -m 644 COPYING.LGPLv2.1 LICENSE.md "$LICENSE_DIR/"

{
  echo "FFmpeg helper bundled with cfmpeg"
  echo "================================="
  echo
  echo "libexec/ffmpeg${EXE_SUFFIX} in this archive is FFmpeg ${VERSION}. It was built from the"
  echo "unmodified source release published by the FFmpeg project, and cfmpeg runs"
  echo "it as a separate program."
  echo
  echo "Source:   $ARCHIVE_URL"
  echo "SHA-256:  $ARCHIVE_SHA256"
  echo "          (of the source tarball this binary was built from)"
  echo "Licence:  GNU Lesser General Public License, version 2.1 or later."
  echo "          See COPYING.LGPLv2.1 and LICENSE.md next to this file."
  echo
  echo "The build enables no GPL or nonfree parts and no external libraries."
  echo "The exact configure invocation was:"
  echo
  echo "  ./configure"
  printf '    %s\n' "${CONFIGURE_FLAGS[@]}"
  echo
  echo "followed by: make ffmpeg${EXE_SUFFIX}"
  echo
  echo "The script that performed the build is scripts/build-helper-ffmpeg.sh in"
  echo "https://github.com/cfmpeg/cfmpeg-cli at the tag of this release. To rebuild"
  echo "the helper, run that script, or run the configure invocation above in the"
  echo "extracted source tarball."
  echo
  if [ "$STATIC" = "1" ]; then
    echo "This is a statically linked build. Besides FFmpeg's own libraries, the"
    echo "linker took code from these toolchain libraries:"
    echo
    # Members the linker pulled out of an archive start a line in the map.
    # Absolute paths are the toolchain's; FFmpeg's own archives are relative.
    if [ -f "$LINK_MAP" ] && grep -q '^/[^ (]*\.a(' "$LINK_MAP"; then
      grep -o '^/[^ (]*\.a(' "$LINK_MAP" | sed 's/($//' | sort -u | sed 's/^/  /'
    else
      echo "  (not recorded: the linker map was missing or listed none)"
    fi
    echo
    echo "Their licence texts are in the directories next to this one."
  else
    echo "This build links the platform's C library dynamically."
  fi
} > "$LICENSE_DIR/NOTICE.txt"

popd >/dev/null
