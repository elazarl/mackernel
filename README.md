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

## Speed: give Podman all your CPUs

Podman on macOS runs Linux inside a VM, and containers see the *VM's* CPU count — not
the Mac's. `make -j$(nproc)` parallelizes to whatever the VM was given, so an
under-provisioned machine builds the kernel far slower than it could.

Check the gap:

```bash
sysctl -n hw.ncpu                                                          # Mac's logical CPUs
podman machine inspect podman-machine-default --format '{{.Resources.CPUs}}'  # VM's CPUs
```

If the VM has fewer, raise it (the machine must be **stopped** to change resources):

```bash
podman machine stop
podman machine set --cpus "$(sysctl -n hw.ncpu)"   # all cores; optionally --memory 16384
podman machine start
```

Verify the container now sees them all:

```bash
podman run --rm mackernel-build nproc              # should match sysctl -n hw.ncpu
```

Notes:
- `--cpus` can't exceed `hw.ncpu`. If the Mac feels sluggish during builds, leave a couple
  of cores for the host (e.g. `--cpus 12` on a 14-core machine).
- The setting is persistent across restarts. To bake it in at creation time:
  `podman machine init --cpus "$(sysctl -n hw.ncpu)" --memory 16384`.
- QEMU's `-smp` in `run-kernel.sh` is independent (QEMU runs on the host, not in the VM).

## Usage

```bash
./build-container.sh    # 1. build the latest-Ubuntu + gcc-15 build image (optional, see below)
./configure-kernel.sh   # 2. produce a minimal, bootable .config in ~/linux
./run-kernel.sh         # 3. download the cloud image if absent, build if needed, boot
```

Step 1 is **optional**: if you skip it, `configure-kernel.sh` / `build-kernel.sh` fall back to
the prebuilt multi-arch image published to GHCR (`ghcr.io/elazarl/mackernel`), which Podman
pulls automatically. Run `./build-container.sh` only when you want to build the image locally
(e.g. you changed the `Containerfile`).

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

## Prebuilt image (GHCR) & versioning

The `.github/workflows/build-push-image.yml` workflow builds the `Containerfile` as a
**multi-arch (amd64 + arm64)** image and pushes it to `ghcr.io/elazarl/mackernel` on every
change to `Containerfile` / `VERSION` (or via manual *Run workflow*). It tags the image with
both `latest` and the version string from the [`VERSION`](VERSION) file, then smoke-tests both
architectures in CI.

The scripts use this image automatically when no local `mackernel-build` image is present, so
a fresh clone needs only:

```bash
./configure-kernel.sh && ./run-kernel.sh   # pulls ghcr.io/elazarl/mackernel:<VERSION>
```

To bump the version, edit `VERSION` and push — the workflow publishes the new tag.

> **Package visibility:** the GHCR package must be **public** for the no-auth pull above to
> work. If it's private, either make it public once (repo → Packages → *Package settings* →
> *Change visibility*) or `podman login ghcr.io` first. Override the source entirely with
> `REMOTE_IMAGE=ghcr.io/<owner>/<image>:<tag>`.

## Configuration knobs (env vars)

| Var | Default | Notes |
|---|---|---|
| `LINUX_SRC` | `~/linux` | Kernel source tree (mounted into the container). |
| `ARCH` | `arm64` | Target architecture. |
| `IMAGE` | `mackernel-build` | Local Podman image tag. |
| `REMOTE_IMAGE` | `ghcr.io/elazarl/mackernel:<VERSION>` | Fallback image when no local one exists. |
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
