# mackernel

Build a minimal, bootable Linux kernel inside a Podman container (latest Ubuntu +
**gcc-15**) and boot it against an Ubuntu cloud image with QEMU — entirely on an
**Apple Silicon Mac, no sudo required**.

The goal is a fast edit-compile-run cycle: boot to a userspace shell in well under a
second thanks to HVF hardware acceleration.

This targets **arm64** (Apple Silicon). The kernel is built *natively* in the container
(no cross-compiler), and QEMU runs the guest with the `hvf` accelerator at near-native speed.

> **You don't have to build the container image.** A prebuilt, multi-arch (amd64 + arm64)
> image is published publicly at `ghcr.io/elazarl/mackernel` by CI. The scripts pull it
> automatically when no local image exists — no `build-container.sh`, no GHCR login needed.
> A fresh clone goes straight to:
> ```bash
> ./configure-kernel.sh && ./run-kernel.sh
> ```
> Build the image yourself only if you've changed the `Containerfile`.

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

By default `run-kernel.sh` boots the cloud image's real init (systemd) so **cloud-init** runs,
brings up the network, and starts `sshd` — see [Networking](#networking-ssh-in-from-the-mac).
The QEMU serial stays attached to your terminal (quit with `Ctrl-a x`).

Want the old straight-to-shell behaviour (no networking / cloud-init)? Boot with
`INIT=/bin/bash ./run-kernel.sh` and you'll land in `root@(none):/#`.

### Sanitizers & extra Kconfig

`configure-kernel.sh` takes flags to layer debugging options onto the minimal config —
each one is off by default, and `olddefconfig` resolves whatever they depend on:

```bash
./configure-kernel.sh --kasan               # Kernel Address Sanitizer (use-after-free / OOB)
./configure-kernel.sh --all-sanitizers       # KASAN + UBSAN + KFENCE + lockdep + kmemleak
./configure-kernel.sh --kasan --atalk        # the AppleTalk DDP race reproducer
./configure-kernel.sh -e NET_9P -e 9P_FS      # arbitrary scripts/config tokens
```

Flags: `--kasan` / `--kasan-inline`, `--kfence`, `--kcsan`, `--ubsan`, `--kmemleak`,
`--lockdep`, `--all-sanitizers`, `--atalk`. Anything else (`-e`/`-d`/`-m SYM`,
`--set-str`/`--set-val K V`, or a raw token) is passed straight to `scripts/config`.
Heap sanitizers automatically disable `SLUB_TINY`. The `EXTRA_CONFIG` env var still works
and is applied first, so CLI flags win over it. Run `./configure-kernel.sh --help` for the
full list.

### Run a C program inside the kernel

`run-in-kernel.py` is the fast path for "does this reproduce in *my* kernel?": it compiles a
C file **fully statically** in the build container (so it has no libc dependency in the guest),
boots the kernel, copies the binary in over SSH, runs it, and streams the output back —
tearing the guest down afterwards (disk writes are discarded via `-snapshot`).

```bash
./run-in-kernel.py repro.c                      # build, boot, run as the 'mac' user
./run-in-kernel.py --sudo repro.c               # run as root (raw sockets, AF_APPLETALK, …)
./run-in-kernel.py repro.c -- --threads 8       # pass args after -- to the guest program
./run-in-kernel.py -o ./repro repro.c           # also keep the static binary on the host
```

The program's exit status becomes the script's exit status. It builds the kernel / fetches the
cloud image / makes the seed automatically if any are missing, picks a free SSH port (so it
won't clash with a `run-kernel.sh` already on 2222), and writes the guest serial console to
`run-in-kernel-boot.log` for post-mortem. Same env knobs as the other scripts
(`LINUX_SRC`, `ARCH`, `IMG`, `SSH_KEY`, `GUEST_USER`, …); see `./run-in-kernel.py --help`.

## Networking: SSH in from the Mac

`run-kernel.sh` gives the guest a `virtio-net` NIC on QEMU's **user-mode (slirp)** networking —
no `sudo`, no host bridge. A host port is forwarded to the guest's SSH, and a cloud-init seed
DHCPs the NIC and installs your SSH key, so the Mac can connect to the guest:

```bash
./run-kernel.sh                                   # boots; cloud-init runs (~10–30s the first time)
# in another terminal, once cloud-init has finished:
ssh -p 2222 -i id_mackernel mac@127.0.0.1         # password: mackernel
```

How it fits together:

- **Kernel** (`configure-kernel.sh`): adds `VIRTIO_NET` + the TCP/IP stack, plus the options
  `tinyconfig` strips that systemd needs (cgroups, the `*fd`/inotify/epoll syscalls, namespaces,
  tmpfs, …) and `ISO9660`/`JOLIET` so the guest can mount the seed. `VIRTIO_RNG` feeds entropy so
  sshd host-key generation doesn't stall.
- **Seed** (`make-seed.sh`): builds `seed.iso`, a cloud-init *NoCloud* disk (ISO9660+Joliet,
  volume label `CIDATA`) made with macOS's own `hdiutil` — no `genisoimage`/`cloud-localds`
  needed. It carries `user-data` (login user `mac` + your SSH key + passwordless sudo),
  `meta-data`, and a v2 `network-config` (DHCP). A passphrase-less keypair (`id_mackernel`) is
  minted on first run. `run-kernel.sh` builds the seed automatically if it's missing.
- **QEMU** (`run-kernel.sh`): `-netdev user,hostfwd=tcp::2222-:22` + `-device virtio-net-pci`
  forwards host `127.0.0.1:2222` → guest `:22`; `-device virtio-rng-pci` supplies entropy.

The guest gets `10.0.2.15` from slirp's built-in DHCP and reaches the Mac at `10.0.2.2`. Because
slirp is NAT, you reach the guest *through the forwarded port* (`127.0.0.1:2222`), not its
`10.0.2.x` address. Forward more ports by adding `hostfwd=` clauses to `run-kernel.sh`.

## What each file does

| File | Role |
|---|---|
| `Containerfile` | Latest Ubuntu image with `gcc-15` (default `cc`) + kernel build deps. |
| `build-container.sh` | Builds the image as `mackernel-build`, verifies `gcc-15` is present. |
| `configure-kernel.sh` | `make tinyconfig` + the minimal option set needed to boot (incl. virtio-net + the bits systemd/cloud-init need). |
| `build-kernel.sh` | Compiles with `CC=gcc-15 HOSTCC=gcc-15` → `arch/arm64/boot/Image`. |
| `make-seed.sh` | Builds a cloud-init NoCloud seed (`seed.iso`) that DHCPs the NIC + installs an SSH key. |
| `run-kernel.sh` | Downloads the cloud image if missing, builds if missing, boots with QEMU/HVF + networking. |
| `run-in-kernel.py` | Compiles a C file *statically* in the container, boots the kernel, and runs the binary in the guest over SSH. |

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

> **Package visibility:** the GHCR package is **public**, so the pull above needs no
> authentication. (If you fork this and your package is private, either make it public —
> repo → Packages → *Package settings* → *Change visibility* — or `podman login ghcr.io`
> first.) Override the source entirely with `REMOTE_IMAGE=ghcr.io/<owner>/<image>:<tag>`.

## Configuration knobs (env vars)

| Var | Default | Notes |
|---|---|---|
| `LINUX_SRC` | `~/linux` | Kernel source tree (mounted into the container). |
| `ARCH` | `arm64` | Target architecture. |
| `EXTRA_CONFIG` | _(empty)_ | Extra `scripts/config` args (applied before CLI flags), e.g. `EXTRA_CONFIG="-e NET_9P -e 9P_FS"`. See also `configure-kernel.sh --help`. |
| `IMAGE` | `mackernel-build` | Local Podman image tag. |
| `REMOTE_IMAGE` | `ghcr.io/elazarl/mackernel:<VERSION>` | Fallback image when no local one exists. |
| `IMG` / `IMG_URL` | Ubuntu Noble arm64 | Cloud image filename / download URL. |
| `SSH_PORT` | `2222` | Host port forwarded to the guest's SSH (`ssh -p $SSH_PORT mac@127.0.0.1`). |
| `INIT` | _(empty)_ | Empty → boot systemd + cloud-init (networking). `/bin/bash` → straight-to-shell, no net. |
| `SEED` | `seed.iso` | cloud-init NoCloud seed disk (built by `make-seed.sh` if absent). |
| `SSH_KEY` | `id_mackernel` | Host SSH private key injected into the guest (generated if absent). |
| `GUEST_USER` / `GUEST_PASS` | `mac` / `mackernel` | Login user + password created by cloud-init. |

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
