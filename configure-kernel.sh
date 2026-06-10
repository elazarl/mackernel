#!/usr/bin/env bash
# (2) Configure the kernel to be minimally bootable under QEMU's "virt" machine.
# Starts from tinyconfig and enables only what's needed to print to the serial
# console, find the virtio disk over the PCIe host bridge, and mount an ext4
# rootfs + exec userspace -- then layers on whatever debugging / sanitizer
# options you ask for via the flags below.
#
#   ./configure-kernel.sh                      # minimal bootable config
#   ./configure-kernel.sh --kasan              # + Kernel Address Sanitizer
#   ./configure-kernel.sh --kasan --atalk      # the AppleTalk race reproducer
#   ./configure-kernel.sh --all-sanitizers     # everything debugging-related
#   ./configure-kernel.sh -e NET_9P -e 9P_FS   # arbitrary scripts/config tokens
#
# Anything after the flags (or any unrecognised -e/-d/-m/--set-* token) is passed
# verbatim to scripts/config, applied last so it wins, before olddefconfig
# resolves dependencies. The EXTRA_CONFIG env var still works and is applied
# first (CLI flags win over it).
set -euo pipefail
cd "$(dirname "$0")"
source ./lib.sh

LINUX_SRC="${LINUX_SRC:-$HOME/linux}"
ARCH="${ARCH:-arm64}"
EXTRA_CONFIG="${EXTRA_CONFIG:-}"

usage() {
  sed -n '2,17p' "$0" | sed 's/^# \{0,1\}//'
  cat <<'EOF'

Sanitizer / debugging flags (all off by default):
  --kasan           Kernel Address Sanitizer (outline; use-after-free, OOB)
  --kasan-inline    KASAN with inline instrumentation (faster guest, bigger build)
  --kfence          Kernel Electric-Fence (low-overhead memory-safety sampling)
  --kcsan           Kernel Concurrency Sanitizer (data-race detector)
  --ubsan           Undefined Behaviour Sanitizer
  --kmemleak        kmemleak memory-leak detector
  --lockdep         lockdep + atomic-sleep checks (deadlock / locking bugs)
  --all-sanitizers  --kasan --ubsan --kfence --lockdep --kmemleak
  --atalk           build the AppleTalk (DDP) stack (CONFIG_ATALK)

Config passthrough (forwarded to scripts/config, repeatable):
  -e SYM            enable CONFIG_SYM
  -d SYM            disable CONFIG_SYM
  -m SYM            build CONFIG_SYM as a module
  --set-str K V     set CONFIG_K to string V
  --set-val K V     set CONFIG_K to value V

  -h, --help        show this help and exit

Env vars: LINUX_SRC (=$HOME/linux), ARCH (=arm64), EXTRA_CONFIG.
EOF
}

# Accumulated scripts/config tokens from the CLI (applied after EXTRA_CONFIG).
CLI_CONFIG=()
add() { CLI_CONFIG+=("$@"); }

# Heap sanitizers need the full SLUB allocator, not tinyconfig's SLUB_TINY.
want_full_slub=0

add_kasan()    { add -e KASAN -e KASAN_OUTLINE -e KASAN_VMALLOC; want_full_slub=1; }
add_kfence()   { add -e KFENCE; want_full_slub=1; }
add_ubsan()    { add -e UBSAN -e UBSAN_BOUNDS -e UBSAN_SHIFT; }
add_kcsan()    { add -e KCSAN; }
add_kmemleak() { add -e DEBUG_KMEMLEAK; }
add_lockdep()  { add -e PROVE_LOCKING -e DEBUG_ATOMIC_SLEEP -e LOCK_STAT; }

while [ $# -gt 0 ]; do
  case "$1" in
    --kasan)          add_kasan ;;
    --kasan-inline)   add -e KASAN -e KASAN_INLINE -e KASAN_VMALLOC; want_full_slub=1 ;;
    --kfence)         add_kfence ;;
    --kcsan)          add_kcsan ;;
    --ubsan)          add_ubsan ;;
    --kmemleak)       add_kmemleak ;;
    --lockdep)        add_lockdep ;;
    --all-sanitizers) add_kasan; add_ubsan; add_kfence; add_lockdep; add_kmemleak ;;
    --atalk)          add -e ATALK ;;
    -e|-d|-m)         add "$1" "${2:?$1 needs a CONFIG symbol}"; shift ;;
    --set-str|--set-val)
                      add "$1" "${2:?$1 needs a symbol}" "${3:?$1 needs a value}"; shift 2 ;;
    -h|--help)        usage; exit 0 ;;
    --)               shift; while [ $# -gt 0 ]; do add "$1"; shift; done; break ;;
    -*)               # unknown -... token: forward it verbatim to scripts/config
                      add "$1" ;;
    *)                add "$1" ;;
  esac
  shift
done

[ "$want_full_slub" -eq 1 ] && add -d SLUB_TINY

