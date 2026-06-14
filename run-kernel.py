#!/usr/bin/env python3
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
    accel, cpu = mklib.qemu_accel_cpu(arch)
    kimg = mklib.kernel_image(linux_src, arch)

    # Ubuntu cloud image (matches the target arch). Override IMG/IMG_URL otherwise.
    img = os.environ.get("IMG", prof["cloud_img"])
    img_url = os.environ.get(
        "IMG_URL", f"https://cloud-images.ubuntu.com/noble/current/{img}"
    )

    # Host port forwarded to the guest's SSH (22). ssh -p "$SSH_PORT" mac@127.0.0.1
    ssh_port = os.environ.get("SSH_PORT", "2222")
    seed = os.environ.get("SEED", "seed.iso")
    # Default: boot the real init (systemd) so cloud-init runs. INIT=/bin/bash skips it.
    init = os.environ.get("INIT", "")

    # Build the kernel if it hasn't been built yet.
    if not kimg.is_file():
        print("kernel Image not found, building it first...", flush=True)
        subprocess.run([sys.executable, str(HERE / "build-kernel.py")], check=True)

    # Download the cloud image if absent (resumable).
    if not Path(img).is_file():
        print(f"cloud image not found, downloading {img_url} ...", flush=True)
        subprocess.run(["curl", "-LfsS", "-C", "-", "-o", img, img_url], check=True)

    # Build the cloud-init seed (SSH key + DHCP network config) if absent. An
    # existing seed is reused as-is, so `rm seed.iso` after changing
    # GUEST_USER/SSH_KEY/GUEST_PASS.
    if not Path(seed).is_file():
        print(f"cloud-init seed not found, building {seed} ...", flush=True)
        subprocess.run([str(HERE / "make-seed.sh")], check=True)

    # Kernel cmdline. With the default (empty) INIT the cloud image boots systemd
    # -> cloud-init -> sshd. INIT=/bin/bash drops straight to a shell (no networking).
    append = f"console={prof['console']} root=/dev/vda1 rw"
    if init:
        append += f" init={init}"

    print(f"=== booting {kimg} ({arch}, accel={accel}) ===")
    print(
        f"    SSH:  ssh -p {ssh_port} -i id_mackernel mac@127.0.0.1"
        "   (after cloud-init finishes)"
    )
    print("    quit: Ctrl-a x", flush=True)
    os.execvp(
        prof["qemu_binary"],
        [
            prof["qemu_binary"],
            "-nographic",
            "-machine", prof["qemu_machine"],
            "-cpu", cpu, "-accel", accel,
            "-m", "2048", "-smp", "4",
            "-kernel", str(kimg),
            "-drive", f"file={img},if=virtio,format=qcow2",
            "-drive", f"file={seed},if=virtio,format=raw,readonly=on",
            "-netdev", f"user,id=net0,hostfwd=tcp::{ssh_port}-:22",
            "-device", "virtio-net-pci,netdev=net0",
            "-device", "virtio-rng-pci",
            "-append", append,
            "-snapshot",
        ],
    )


if __name__ == "__main__":
    sys.exit(main())
