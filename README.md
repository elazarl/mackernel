# mackernel

Build a minimal, bootable Linux kernel inside a Podman container (latest Ubuntu +
**gcc-15**) and boot it against an Ubuntu cloud image with QEMU — entirely on an
**Apple Silicon Mac, no sudo required**.

The goal is a fast edit-compile-run cycle: boot to a userspace shell in well under a
second thanks to HVF hardware acceleration.

This targets **arm64** (Apple Silicon). The kernel is built *natively* in the container
(no cross-compiler), and QEMU runs the guest with the `hvf` accelerator at near-native speed.

## Prerequisites

```bash
brew install qemu podman
podman machine init    # if you don't already have a machine
podman machine start
```

You also need a Linux kernel source tree. By default the scripts use `~/linux`:

```bash
git clone --depth=1 https://github.com/torvalds/linux.git ~/linux
```

## Usage

```bash
./build-container.sh    # 1. build the latest-Ubuntu + gcc-15 build image
./configure-kernel.sh   # 2. produce a minimal, bootable .config in ~/linux
./run-kernel.sh         # 3. download the cloud image if absent, build if needed, boot
```

`run-kernel.sh` builds the kernel automatically (via `build-kernel.sh`) if the `Image`
isn't there yet, so the day-to-day loop is just:

```bash
# edit ~/linux ...
./build-kernel.sh && ./run-kernel.sh
```

You'll land in `root@(none):/#` with the cloud image's userspace. Quit QEMU with `Ctrl-a x`.

## What each file does

| File | Role |
|---|---|
| `Containerfile` | Latest Ubuntu image with `gcc-15` (default `cc`) + kernel build deps. |
| `build-container.sh` | Builds the image as `mackernel-build`, verifies `gcc-15` is present. |
| `configure-kernel.sh` | `make tinyconfig` + the minimal option set needed to boot. |
| `build-kernel.sh` | Compiles with `CC=gcc-15 HOSTCC=gcc-15` → `arch/arm64/boot/Image`. |
| `run-kernel.sh` | Downloads the cloud image if missing, builds if missing, boots with QEMU/HVF. |

## Configuration knobs (env vars)

| Var | Default | Notes |
|---|---|---|
| `LINUX_SRC` | `~/linux` | Kernel source tree (mounted into the container). |
| `ARCH` | `arm64` | Target architecture. |
| `IMAGE` | `mackernel-build` | Podman image tag. |
| `IMG` / `IMG_URL` | Ubuntu Noble arm64 | Cloud image filename / download URL. |

## Notes

- **gcc-15 on latest Ubuntu:** if `gcc-15` isn't in the default repos for the Ubuntu
  release `ubuntu:latest` resolves to, uncomment the `ppa:ubuntu-toolchain-r/test` lines
  near the top of the `Containerfile`. `build-container.sh` fails loudly if `gcc-15` is missing.
- **Why `PCI_HOST_GENERIC`:** QEMU's arm64 `virt` machine exposes virtio-blk over a generic
  ECAM PCIe host bridge. `tinyconfig` omits that driver, so without it the PCI bus is never
  enumerated, the disk never appears, and the kernel panics with "Unable to mount root fs".
- **arm64 console is `ttyAMA0`** (PL011 UART), not `ttyS0`.
- `run-kernel.sh` uses `-snapshot`, so writes are discarded on exit and the cloud image stays
  pristine and reusable.
