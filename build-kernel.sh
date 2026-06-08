#!/usr/bin/env bash
# (3) Compile the kernel with gcc-15 inside the build container.
set -euo pipefail
cd "$(dirname "$0")"

IMAGE="${IMAGE:-mackernel-build}"
LINUX_SRC="${LINUX_SRC:-$HOME/linux}"
ARCH="${ARCH:-arm64}"

if [ ! -f "$LINUX_SRC/.config" ]; then
  echo "error: no .config in $LINUX_SRC -- run ./configure-kernel.sh first" >&2
  exit 1
fi

podman run --rm -v "$LINUX_SRC:/linux" -w /linux -e ARCH="$ARCH" "$IMAGE" \
  bash -c 'make ARCH="$ARCH" CC=gcc-15 HOSTCC=gcc-15 -j"$(nproc)" Image'

echo "=== built: ==="
ls -lh "$LINUX_SRC/arch/$ARCH/boot/Image"
