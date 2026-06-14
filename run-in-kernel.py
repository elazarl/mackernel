#!/usr/bin/env python3
"""(6) Compile a C file fully statically in the build container, boot the
mackernel under QEMU/HVF, and run the binary inside the guest over SSH.

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
"""
from __future__ import annotations

import argparse
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mklib  # noqa: E402

HERE = Path(__file__).resolve().parent


def log(msg: str) -> None:
    print(f"\033[1;36m[run-in-kernel]\033[0m {msg}", flush=True)


def die(msg: str) -> "None":
    print(f"\033[1;31m[run-in-kernel] error:\033[0m {msg}", file=sys.stderr, flush=True)
    sys.exit(1)


def run(cmd, **kw):
    """Run a command, echoing it; raise on non-zero unless check=False."""
    return subprocess.run(cmd, **kw)


def free_port(preferred: int) -> int:
    """Return `preferred` if it is free, otherwise an OS-assigned free port."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        try:
            s.bind(("127.0.0.1", preferred))
            return preferred
        except OSError:
            pass
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def compile_static(cfile: Path, image: str, is_local: bool, plat_args: list[str],
                   cflags: list[str]) -> Path:
    """Statically compile `cfile` inside the build container; return host path.

    Builds for the guest's architecture (plat_args is empty for a native build,
    or --platform for an emulated foreign arch), so the static binary matches the
    kernel it will run under.
    """
    builddir = Path(tempfile.mkdtemp(prefix=".rik-build-", dir=HERE))
    src_name = cfile.name
    shutil.copy(cfile, builddir / src_name)
    out_name = cfile.stem  # binary named after the source

    podman = ["podman", "run", "--rm", *plat_args]
    if is_local:
        podman += ["--pull=never"]
    podman += [
        "-v", f"{builddir}:/build", "-w", "/build", image,
        "gcc-15", "-static", "-O2", "-pthread", *cflags,
        "-o", out_name, src_name,
    ]
    log(f"compiling {cfile.name} statically in {image} ...")
    if run(podman).returncode != 0:
        shutil.rmtree(builddir, ignore_errors=True)
        die("static compilation failed")
    binary = builddir / out_name
    if not binary.exists():
        shutil.rmtree(builddir, ignore_errors=True)
        die("compiler produced no output binary")
    return binary


def ensure_prereqs(kimg: Path, img: Path, img_url: str, seed: Path) -> None:
    """Build the kernel / fetch the cloud image / make the seed if missing."""
    if not kimg.exists():
        log("kernel Image not found, building it (./build-kernel.py) ...")
        if run([sys.executable, "./build-kernel.py"], cwd=HERE).returncode != 0:
            die("kernel build failed")
    if not img.exists():
        log(f"cloud image not found, downloading {img_url} ...")
        if run(["curl", "-LfsS", "-C", "-", "-o", str(img), img_url]).returncode != 0:
            die("cloud image download failed")
    if not seed.exists():
        log(f"cloud-init seed not found, building {seed.name} (./make-seed.sh) ...")
        if run(["./make-seed.sh"], cwd=HERE).returncode != 0:
            die("seed build failed")


def ssh_base(port: int, key: Path, user: str) -> list[str]:
    return [
        "-p", str(port), "-i", str(key),
        "-o", "StrictHostKeyChecking=no",
        "-o", "UserKnownHostsFile=/dev/null",
        "-o", "GlobalKnownHostsFile=/dev/null",
        "-o", "LogLevel=ERROR",
        "-o", "ConnectTimeout=5",
        f"{user}@127.0.0.1",
    ]


def wait_for_ssh(port: int, key: Path, user: str, timeout: int) -> None:
    log(f"waiting for guest SSH on 127.0.0.1:{port} (cloud-init can take ~10-40s) ...")
    deadline = time.monotonic() + timeout
    attempt = 0
    while time.monotonic() < deadline:
        attempt += 1
        r = subprocess.run(
            ["ssh", *ssh_base(port, key, user), "true"],
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        )
        if r.returncode == 0:
            log("guest is up.")
            return
        if attempt % 5 == 0:
            log(f"  ... still waiting ({int(deadline - time.monotonic())}s left)")
        time.sleep(2)
    die(f"guest SSH did not come up within {timeout}s (see boot log)")


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
    prof = mklib.arch_profile(arch)
    accel, cpu = mklib.qemu_accel_cpu(arch)
    linux_src = Path(os.environ.get("LINUX_SRC", str(Path.home() / "linux")))
    kimg = mklib.kernel_image(linux_src, arch)
    img = Path(os.environ.get("IMG", prof["cloud_img"]))
    img_url = os.environ.get(
        "IMG_URL", f"https://cloud-images.ubuntu.com/noble/current/{img.name}")
    seed = Path(os.environ.get("SEED", "seed.iso"))
    key = Path(os.environ.get("SSH_KEY", "id_mackernel"))
    user = os.environ.get("GUEST_USER", "mac")

    image, is_local = mklib.resolve_image(arch)
    log(f"using build image: {image} (target arch: {arch})")

    ensure_prereqs(kimg, img, img_url, seed)

    binary = compile_static(args.cfile, image, is_local, mklib.platform_args(arch),
                            args.cflags.split() if args.cflags else [])
    builddir = binary.parent
    if args.output:
        shutil.copy(binary, args.output)
        log(f"static binary saved to {args.output}")

    port = free_port(args.ssh_port)
    if port != args.ssh_port:
        log(f"port {args.ssh_port} busy, using {port} instead")

    boot_log = HERE / "run-in-kernel-boot.log"
    qemu = [
        prof["qemu_binary"],
        "-machine", prof["qemu_machine"],
        "-cpu", cpu, "-accel", accel,
        "-m", "2048", "-smp", "4",
        "-kernel", str(kimg),
        "-drive", f"file={img},if=virtio,format=qcow2",
        "-drive", f"file={seed},if=virtio,format=raw,readonly=on",
        "-netdev", f"user,id=net0,hostfwd=tcp::{port}-:22",
        "-device", "virtio-net-pci,netdev=net0",
        "-device", "virtio-rng-pci",
        "-append", f"console={prof['console']} root=/dev/vda1 rw",
        "-snapshot",          # discard guest disk writes on exit
        "-display", "none",
        "-serial", f"file:{boot_log}",
        "-monitor", "none",
    ]

    log(f"booting kernel (serial -> {boot_log.name}) ...")
    qemu_proc = subprocess.Popen(qemu)
    rc = 1
    try:
        wait_for_ssh(port, key, user, args.boot_timeout)

        guest_path = f"/tmp/{binary.name}"
        log(f"copying binary to guest:{guest_path} ...")
        scp = subprocess.run(
            ["scp", "-P", str(port), "-i", str(key),
             "-o", "StrictHostKeyChecking=no",
             "-o", "UserKnownHostsFile=/dev/null",
             "-o", "GlobalKnownHostsFile=/dev/null",
             "-o", "LogLevel=ERROR",
             str(binary), f"{user}@127.0.0.1:{guest_path}"])
        if scp.returncode != 0:
            die("scp of the binary to the guest failed")

        runner = "sudo " if args.sudo else ""
        quoted_args = " ".join("'%s'" % a.replace("'", "'\\''") for a in args.prog_args)
        remote_cmd = f"chmod +x {guest_path} && {runner}{guest_path} {quoted_args}"
        log(f"running in guest: {runner}{guest_path} {' '.join(args.prog_args)}".rstrip())
        print("\033[1;32m---------------- guest program output ----------------\033[0m", flush=True)
        rc = subprocess.run(["ssh", *ssh_base(port, key, user), remote_cmd]).returncode
        print("\033[1;32m------------------------------------------------------\033[0m", flush=True)
        log(f"program exited with status {rc}")
    finally:
        if args.keep_running:
            log(f"--keep-running: QEMU still up. SSH: ssh -p {port} -i {key} {user}@127.0.0.1")
            log(f"  kill it with: kill {qemu_proc.pid}")
        else:
            log("shutting down the guest ...")
            qemu_proc.terminate()
            try:
                qemu_proc.wait(timeout=10)
            except subprocess.TimeoutExpired:
                qemu_proc.kill()
            shutil.rmtree(builddir, ignore_errors=True)

    return rc


if __name__ == "__main__":
    sys.exit(main())
