#!/usr/bin/env bash
# (4) Download the Ubuntu cloud image if absent, then boot the built kernel
# against it with QEMU. Runs on the macOS host (no sudo) with HVF acceleration.
#
# Networking: a virtio NIC on QEMU user-mode (slirp) networking with a host
# port-forward, plus a cloud-init seed that DHCPs the NIC and installs an SSH key.
# The cloud image's real init (systemd) boots so cloud-init actually runs, then:
#
#     ssh -p "$SSH_PORT" -i id_mackernel mac@127.0.0.1
#
# reaches the guest from the Mac host. Set INIT=/bin/bash for the old straight-to-
# shell behaviour (cloud-init/networking will NOT run in that mode).
set -euo pipefail
cd "$(dirname "$0")"

LINUX_SRC="${LINUX_SRC:-$HOME/linux}"
ARCH="${ARCH:-arm64}"
KIMG="$LINUX_SRC/arch/$ARCH/boot/Image"

# Ubuntu cloud image (arm64). Override IMG/IMG_URL for a different release.
IMG="${IMG:-noble-server-cloudimg-arm64.img}"
IMG_URL="${IMG_URL:-https://cloud-images.ubuntu.com/noble/current/$IMG}"

# Host port forwarded to the guest's SSH (22). ssh -p "$SSH_PORT" mac@127.0.0.1
SSH_PORT="${SSH_PORT:-2222}"
SEED="${SEED:-seed.iso}"
# Default: boot the real init (systemd) so cloud-init runs. INIT=/bin/bash skips it.
INIT="${INIT:-}"

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

# Build the cloud-init seed (SSH key + DHCP network config) if absent. An existing
# seed is reused as-is, so `rm seed.iso` after changing GUEST_USER/SSH_KEY/GUEST_PASS.
if [ ! -f "$SEED" ]; then
  echo "cloud-init seed not found, building $SEED ..."
  ./make-seed.sh
fi

# Kernel cmdline. With the default (empty) INIT the cloud image boots systemd ->
# cloud-init -> sshd. INIT=/bin/bash drops straight to a shell (no networking).
APPEND="console=ttyAMA0 root=/dev/vda1 rw"
[ -n "$INIT" ] && APPEND="$APPEND init=$INIT"

echo "=== booting $KIMG ==="
echo "    SSH:  ssh -p $SSH_PORT -i id_mackernel mac@127.0.0.1   (after cloud-init finishes)"
echo "    quit: Ctrl-a x"
exec qemu-system-aarch64 \
    -nographic \
    -machine virt,gic-version=3 \
    -cpu host -accel hvf \
    -m 2048 -smp 4 \
    -kernel "$KIMG" \
    -drive file="$IMG",if=virtio,format=qcow2 \
    -drive file="$SEED",if=virtio,format=raw,readonly=on \
    -netdev user,id=net0,hostfwd=tcp::"$SSH_PORT"-:22 \
    -device virtio-net-pci,netdev=net0 \
    -device virtio-rng-pci \
    -append "$APPEND" \
    -snapshot
