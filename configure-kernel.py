#!/usr/bin/env python3
# (2) Configure the kernel to be minimally bootable under QEMU's "virt"/"q35"
# machine. Starts from tinyconfig, merges the Kconfig fragments in kconf/
# (base + the per-arch platform fragment), then layers on whatever debugging /
# sanitizer options you ask for via the flags below.
#
#   ./configure-kernel.py                      # minimal bootable config
#   ./configure-kernel.py --kasan              # + Kernel Address Sanitizer
#   ./configure-kernel.py --kasan --atalk      # the AppleTalk race reproducer
#   ./configure-kernel.py --all-sanitizers     # everything debugging-related
#   ./configure-kernel.py -e NET_9P -e 9P_FS   # arbitrary scripts/config tokens
#   ARCH=x86_64 ./configure-kernel.py          # configure for x86_64 instead
#
# Anything after the flags (or any unrecognised -e/-d/-m/--set-* token) is passed
# verbatim to scripts/config, applied last so it wins, before olddefconfig
# resolves dependencies. The EXTRA_CONFIG env var still works and is applied
# first (CLI flags win over it).
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mklib  # noqa: E402

HERE = Path(__file__).resolve().parent

USAGE = """\
usage: configure-kernel.py [flags] [scripts/config tokens...]

Configure a minimal, bootable kernel from kconf/ fragments (base + per-arch).

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

Env vars: LINUX_SRC (=$HOME/linux), ARCH (=host arch), EXTRA_CONFIG.
"""


def parse_args(argv: list[str]) -> tuple[list[str], bool]:
    """Return (scripts/config tokens, want_full_slub). Mirrors the old bash
    arg loop: sanitizer flags expand to tokens, -e/-d/-m and --set-* pass
    through, -- forwards the rest verbatim, unknown -... tokens forward too."""
    cfg: list[str] = []
    want_full_slub = False
    i = 0
    while i < len(argv):
        a = argv[i]
        if a == "--kasan":
            cfg += ["-e", "KASAN", "-e", "KASAN_OUTLINE", "-e", "KASAN_VMALLOC"]
            want_full_slub = True
        elif a == "--kasan-inline":
            cfg += ["-e", "KASAN", "-e", "KASAN_INLINE", "-e", "KASAN_VMALLOC"]
            want_full_slub = True
        elif a == "--kfence":
            cfg += ["-e", "KFENCE"]
            want_full_slub = True
        elif a == "--kcsan":
            cfg += ["-e", "KCSAN"]
        elif a == "--ubsan":
            cfg += ["-e", "UBSAN", "-e", "UBSAN_BOUNDS", "-e", "UBSAN_SHIFT"]
        elif a == "--kmemleak":
            cfg += ["-e", "DEBUG_KMEMLEAK"]
        elif a == "--lockdep":
            cfg += ["-e", "PROVE_LOCKING", "-e", "DEBUG_ATOMIC_SLEEP", "-e", "LOCK_STAT"]
        elif a == "--all-sanitizers":
            cfg += ["-e", "KASAN", "-e", "KASAN_OUTLINE", "-e", "KASAN_VMALLOC",
                    "-e", "UBSAN", "-e", "UBSAN_BOUNDS", "-e", "UBSAN_SHIFT",
                    "-e", "KFENCE",
                    "-e", "PROVE_LOCKING", "-e", "DEBUG_ATOMIC_SLEEP", "-e", "LOCK_STAT",
                    "-e", "DEBUG_KMEMLEAK"]
            want_full_slub = True
        elif a == "--atalk":
            cfg += ["-e", "ATALK"]
        elif a in ("-e", "-d", "-m"):
            if i + 1 >= len(argv):
                sys.exit(f"{a} needs a CONFIG symbol")
            cfg += [a, argv[i + 1]]
            i += 1
        elif a in ("--set-str", "--set-val"):
            if i + 2 >= len(argv):
                sys.exit(f"{a} needs a symbol and a value")
            cfg += [a, argv[i + 1], argv[i + 2]]
            i += 2
        elif a in ("-h", "--help"):
            print(USAGE)
            raise SystemExit(0)
        elif a == "--":
            cfg += argv[i + 1:]
            break
        else:
            # unknown -... token or positional: forward verbatim to scripts/config
            cfg.append(a)
        i += 1
    return cfg, want_full_slub


