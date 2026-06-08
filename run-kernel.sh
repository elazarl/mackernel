#!/usr/bin/env bash
# (4) Download the Ubuntu cloud image if absent, then boot the built kernel
# against it with QEMU. Runs on the macOS host (no sudo) with HVF acceleration.
set -euo pipefail
cd "$(dirname "$0")"

LINUX_SRC="${LINUX_SRC:-$HOME/linux}"
ARCH="${ARCH:-arm64}"
KIMG="$LINUX_SRC/arch/$ARCH/boot/Image"

# Ubuntu cloud image (arm64). Override IMG/IMG_URL for a different release.
IMG="${IMG:-noble-server-cloudimg-arm64.img}"
IMG_URL="${IMG_URL:-https://cloud-images.ubuntu.com/noble/current/$IMG}"

# Build the kernel if it hasn't been built yet.
if [ ! -f "$KIMG" ]; then
  echo "kernel Image not found, building it first..."
  ./build-kernel.sh
fi

# Download the cloud image if absent (resumable).
if [ ! -f "$IMG" ]; then
  echo "cloud image not found, downloading $IMG_URL ..."
  curl -LfsS -C - -o "$IMG" "$IMG_URL"
fi

echo "=== booting $KIMG (Ctrl-a x to quit QEMU) ==="
exec qemu-system-aarch64 \
    -nographic \
    -machine virt,gic-version=3 \
    -cpu host -accel hvf \
    -m 2048 -smp 4 \
    -kernel "$KIMG" \
    -drive file="$IMG",if=virtio,format=qcow2 \
    -append "console=ttyAMA0 root=/dev/vda1 init=/bin/bash" \
    -snapshot
