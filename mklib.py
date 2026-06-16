#!/usr/bin/env python3
"""Shared helpers for the mackernel Python scripts.

Single source of truth for the two axes that make the project portable:

  * target ARCH (arm64 / x86_64) -- decides the kernel config fragment, the
    kernel image target & path, the QEMU binary/machine/console, the cloud-image
    arch, and the container --platform.
  * host OS + whether the host arch matches the target -- decides the QEMU
    accelerator (hvf on a native Mac, kvm on native Linux, tcg when emulating a
    foreign arch) and thus the -cpu model.
"""
# Keep annotations lazy so `str | None` etc. parse on Python 3.9 (e.g. RHEL 9).
from __future__ import annotations

import os
import platform
import shutil
import subprocess
from pathlib import Path

HERE = Path(__file__).resolve().parent


# --- build image -----------------------------------------------------------

def resolve_image(want_arch: str | None = None) -> tuple[str, bool]:
    """Return (image_ref, is_local): prefer the locally-built image, otherwise
    fall back to the multi-arch GHCR image (podman pulls it automatically on
    first use). When want_arch is given and the local image is built for a
    different architecture, use the GHCR image instead -- the local single-arch
    image cannot satisfy a cross-arch `--platform` request."""
    try:
        version = (HERE / "VERSION").read_text().strip()
    except OSError:
        version = "latest"

    # Locally-built image tag (produced by build-container.sh).
    local_image = os.environ.get("IMAGE", "mackernel-build")
    # Prebuilt multi-arch image published to GHCR by CI. Override REMOTE_IMAGE
    # to point elsewhere (e.g. a fork).
    remote_image = os.environ.get(
        "REMOTE_IMAGE", f"ghcr.io/elazarl/mackernel:{version}"
    )

    exists = subprocess.run(
        ["podman", "image", "exists", local_image],
        stderr=subprocess.DEVNULL,
    ).returncode == 0
    if not exists:
        return remote_image, False
    if want_arch is not None and _local_image_arch(local_image) != normalize_arch(want_arch):
        return remote_image, False
    return local_image, True


def _local_image_arch(image: str) -> str | None:
    """Architecture a local image was built for ('arm64'/'x86_64'), or None."""
    out = subprocess.run(
        ["podman", "image", "inspect", image, "--format", "{{.Architecture}}"],
        capture_output=True, text=True,
    )
    if out.returncode != 0 or not out.stdout.strip():
        return None
    return normalize_arch(out.stdout.strip())


# --- host / arch detection -------------------------------------------------

def normalize_arch(value: str) -> str:
    """Map the many spellings of an architecture to 'arm64' or 'x86_64'."""
    v = value.lower()
    if v in ("arm64", "aarch64"):
        return "arm64"
    if v in ("x86_64", "amd64", "x64"):
        return "x86_64"
    raise SystemExit(f"unsupported architecture: {value!r} (want arm64 or x86_64)")


def host_os() -> str:
    """'mac' or 'linux'."""
    return "mac" if platform.system() == "Darwin" else "linux"


def host_arch() -> str:
    return normalize_arch(platform.machine())


def target_arch() -> str:
    """The kernel target arch: ARCH env override, else the host's arch."""
    return normalize_arch(os.environ.get("ARCH", host_arch()))


# --- per-arch profile ------------------------------------------------------

_PROFILES = {
    "arm64": {
        "kernel_arch": "arm64",
        "arch_dir": "arch/arm64",
        "image_name": "Image",
        "container_platform": "linux/arm64",
        "qemu_binary": "qemu-system-aarch64",
        "qemu_machine": "virt,gic-version=3",
        "console": "ttyAMA0",
        "cloud_img": "noble-server-cloudimg-arm64.img",
    },
    "x86_64": {
        "kernel_arch": "x86_64",
        "arch_dir": "arch/x86",
        "image_name": "bzImage",
        "container_platform": "linux/amd64",
        "qemu_binary": "qemu-system-x86_64",
        "qemu_machine": "q35",
        "console": "ttyS0",
        "cloud_img": "noble-server-cloudimg-amd64.img",
    },
}


def arch_profile(arch: str) -> dict:
    """Per-arch settings. 'image_path' is relative to the kernel source tree."""
    p = dict(_PROFILES[normalize_arch(arch)])
    p["image_path"] = f"{p['arch_dir']}/boot/{p['image_name']}"
    return p


def build_dir() -> str | None:
    """Optional out-of-tree build directory (env BUILD_DIR). When set, the kernel
    is built there via `make O=`, leaving the source tree clean -- which also lets
    one source tree build several arches (point BUILD_DIR at a per-arch dir).
    When unset, the build is in-tree. On macOS the dir must be under $HOME so the
    podman-machine VM can mount it."""
    return os.environ.get("BUILD_DIR") or None


def output_root(linux_src) -> str:
    """Where build outputs (.config, kernel image) live: BUILD_DIR if set, else
    the source tree (in-tree build)."""
    return build_dir() or str(linux_src)


def config_path(linux_src) -> Path:
    return Path(output_root(linux_src)) / ".config"


def build_dir_mount() -> tuple[list[str], str]:
    """Return (podman -v args, container `make O=` path) for the optional
    BUILD_DIR. Empty when BUILD_DIR is unset (in-tree build). When set, the host
    dir is created if needed and mounted at /out."""
    bd = build_dir()
    if not bd:
        return [], ""
    Path(bd).mkdir(parents=True, exist_ok=True)
    return ["-v", volume(Path(bd).resolve(), "/out")], "/out"


