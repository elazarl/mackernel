#!/usr/bin/env python3
"""(6) Compile a C file fully statically in the build container, boot the
mackernel under QEMU, and run the binary inside the guest over SSH.

    ./run-in-kernel.py test_atalk_race_ssh.c            # build, boot, run
    ./run-in-kernel.py --sudo repro.c -- --threads 8    # run as root, pass args
    ./run-in-kernel.py -o /tmp/x prog.c                  # keep the static binary

It glues together the existing pieces:
  * the same Podman build image as build-kernel.py (gcc-15) -> a *static* ELF,
  * the kernel Image from build-kernel.py (built on demand if missing),
  * the cloud image + cloud-init seed from run-kernel.py / make-seed.sh,
  * QEMU user-mode networking with a forwarded SSH port.

Static linking matters: the guest is an Ubuntu cloud image, but a statically
linked binary has no runtime library dependencies, so a program compiled in the
build container runs unchanged in the guest regardless of its libc.

The boot/SSH/run/teardown engine lives in guestlib.py (shared with run-kernel.py).
"""
from __future__ import annotations

import argparse
import os
import shutil
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mklib  # noqa: E402
import guestlib as g  # noqa: E402
from guestlib import die, log, run  # noqa: E402

g.TAG = "run-in-kernel"
HERE = Path(__file__).resolve().parent


def ensure_kernel(linux_src: Path, arch: str) -> Path:
    """Build the kernel for `arch` if its image is missing; return the image path."""
    kimg = mklib.kernel_image(linux_src, arch)
    if not kimg.exists():
        log("kernel image not found, building it (./build-kernel.py) ...")
        if run([sys.executable, str(HERE / "build-kernel.py")], cwd=HERE,
               env={**os.environ, "LINUX_SRC": str(linux_src)}).returncode != 0:
            die("kernel build failed")
    return kimg


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Compile a C file statically and run it inside the booted mackernel over SSH.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("cfile", type=Path, help="C source file to compile and run")
    ap.add_argument("prog_args", nargs="*", help="arguments passed to the program in the guest")
    ap.add_argument("--sudo", action="store_true",
                    help="run the program as root in the guest (needed for raw sockets, etc.)")
    ap.add_argument("-o", "--output", type=Path,
                    help="also copy the static binary to this host path")
    ap.add_argument("--cflags", default="",
                    help="extra flags for gcc (space-separated), e.g. --cflags='-Wall -g'")
    ap.add_argument("--ssh-port", type=int, default=int(os.environ.get("SSH_PORT", "2222")),
                    help="host port to forward to guest:22 (default 2222, auto-bumped if busy)")
    ap.add_argument("--boot-timeout", type=int, default=120,
                    help="seconds to wait for guest SSH (default 120)")
    ap.add_argument("--keep-running", action="store_true",
                    help="leave QEMU running after the program exits")
    args = ap.parse_args()

    os.chdir(HERE)
    if not args.cfile.exists():
        die(f"no such C file: {args.cfile}")

    arch = mklib.target_arch()
    linux_src = Path(os.environ.get("LINUX_SRC", str(Path.home() / "linux")))
    img = Path(os.environ.get("IMG", mklib.arch_profile(arch)["cloud_img"]))
    img_url = os.environ.get(
        "IMG_URL", f"https://cloud-images.ubuntu.com/noble/current/{img.name}")
    seed = Path(os.environ.get("SEED", "seed.iso"))
    key = Path(os.environ.get("SSH_KEY", "id_mackernel"))
    user = os.environ.get("GUEST_USER", "mac")

    image, is_local = mklib.resolve_image(arch)
    log(f"using build image: {image} (target arch: {arch})")

    ensure_kernel(linux_src, arch)
    g.ensure_cloud_image(img, img_url)
    g.ensure_seed(seed)

    binary = g.compile_c([args.cfile], args.cfile.stem, image, is_local,
                         mklib.platform_args(arch),
                         args.cflags.split() if args.cflags else [])
    builddir = binary.parent
    if args.output:
        shutil.copy(binary, args.output)
        log(f"static binary saved to {args.output}")

    port = g.free_port(args.ssh_port)
    if port != args.ssh_port:
        log(f"port {args.ssh_port} busy, using {port} instead")

    boot_log = HERE / "run-in-kernel-boot.log"
    proc = g.boot_qemu(arch, linux_src, img, seed, port, boot_log)
    rc = 1
    try:
        g.wait_for_ssh(port, key, user, args.boot_timeout)

        guest_path = f"/tmp/{binary.name}"
        log(f"copying binary to guest:{guest_path} ...")
        if g.scp_to_guest(port, key, user, [binary], guest_path) != 0:
            die("scp of the binary to the guest failed")

        runner = "sudo " if args.sudo else ""
        quoted_args = " ".join("'%s'" % a.replace("'", "'\\''") for a in args.prog_args)
        remote_cmd = f"chmod +x {guest_path} && {runner}{guest_path} {quoted_args}"
        log(f"running in guest: {runner}{guest_path} {' '.join(args.prog_args)}".rstrip())
        print("\033[1;32m---------------- guest program output ----------------\033[0m", flush=True)
        rc = g.ssh_run(port, key, user, remote_cmd)
        print("\033[1;32m------------------------------------------------------\033[0m", flush=True)
        log(f"program exited with status {rc}")
    finally:
        if args.keep_running:
            log(f"--keep-running: QEMU still up. SSH: ssh -p {port} -i {key} {user}@127.0.0.1")
            log(f"  kill it with: kill {proc.pid}")
        else:
            g.teardown(proc)
            shutil.rmtree(builddir, ignore_errors=True)

    return rc


if __name__ == "__main__":
    sys.exit(main())
