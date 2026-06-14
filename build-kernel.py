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

    image, is_local = mklib.resolve_image(arch)
    print(f"using build image: {image}", flush=True)
    print(f"target arch: {arch}", flush=True)

    # For a local image, --pull=never keeps podman from consulting a (possibly
    # broken) registry credential helper to re-resolve the --platform manifest.
    pull = ["--pull=never"] if is_local else []

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

    # arm64 builds 'Image', x86_64 builds 'bzImage'. The container runs the
    # target platform (native, or emulated for a foreign arch), so plain
    # CC=gcc-15 builds natively inside it -- no cross-compiler needed.
    subprocess.run(
        [
            "podman", "run", "--rm", *pull, *mklib.platform_args(arch),
            "-v", f"{linux_src}:/linux",
            *out_mount,
            "-w", "/linux",
            "-e", f"ARCH={prof['kernel_arch']}",
            "-e", f"TARGET={prof['image_name']}",
            "-e", f"OPT={opt}",
            image,
            "bash", "-c",
            'make ARCH="$ARCH" $OPT CC=gcc-15 HOSTCC=gcc-15 -j"$(nproc)" "$TARGET"',
        ],
        check=True,
    )

    print("=== built: ===", flush=True)
    subprocess.run(["ls", "-lh", str(mklib.kernel_image(linux_src, arch))])
    return 0


if __name__ == "__main__":
    sys.exit(main())
