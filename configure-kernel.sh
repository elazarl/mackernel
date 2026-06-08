#!/usr/bin/env bash
# (2) Configure the kernel to be minimally bootable under QEMU's "virt" machine.
# Starts from tinyconfig and enables only what's needed to print to the serial
# console, find the virtio disk over the PCIe host bridge, and mount an ext4
# rootfs + exec userspace.
set -euo pipefail
cd "$(dirname "$0")"
source ./lib.sh

LINUX_SRC="${LINUX_SRC:-$HOME/linux}"
ARCH="${ARCH:-arm64}"
IMG_REF="$(resolve_image)"
echo "using build image: $IMG_REF"

if [ ! -f "$LINUX_SRC/Makefile" ]; then
  echo "error: no kernel tree at LINUX_SRC=$LINUX_SRC" >&2
  exit 1
fi

podman run --rm -v "$LINUX_SRC:/linux" -w /linux -e ARCH="$ARCH" "$IMG_REF" bash -c '
  set -e
  make ARCH="$ARCH" tinyconfig

  # console + printk
  ./scripts/config -e PRINTK -e PRINTK_TIME -e TTY
  # block layer + virtio disk
  ./scripts/config -e BLOCK -e BLK_DEV
  ./scripts/config -e PCI -e VIRTIO_MENU -e VIRTIO_PCI -e VIRTIO_BLK -e VIRTIO_MMIO
  # the part tinyconfig misses: the arm64 "virt" PCIe ECAM host bridge
  # (without it the PCI bus is never enumerated and the disk never appears)
  ./scripts/config -e PCI_HOST_GENERIC -e PCI_HOST_COMMON -e PCI_ECAM
  # root filesystem
  ./scripts/config -e EXT4_FS
  # run userspace binaries + scripts
  ./scripts/config -e BINFMT_ELF -e BINFMT_SCRIPT -e BINFMT_MISC
  # almost nothing works without these
  ./scripts/config -e PROC_FS -e SYSFS -e DEVTMPFS -e DEVTMPFS_MOUNT
  ./scripts/config -e FUTEX -e MULTIUSER
  # arm64 "virt" platform: PL011 serial (ttyAMA0), GICv3, arch timer, PSCI
  ./scripts/config -e SERIAL_AMBA_PL011 -e SERIAL_AMBA_PL011_CONSOLE
  ./scripts/config -e ARM_GIC -e ARM_GIC_V3 -e ARM_ARCH_TIMER -e ARM_PSCI_FW
  ./scripts/config -e POWER_RESET -e POWER_RESET_SYSCON -e OF -e OF_FLATTREE
  # debug symbols (attach with: qemu -s  +  gdb)
  ./scripts/config -e DEBUG_INFO_DWARF5

  make ARCH="$ARCH" olddefconfig
'

echo "=== configured. key options: ==="
grep -E "^CONFIG_(VIRTIO_BLK|PCI_HOST_GENERIC|SERIAL_AMBA_PL011_CONSOLE|EXT4_FS)=" "$LINUX_SRC/.config"