# Merge env EXTRA_CONFIG (first) with the CLI tokens (last, so they win).
EXTRA_CONFIG="$EXTRA_CONFIG ${CLI_CONFIG[*]:-}"

IMG_REF="$(resolve_image)"
echo "using build image: $IMG_REF"
[ -n "${EXTRA_CONFIG// /}" ] && echo "extra config: $EXTRA_CONFIG"

if [ ! -f "$LINUX_SRC/Makefile" ]; then
  echo "error: no kernel tree at LINUX_SRC=$LINUX_SRC" >&2
  exit 1
fi

podman run --rm -v "$LINUX_SRC:/linux" -w /linux -e ARCH="$ARCH" -e EXTRA_CONFIG="$EXTRA_CONFIG" "$IMG_REF" bash -c '
  set -e
  make ARCH="$ARCH" tinyconfig

  # console + printk
  ./scripts/config -e PRINTK -e PRINTK_TIME -e TTY
  # block layer + virtio disk
  ./scripts/config -e BLOCK -e BLK_DEV
  ./scripts/config -e PCI -e VIRTIO_MENU -e VIRTIO_PCI -e VIRTIO_BLK -e VIRTIO_MMIO
  # virtio networking + virtio entropy (so sshd/cloud-init do not stall on low entropy)
  ./scripts/config -e VIRTIO_NET -e HW_RANDOM -e HW_RANDOM_VIRTIO
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

  # boot an initramfs (used by run-kernel-test.sh) + readable oops/sanitizer traces
  ./scripts/config -e BLK_DEV_INITRD -e RD_GZIP
  ./scripts/config -e KALLSYMS -e KALLSYMS_ALL
  # debug symbols (attach with: qemu -s  +  gdb)
  ./scripts/config -e DEBUG_INFO_DWARF5

  # === networking + cloud-init: enough kernel to boot the real init (systemd) ===
  # so cloud-init runs, configures the NIC, and starts sshd we can reach from the host.

  # TCP/IP stack: PACKET (dhcp), UNIX sockets (systemd/udev), INET, the netdev core.
  ./scripts/config -e NET -e PACKET -e UNIX -e INET -e IPV6
  ./scripts/config -e NETDEVICES -e NET_CORE

  # systemd prerequisites that tinyconfig (+EXPERT) strips out:
  #   control-group hierarchy, the *fd/inotify/epoll syscalls, file locking, tmpfs/shmem,
  #   process namespaces, and the proc sysctl interface.
  ./scripts/config -e CGROUPS -e NAMESPACES -e UTS_NS -e IPC_NS -e PID_NS -e NET_NS -e USER_NS
  ./scripts/config -e EPOLL -e SIGNALFD -e TIMERFD -e EVENTFD -e INOTIFY_USER -e FHANDLE
  ./scripts/config -e FILE_LOCKING -e SHMEM -e TMPFS -e TMPFS_POSIX_ACL -e POSIX_MQUEUE
  ./scripts/config -e SYSCTL -e PROC_SYSCTL -e UNIX98_PTYS
  ./scripts/config -e SECCOMP -e SECCOMP_FILTER
  # crypto bits sshd / systemd reach for
  ./scripts/config -e CRYPTO -e CRYPTO_HMAC -e CRYPTO_SHA256 -e CRYPTO_USER_API_HASH

  # the cloud image /boot/efi is a vfat partition; with no FAT support that mount
  # fails, local-fs.target fails, and the boot drops to emergency mode (no sshd).
  ./scripts/config -e FAT_FS -e VFAT_FS

  # mount the cloud-init NoCloud seed (an ISO9660+Joliet disk labelled CIDATA).
  # Joliet preserves the lowercase "user-data"/"meta-data" filenames, and pulls in NLS.
  ./scripts/config -e ISO9660_FS -e JOLIET
  ./scripts/config -e NLS -e NLS_CODEPAGE_437 -e NLS_ISO8859_1 -e NLS_UTF8
  ./scripts/config --set-str NLS_DEFAULT "utf8"

  # caller-supplied extra Kconfig tweaks (sanitizer flags + passthrough), applied
  # last so they win, then let olddefconfig pull in whatever deps they imply.
  if [ -n "${EXTRA_CONFIG// /}" ]; then
    echo "applying EXTRA_CONFIG: $EXTRA_CONFIG"
    ./scripts/config $EXTRA_CONFIG
  fi

  make ARCH="$ARCH" olddefconfig
'

echo "=== configured. key options: ==="
grep -E "^CONFIG_(VIRTIO_BLK|VIRTIO_NET|PCI_HOST_GENERIC|SERIAL_AMBA_PL011_CONSOLE|EXT4_FS|NET|INET|CGROUPS|ISO9660_FS|INOTIFY_USER|KASAN|KFENCE|KCSAN|UBSAN|DEBUG_KMEMLEAK|PROVE_LOCKING|ATALK)=" "$LINUX_SRC/.config" || true
