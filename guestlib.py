#!/usr/bin/env python3
"""Shared guest engine for the mackernel runners.

Boot the built kernel under QEMU, wait for the guest's SSH, copy files in, run
commands, and tear down -- plus the cloud-image/seed prerequisites and the
in-container static compile. Used by run-in-kernel.py and run-kernel.py (bundle
mode). Per-arch QEMU/image settings come from mklib.
"""
from __future__ import annotations

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

# Prefix for log lines; callers may override (e.g. "run-in-kernel").
TAG = "mackernel"


def log(msg: str) -> None:
    print(f"\033[1;36m[{TAG}]\033[0m {msg}", flush=True)


def die(msg: str) -> "None":
    print(f"\033[1;31m[{TAG}] error:\033[0m {msg}", file=sys.stderr, flush=True)
    sys.exit(1)


def run(cmd, **kw):
    """Run a command; raise on non-zero only if check=True is passed."""
    return subprocess.run(cmd, **kw)


def free_port(preferred: int) -> int:
    """Return `preferred` if free, otherwise an OS-assigned free port."""
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


_SSH_OPTS = [
    "-o", "StrictHostKeyChecking=no",
    "-o", "UserKnownHostsFile=/dev/null",
    "-o", "GlobalKnownHostsFile=/dev/null",
    "-o", "LogLevel=ERROR",
    "-o", "ConnectTimeout=5",
]


def ssh_base(port: int, key: Path, user: str) -> list[str]:
    """ssh args (options + user@host); append the remote command."""
    return ["-p", str(port), "-i", str(key), *_SSH_OPTS, f"{user}@127.0.0.1"]


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


def scp_to_guest(port: int, key: Path, user: str, srcs, dest: str) -> int:
    """Recursively copy host paths into the guest at `dest`. Returns scp rc."""
    cmd = ["scp", "-r", "-P", str(port), "-i", str(key), *_SSH_OPTS,
           *[str(s) for s in srcs], f"{user}@127.0.0.1:{dest}"]
    return subprocess.run(cmd).returncode


def ssh_run(port: int, key: Path, user: str, remote_cmd: str, **kw) -> int:
    """Run a shell command in the guest, inheriting stdio. Returns its rc."""
    return subprocess.run(["ssh", *ssh_base(port, key, user), remote_cmd], **kw).returncode


def boot_qemu(arch: str, linux_src, img, seed, port: int, serial_log: Path) -> subprocess.Popen:
    """Boot the built kernel headless, serial -> serial_log; return the Popen.

    Uses -snapshot so guest disk writes are discarded on exit."""
    prof = mklib.arch_profile(arch)
    accel, cpu = mklib.qemu_accel_cpu(arch)
    kimg = mklib.kernel_image(linux_src, arch)
    # Guest network isolation: slirp restrict=on lets the host reach the guest
    # via the forwarded SSH port but blocks the guest from initiating any outbound
    # connection (it can't phone home). Boot/cloud-init/sshd need no egress.
    # Set GUEST_NET=open to allow guest egress (e.g. apt).
    restrict = "" if os.environ.get("GUEST_NET") == "open" else ",restrict=on"
    qemu = [
        mklib.qemu_binary(arch),
        *mklib.qemu_hardening_args(),
        "-machine", prof["qemu_machine"],
        "-cpu", cpu, "-accel", accel,
        "-m", "2048", "-smp", "4",
        "-kernel", str(kimg),
        "-drive", f"file={img},if=virtio,format=qcow2",
        "-drive", f"file={seed},if=virtio,format=raw,readonly=on",
        "-netdev", f"user,id=net0,hostfwd=tcp::{port}-:22{restrict}",
        "-device", "virtio-net-pci,netdev=net0",
        "-object", "rng-builtin,id=rng0",
        "-device", "virtio-rng-pci,rng=rng0",
        "-append", f"console={prof['console']} root=/dev/vda1 rw",
        "-snapshot",
        "-display", "none",
        "-serial", f"file:{serial_log}",
        "-monitor", "none",
    ]
    # Optional outer sandbox confining the qemu process (MK_SANDBOX); empty by default.
    # The serial log may live outside HERE (the service's --log-dir), so bind its
    # dir read-write or qemu can't create the log inside the jail.
    prefix = mklib.sandbox_prefix(arch, run_dir=HERE, files=[kimg, img, seed],
                                  writable=[Path(serial_log).resolve().parent])
    if prefix:
        log(f"sandbox: {os.environ.get('MK_SANDBOX')} ({prefix[0]})")
    log(f"booting kernel ({arch}, accel={accel}; serial -> {serial_log.name}) ...")
    return subprocess.Popen(prefix + qemu, preexec_fn=_no_core_dumps)


def _no_core_dumps() -> None:
    """preexec hook: forbid core dumps from the qemu process (no guest RAM dumped
    to disk on a crash). Best-effort -- ignored if the platform lacks RLIMIT_CORE."""
    try:
        import resource
        resource.setrlimit(resource.RLIMIT_CORE, (0, 0))
    except (ImportError, ValueError, OSError):
        pass


def teardown(proc: subprocess.Popen) -> None:
    log("shutting down the guest ...")
    proc.terminate()
    try:
        proc.wait(timeout=10)
    except subprocess.TimeoutExpired:
        proc.kill()


def ensure_cloud_image(img: Path, img_url: str) -> None:
    if not img.exists():
        log(f"cloud image not found, downloading {img_url} ...")
        if run(["curl", "-LfsS", "-C", "-", "-o", str(img), img_url]).returncode != 0:
            die("cloud image download failed")


def ensure_seed(seed: Path) -> None:
    if not seed.exists():
        log(f"cloud-init seed not found, building {seed.name} (./make-seed.sh) ...")
        if run([str(HERE / "make-seed.sh")], cwd=HERE).returncode != 0:
            die("seed build failed")


def compile_c(srcs, out_name: str, image: str, is_local: bool,
              plat_args: list[str], cflags: list[str]) -> Path:
    """Compile all .c files in `srcs` together into one static binary named
    `out_name`, inside the build container; non-.c files (headers, data) are
    copied alongside so includes resolve. Returns the host path of the binary.

    Builds for the guest's architecture (plat_args is empty for native, or
    --platform for an emulated foreign arch), so the binary matches the kernel."""
    builddir = Path(tempfile.mkdtemp(prefix=".mk-build-", dir=HERE))
    cfiles = []
    for s in srcs:
        shutil.copy(s, builddir / Path(s).name)
        if Path(s).suffix == ".c":
            cfiles.append(Path(s).name)
    if not cfiles:
        shutil.rmtree(builddir, ignore_errors=True)
        die("no .c files to compile")

    mklib.ensure_pulled(image, is_local, plat_args)
    podman = ["podman", "run", "--rm", *plat_args]
    if is_local:
        podman += ["--pull=never"]
    podman += [
        *mklib.hardening_args(),
        "-v", mklib.volume(builddir, "/build"), "-w", "/build", image,
        "gcc-15", "-static", "-O2", "-pthread", *cflags,
        "-o", out_name, *cfiles,
    ]
    log(f"compiling {', '.join(cfiles)} statically in {image} ...")
    if run(podman).returncode != 0:
        shutil.rmtree(builddir, ignore_errors=True)
        die("static compilation failed")
    binary = builddir / out_name
    if not binary.exists():
        shutil.rmtree(builddir, ignore_errors=True)
        die("compiler produced no output binary")
    return binary
