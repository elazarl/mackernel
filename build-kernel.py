#!/usr/bin/env python3
# (3) Compile the kernel with gcc-15 inside the build container.
import os
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mklib  # noqa: E402

HERE = Path(__file__).resolve().parent


def main() -> int:
    os.chdir(HERE)

    linux_src = os.environ.get("LINUX_SRC", os.path.expanduser("~/linux"))
    arch = mklib.target_arch()
    prof = mklib.arch_profile(arch)

    # gcc version from the bundle's `compiler:` key (run-kernel.py sets MK_GCC).
    gcc = os.environ.get("MK_GCC", "14")
    image, is_local = mklib.resolve_image(arch, gcc)
    print(f"using build image: {image} (gcc-{gcc})", flush=True)
    print(f"target arch: {arch}", flush=True)

    # For a local image, --pull=never keeps podman from consulting a (possibly
    # broken) registry credential helper to re-resolve the --platform manifest.
    pull = ["--pull=never"] if is_local else []
    # Pre-pull remote images so the hardened (network-less) run finds them locally.
    mklib.ensure_pulled(image, is_local, mklib.platform_args(arch))

    config = mklib.config_path(linux_src)
    if not config.is_file():
        print(
            f"error: no .config at {config} -- run ./configure-kernel.py first",
            file=sys.stderr,
        )
        return 1

    # Optional out-of-tree build dir, mounted at /out and used as `make O=`.
    out_mount, mk_o = mklib.build_dir_mount()
    opt = f"O={mk_o}" if mk_o else ""

    # arm64 builds 'Image', x86_64 builds 'bzImage'. CROSS is empty for a native
    # build (the container is the target arch, CC=gcc-N), or the cross prefix
    # (CC=aarch64-linux-gnu-gcc-N) when cross-compiling arm64 on an x86_64 host --
    # then the container is the host arch and the toolchain runs natively.
    cross = mklib.cross_compile(arch)
    subprocess.run(
        [
            "podman", "run", "--rm", *pull, *mklib.platform_args(arch),
            *mklib.hardening_args(arch),
            "-v", mklib.volume(linux_src, "/linux"),
            *out_mount,
            "-w", "/linux",
            "-e", f"ARCH={prof['kernel_arch']}",
            "-e", f"TARGET={prof['image_name']}",
            "-e", f"OPT={opt}",
            "-e", f"GCC={gcc}",
            "-e", f"CROSS={cross}",
            image,
            "bash", "-c",
            'make ARCH="$ARCH" $OPT CROSS_COMPILE="$CROSS" CC="${CROSS}gcc-$GCC" '
            'HOSTCC="gcc-$GCC" -j"$(nproc)" "$TARGET"',
        ],
        check=True,
    )

    print("=== built: ===", flush=True)
    subprocess.run(["ls", "-lh", str(mklib.kernel_image(linux_src, arch))])
    return 0


if __name__ == "__main__":
    sys.exit(main())