# In-container script: tinyconfig -> merge kconf fragments -> EXTRA_CONFIG ->
# olddefconfig. MK_O is the kernel `make O=` dir ("/out" when BUILD_DIR is set,
# empty for an in-tree build); CFG is the resulting .config path.
CONTAINER_SCRIPT = r'''
set -e
if [ -n "$MK_O" ]; then OPT="O=$MK_O"; CFG="$MK_O/.config"; else OPT=""; CFG=".config"; fi
make ARCH="$ARCH" $OPT tinyconfig
scripts/kconfig/merge_config.sh -m -O "$(dirname "$CFG")" "$CFG" /kconf/base.config "/kconf/${MK_ARCH}.config"
if [ -n "${EXTRA_CONFIG// /}" ]; then
  echo "applying EXTRA_CONFIG: $EXTRA_CONFIG"
  ./scripts/config --file "$CFG" $EXTRA_CONFIG
fi
make ARCH="$ARCH" $OPT olddefconfig
'''


def main() -> int:
    os.chdir(HERE)

    cfg, want_full_slub = parse_args(sys.argv[1:])
    if want_full_slub:
        # Heap sanitizers need the full SLUB allocator, not tinyconfig's SLUB_TINY.
        cfg += ["-d", "SLUB_TINY"]

    # Merge env EXTRA_CONFIG (first) with the CLI tokens (last, so they win).
    extra_config = (os.environ.get("EXTRA_CONFIG", "") + " " + " ".join(cfg)).strip()

    linux_src = os.environ.get("LINUX_SRC", os.path.expanduser("~/linux"))
    arch = mklib.target_arch()
    prof = mklib.arch_profile(arch)

    image, is_local = mklib.resolve_image(arch)
    print(f"using build image: {image}", flush=True)
    print(f"target arch: {arch}", flush=True)
    if extra_config:
        print(f"extra config: {extra_config}", flush=True)

    if not Path(linux_src, "Makefile").is_file():
        print(f"error: no kernel tree at LINUX_SRC={linux_src}", file=sys.stderr)
        return 1

    # Optional out-of-tree build dir, mounted at /out and used as `make O=`.
    out_mount, mk_o = mklib.build_dir_mount()

    # For a local image, --pull=never keeps podman from consulting a (possibly
    # broken) registry credential helper to re-resolve the --platform manifest.
    pull = ["--pull=never"] if is_local else []
    subprocess.run(
        [
            "podman", "run", "--rm", *pull, *mklib.platform_args(arch),
            "-v", f"{linux_src}:/linux",
            "-v", f"{HERE / 'kconf'}:/kconf:ro",
            *out_mount,
            "-w", "/linux",
            "-e", f"ARCH={prof['kernel_arch']}",
            "-e", f"MK_ARCH={arch}",
            "-e", f"MK_O={mk_o}",
            "-e", f"EXTRA_CONFIG={extra_config}",
            image,
            "bash", "-c", CONTAINER_SCRIPT,
        ],
        check=True,
    )

    print("=== configured. key options: ===", flush=True)
    keys = ("VIRTIO_BLK|VIRTIO_NET|PCI_HOST_GENERIC|PCI_MMCONFIG|"
            "SERIAL_AMBA_PL011_CONSOLE|SERIAL_8250_CONSOLE|EXT4_FS|NET|INET|"
            "CGROUPS|ISO9660_FS|INOTIFY_USER|KASAN|KFENCE|KCSAN|UBSAN|"
            "DEBUG_KMEMLEAK|PROVE_LOCKING|ATALK")
    subprocess.run(
        ["grep", "-E", f"^CONFIG_({keys})=", str(mklib.config_path(linux_src))]
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