def kernel_image(linux_src, arch: str) -> Path:
    """Absolute path to the built kernel image for `arch`.

    With BUILD_DIR unset the build is in-tree; a single tree then holds one
    host-arch build at a time (the kernel's host tools are arch-specific), so a
    non-host arch needs a separate LINUX_SRC or its own BUILD_DIR."""
    return Path(output_root(linux_src)) / arch_profile(arch)["image_path"]


def platform_args(arch: str) -> list[str]:
    """podman `--platform` args. Only needed when building for a foreign arch
    (emulated); for a native build the local image is already the right platform
    and passing --platform would force podman to re-resolve it via the registry."""
    if normalize_arch(arch) == host_arch():
        return []
    return ["--platform", arch_profile(arch)["container_platform"]]


_SELINUX_ENFORCING = None


def selinux_enforcing() -> bool:
    """True on a host with SELinux in enforcing mode (RHEL/Fedora). Cached."""
    global _SELINUX_ENFORCING
    if _SELINUX_ENFORCING is None:
        try:
            _SELINUX_ENFORCING = Path("/sys/fs/selinux/enforce").read_text().strip() == "1"
        except OSError:
            _SELINUX_ENFORCING = False
    return _SELINUX_ENFORCING


def volume(host, container: str, *, ro: bool = False) -> str:
    """Build a podman `-v` value. Adds the `:z` relabel suffix when SELinux is
    enforcing so a rootless container can access the bind mount (without it RHEL
    denies access -- `stat: Permission denied`). `:z` is a no-op on non-SELinux
    hosts, so this stays cross-platform."""
    opts = ["ro"] if ro else []
    if selinux_enforcing():
        opts.append("z")
    return f"{host}:{container}" + ((":" + ",".join(opts)) if opts else "")


def hardening_args() -> list[str]:
    """podman flags for least-privilege build/compile containers. Compilation
    needs no network and no privilege escalation, and can run on a read-only root
    filesystem -- the work dirs (kernel tree, build/seed scratch) are bind-mounted
    read-write separately, so builds still write their outputs. /tmp and /run are
    writable tmpfs for the toolchain's scratch.

    All Linux capabilities are dropped except DAC_OVERRIDE: under rootless podman
    on macOS the bind-mounted tree is presented with a uid the container doesn't
    match, so the build needs DAC_OVERRIDE to read/write it (without it even
    `stat Makefile` is denied). That one cap is benign -- it only bypasses file
    permission bits inside our own mounts."""
    return [
        "--network=none",
        "--cap-drop=all",
        "--cap-add=dac_override",
        "--security-opt", "no-new-privileges",
        "--read-only",
        "--tmpfs", "/tmp:rw,exec",
        "--tmpfs", "/run:rw",
    ]


def qemu_hardening_args() -> list[str]:
    """Least-surface flags for the qemu-system process (the VM-boot step), to
    complement the locked-down build containers.

    Cross-platform: ignore host/user qemu config files and create no implicit
    devices (boot_qemu adds every device it needs explicitly), shrinking the
    emulated attack surface. On Linux, also turn on QEMU's seccomp sandbox to
    whitelist host syscalls and deny privilege escalation / process spawning /
    resource control. macOS has no seccomp, so `-sandbox` is omitted there -- the
    HVF hardware-VM boundary plus an optional Seatbelt (`sandbox-exec`) profile
    around the process cover that host (see docs/qemu-hardening notes)."""
    args = ["-no-user-config", "-nodefaults"]
    if host_os() == "linux":
        args += ["-sandbox",
                 "on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny"]
    return args


def ensure_pulled(ref: str, is_local: bool, plat_args: list[str]) -> None:
    """Pull a remote image up front so a later hardened (--network=none) run finds
    it locally -- a network-less container cannot pull on demand. No-op for the
    already-present local image."""
    if is_local:
        return
    subprocess.run(["podman", "pull", *plat_args, ref], check=True)


def qemu_binary(arch: str) -> str:
    """The qemu-system binary to launch. The QEMU env var overrides everything;
    otherwise use the per-arch default name if it's on PATH; otherwise fall back
    to RHEL/Fedora's qemu-kvm (shipped at /usr/libexec/qemu-kvm, not as
    qemu-system-x86_64) for x86_64. Returns the default name as a last resort so
    the failure (and the fix: install qemu or set QEMU=) is clear at exec time."""
    env = os.environ.get("QEMU")
    if env:
        return env
    name = arch_profile(arch)["qemu_binary"]
    if shutil.which(name):
        return name
    if normalize_arch(arch) == "x86_64":
        for cand in ("/usr/libexec/qemu-kvm", "/usr/bin/qemu-kvm"):
            if os.path.exists(cand):
                return cand
    return name


def qemu_accel_cpu(arch: str) -> tuple[str, str]:
    """Return (accel, cpu). Hardware acceleration only when the target arch
    matches the host arch; otherwise QEMU emulates with TCG (slow but works)."""
    if normalize_arch(arch) == host_arch():
        accel = "hvf" if host_os() == "mac" else "kvm"
        return accel, "host"
    return "tcg", "max"
