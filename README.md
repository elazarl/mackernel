# mackernel

Build a minimal, bootable Linux kernel inside a Podman container (latest Ubuntu +
**gcc-15**) and boot it against an Ubuntu cloud image with QEMU — **no sudo required**,
on **macOS or Linux**, targeting **arm64 or x86_64**.

The goal is a fast edit-compile-run cycle: boot to a userspace shell in well under a
second when the guest arch matches the host (hardware acceleration).

**Host/target matrix.** The target architecture defaults to the host's; set `ARCH`
to cross-target. When the target matches the host arch the build is native and QEMU
uses hardware acceleration (`hvf` on macOS, `kvm` on Linux). When they differ
(e.g. `ARCH=x86_64` on an Apple Silicon Mac) both the build and the boot run under
**emulation** — the build uses the matching-arch container (the multi-arch GHCR
image), and QEMU falls back to TCG. Emulation works but is much slower.

| Host \ Target | arm64 | x86_64 |
|---|---|---|
| macOS arm64 | native, hvf | emulated, tcg |
| Linux arm64 | native, kvm | emulated, tcg |
| Linux x86_64 | emulated, tcg | native, kvm |

> **You don't have to build the container image.** A prebuilt, multi-arch (amd64 + arm64)
> image is published publicly at `ghcr.io/elazarl/mackernel` by CI. The scripts pull it
> automatically when no local image exists — no `build-container.sh`, no GHCR login needed.
> A fresh clone goes straight to:
> ```bash
> ./configure-kernel.py && ./run-kernel.py
> ```
> Build the image yourself only if you've changed the `Containerfile`.

## Prerequisites

QEMU + Podman, plus Python 3 (for the `*.py` scripts). On **macOS**:

```bash
brew install qemu podman
podman machine init    # if you don't already have a machine
podman machine start
```

On **Linux**, install from your distro and you're done — Podman runs natively, so
there's no `podman machine` step. You also need a tool to build the cloud-init seed
ISO (`xorriso` or `genisoimage`):

```bash
sudo apt install qemu-system podman xorriso        # Debian/Ubuntu
sudo dnf install qemu-kvm podman xorriso           # Fedora/RHEL
```

(macOS builds the seed with the bundled `hdiutil`, so no extra package there.)

You also need a Linux kernel source tree. By default the scripts use `~/linux`:

```bash
git clone --depth=1 https://github.com/torvalds/linux.git ~/linux
```

## Speed: give Podman all your CPUs (macOS only)

On Linux, Podman runs containers natively and `make -j$(nproc)` already uses every
core — skip this section. It applies only to **macOS**, where Podman runs Linux
inside a VM and containers see the *VM's* CPU count — not the Mac's. `make -j$(nproc)`
parallelizes to whatever the VM was given, so an under-provisioned machine builds the
kernel far slower than it could.

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
- QEMU's `-smp` in `run-kernel.py` is independent (QEMU runs on the host, not in the VM).

## Usage

```bash
./build-container.sh    # 1. build the latest-Ubuntu + gcc-15 build image (optional, see below)
./configure-kernel.py   # 2. produce a minimal, bootable .config in ~/linux
./run-kernel.py         # 3. download the cloud image if absent, build if needed, boot
```

Step 1 is **optional**: if you skip it, `configure-kernel.py` / `build-kernel.py` fall back to
the prebuilt multi-arch image published to GHCR (`ghcr.io/elazarl/mackernel`), which Podman
pulls automatically. Run `./build-container.sh` only when you want to build the image locally
(e.g. you changed the `Containerfile`).

`run-kernel.py` builds the kernel automatically (via `build-kernel.py`) if the `Image`
isn't there yet, so the day-to-day loop is just:

```bash
# edit ~/linux ...
./build-kernel.py && ./run-kernel.py
```

By default `run-kernel.py` boots the cloud image's real init (systemd) so **cloud-init** runs,
brings up the network, and starts `sshd` — see [Networking](#networking-ssh-in-from-the-mac).
The QEMU serial stays attached to your terminal (quit with `Ctrl-a x`).

Want the old straight-to-shell behaviour (no networking / cloud-init)? Boot with
`INIT=/bin/bash ./run-kernel.py` and you'll land in `root@(none):/#`.

### Targeting a different architecture

All four scripts read `ARCH` (default: the host's arch — `arm64` or `x86_64`). Set it
to cross-target; the same value flows through configure, build, and boot:

```bash
ARCH=x86_64 ./configure-kernel.py && ARCH=x86_64 ./run-kernel.py   # x86_64 guest
```

The kernel config comes from `kconf/base.config` (arch-independent) merged with
`kconf/$ARCH.config` (the platform drivers — serial console, interrupt controller,
timer, PCI host bridge). When the target differs from the host arch, the build runs in
the matching-arch container under emulation and QEMU boots with TCG — correct, but
slow (minutes rather than seconds).

Set **`BUILD_DIR`** to build out-of-tree (kernel `make O=`): the source tree stays
clean and each arch can have its own output dir, so one checkout can build both arms
of the matrix without clobbering. All steps (configure, build, boot) read their
outputs — `.config`, the kernel image — from there:

```bash
# one source tree, two arches, no mrproper dance:
BUILD_DIR=~/mk/arm64   ARCH=arm64   ./configure-kernel.py && BUILD_DIR=~/mk/arm64   ./run-kernel.py
BUILD_DIR=~/mk/x86_64  ARCH=x86_64  ./configure-kernel.py && BUILD_DIR=~/mk/x86_64  ./run-kernel.py
```

(On macOS, `BUILD_DIR` must live under `$HOME` so the podman-machine VM can mount it.
`make O=` needs the *source* tree clean, so use `BUILD_DIR` from the start, or
`make mrproper` a tree that was previously built in-tree.)

### Sanitizers & extra Kconfig

`configure-kernel.py` takes flags to layer debugging options onto the minimal config —
each one is off by default, and `olddefconfig` resolves whatever they depend on:

```bash
./configure-kernel.py --kasan               # Kernel Address Sanitizer (use-after-free / OOB)
./configure-kernel.py --all-sanitizers       # KASAN + UBSAN + KFENCE + lockdep + kmemleak
./configure-kernel.py --kasan --atalk        # the AppleTalk DDP race reproducer
./configure-kernel.py -e NET_9P -e 9P_FS      # arbitrary scripts/config tokens
```

Flags: `--kasan` / `--kasan-inline`, `--kfence`, `--kcsan`, `--ubsan`, `--kmemleak`,
`--lockdep`, `--all-sanitizers`, `--atalk`. Anything else (`-e`/`-d`/`-m SYM`,
`--set-str`/`--set-val K V`, or a raw token) is passed straight to `scripts/config`.
Heap sanitizers automatically disable `SLUB_TINY`. The `EXTRA_CONFIG` env var still works
and is applied first, so CLI flags win over it. Run `./configure-kernel.py --help` for the
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
won't clash with a `run-kernel.py` already on 2222), and writes the guest serial console to
`run-in-kernel-boot.log` for post-mortem. Same env knobs as the other scripts
(`LINUX_SRC`, `ARCH`, `IMG`, `SSH_KEY`, `GUEST_USER`, …); see `./run-in-kernel.py --help`.

### Run a bundle file (kernel + userspace + module + script)

For a richer, *shareable* repro, give `run-kernel.py` a **bundle** file — a single
Markdown/text file describing the whole thing. `run-kernel.py` builds it, boots, runs it
in the guest, streams the output, and exits with the guest's status:

```bash
./run-kernel.py examples/greeter.md                          # local file
./run-kernel.py https://lore.kernel.org/all/<msgid>/         # lkml message
./run-kernel.py https://gist.github.com/<user>/<id>          # gist
```

The bundle can be a local file or an **http(s) URL** — lkml (`lore.kernel.org`) and
`gist.github.com` page URLs are fetched in their raw form automatically, so you can point
it straight at a reproducer someone posted.

The file has an optional `---`-delimited metadata block (it may appear **anywhere**, so a
patch-set cover letter works as a source) plus fenced code blocks tagged `role:filename`:

````markdown
---
url: git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git   # optional remote
commit: v6.12                                                            # optional treeish
patch: https://example.com/series.patch                                  # optional patch URL
arch: x86_64                                                             # optional; else native
---

```user:file.c
#include "file.h"
int main(void) { return R; }     // compiled statically; one binary
```
```user:file.h
#define R 1
```
```module:greeter.c
... module_init(...) ...          // built as a loadable .ko, sudo-insmod'd
```
```kconf:extra.config
CONFIG_PRINTK_CALLER=y            // merged into the kernel config
```
```init:init.sh
#!/bin/bash
./file                            // start script; runs in the guest
```
````

Roles and defaults:
- **`user:`** — C sources + headers, compiled together into one static binary. A single `.c`
  is named after its stem (so `file.c` → `./file`); multiple `.c` are named after the bundle.
- **`module:`** — each `.c` is built as its own loadable `.ko` and `sudo insmod`'d in order
  (the kernel is built with `CONFIG_MODULES`).
- **`kconf:`** — Kconfig fragment lines merged on top of `kconf/base.config` + the per-arch
  fragment; a bundle with `kconf:` blocks reconfigures + rebuilds.
- **`init:`** — the start script. If present it runs (cwd = the guest staging dir); otherwise
  the user binary runs; a module-only bundle dumps `dmesg` so `module_init` output shows.

**Kernel source:** with no metadata the bundle builds `LINUX_SRC` (`~/linux`). With `url`,
that remote is added to the tree and fetched; `commit` is checked out into a **cached git
worktree** (`~/linux-wt/<commit>`, reused across runs) and `patch` is applied there — so a
bundle is self-contained about which kernel it targets without disturbing your main tree.
Serial console goes to `run-kernel-boot.log`.

## Networking: SSH in from the Mac

`run-kernel.py` gives the guest a `virtio-net` NIC on QEMU's **user-mode (slirp)** networking —
no `sudo`, no host bridge. A host port is forwarded to the guest's SSH, and a cloud-init seed
DHCPs the NIC and installs your SSH key, so the Mac can connect to the guest:

```bash
./run-kernel.py                                   # boots; cloud-init runs (~10–30s the first time)
# in another terminal, once cloud-init has finished:
ssh -p 2222 -i id_mackernel mac@127.0.0.1         # password: mackernel
```

How it fits together:

- **Kernel** (`configure-kernel.py` + `kconf/`): `kconf/base.config` adds `VIRTIO_NET` + the
  TCP/IP stack, plus the options `tinyconfig` strips that systemd needs (cgroups, the
  `*fd`/inotify/epoll syscalls, namespaces, tmpfs, …) and `ISO9660`/`JOLIET` so the guest can
  mount the seed; `VIRTIO_RNG` feeds entropy so sshd host-key generation doesn't stall. The
  per-arch `kconf/<arch>.config` adds the platform drivers (serial console, interrupt
  controller, timer, PCI host bridge).
- **Seed** (`make-seed.sh`): builds `seed.iso`, a cloud-init *NoCloud* disk (ISO9660+Joliet,
  volume label `CIDATA`) made with macOS's `hdiutil` or Linux's `xorriso`/`genisoimage`.
  It carries `user-data` (login user `mac` + your SSH key + passwordless sudo),
  `meta-data`, and a v2 `network-config` (DHCP). A passphrase-less keypair (`id_mackernel`) is
  minted on first run. `run-kernel.py` builds the seed automatically if it's missing.
- **QEMU** (`run-kernel.py`): `-netdev user,hostfwd=tcp::2222-:22` + `-device virtio-net-pci`
  forwards host `127.0.0.1:2222` → guest `:22`; `-device virtio-rng-pci` supplies entropy.

The guest gets `10.0.2.15` from slirp's built-in DHCP and reaches the Mac at `10.0.2.2`. Because
slirp is NAT, you reach the guest *through the forwarded port* (`127.0.0.1:2222`), not its
`10.0.2.x` address. Forward more ports by adding `hostfwd=` clauses to `run-kernel.py`.

## What each file does

| File | Role |
|---|---|
| `Containerfile` | Latest Ubuntu image with `gcc-15` (default `cc`) + kernel build deps. |
| `build-container.sh` | Builds the image as `mackernel-build`, verifies `gcc-15` is present. |
| `mklib.py` | Shared helpers: host/arch detection, per-arch QEMU + image settings, build-image resolution. |
| `guestlib.py` | Shared guest engine: boot QEMU, wait for SSH, scp/run in the guest, static compile, teardown. |
| `kconf/` | Kconfig fragments: `base.config` (arch-independent) + `arm64.config` / `x86_64.config` (platform drivers). |
| `configure-kernel.py` | `make tinyconfig` + merge `kconf/` fragments (base + per-arch + `--fragment`) into a bootable `.config`. |
| `build-kernel.py` | Compiles with `CC=gcc-15 HOSTCC=gcc-15` → `arch/<arch>/boot/{Image,bzImage}`. |
| `make-seed.sh` | Builds a cloud-init NoCloud seed (`seed.iso`) that DHCPs the NIC + installs an SSH key. |
| `run-kernel.py` | Boots with QEMU (HVF/KVM/TCG) interactively, **or** builds + runs a bundle file (see above). |
| `run-in-kernel.py` | Compiles a C file *statically* in the container, boots the kernel, and runs the binary in the guest over SSH. |
| `examples/greeter.md` | Example bundle: userspace binary + kernel module + start script. |

## Prebuilt image (GHCR) & versioning

The `.github/workflows/build-push-image.yml` workflow builds the `Containerfile` as a
**multi-arch (amd64 + arm64)** image and pushes it to `ghcr.io/elazarl/mackernel` on every
change to `Containerfile` / `VERSION` (or via manual *Run workflow*). It tags the image with
both `latest` and the version string from the [`VERSION`](VERSION) file, then smoke-tests both
architectures in CI.

The scripts use this image automatically when no local `mackernel-build` image is present, so
a fresh clone needs only:

```bash
./configure-kernel.py && ./run-kernel.py   # pulls ghcr.io/elazarl/mackernel:<VERSION>
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
| `BUILD_DIR` | _(in-tree)_ | Out-of-tree build dir (`make O=`); outputs go here. macOS: keep it under `$HOME`. |
| `ARCH` | _host arch_ | Target architecture (`arm64` or `x86_64`). Defaults to the host's. |
| `EXTRA_CONFIG` | _(empty)_ | Extra `scripts/config` args (applied before CLI flags), e.g. `EXTRA_CONFIG="-e NET_9P -e 9P_FS"`. See also `configure-kernel.py --help`. |
| `IMAGE` | `mackernel-build` | Local Podman image tag. |
| `REMOTE_IMAGE` | `ghcr.io/elazarl/mackernel:<VERSION>` | Fallback image when no local one exists. |
| `IMG` / `IMG_URL` | Ubuntu Noble (matches `ARCH`) | Cloud image filename / download URL. |
| `SSH_PORT` | `2222` | Host port forwarded to the guest's SSH (`ssh -p $SSH_PORT mac@127.0.0.1`). |
| `INIT` | _(empty)_ | Empty → boot systemd + cloud-init (networking). `/bin/bash` → straight-to-shell, no net. |
| `SEED` | `seed.iso` | cloud-init NoCloud seed disk (built by `make-seed.sh` if absent). |
| `SSH_KEY` | `id_mackernel` | Host SSH private key injected into the guest (generated if absent). |
| `GUEST_USER` / `GUEST_PASS` | `mac` / `mackernel` | Login user + password created by cloud-init. |

## Notes

- **gcc-15 on latest Ubuntu:** if `gcc-15` isn't in the default repos for the Ubuntu
  release `ubuntu:latest` resolves to, uncomment the `ppa:ubuntu-toolchain-r/test` lines
  near the top of the `Containerfile`. `build-container.sh` fails loudly if `gcc-15` is missing.
- **Why `PCI_HOST_GENERIC` (arm64):** QEMU's arm64 `virt` machine exposes virtio-blk over a
  generic ECAM PCIe host bridge. `tinyconfig` omits that driver, so without it the PCI bus is
  never enumerated, the disk never appears, and the kernel panics with "Unable to mount root
  fs". It lives in `kconf/arm64.config`; x86_64's q35 uses `PCI_MMCONFIG` (`kconf/x86_64.config`).
- **Serial console differs by arch:** arm64 is `ttyAMA0` (PL011 UART); x86_64 is `ttyS0`
  (8250). `run-kernel.py` sets the right `console=` automatically.
- **Cross-arch builds in one tree:** a kernel source tree holds one host-arch build at a time
  (its host tools — `mk_elfconfig` etc. — are arch-specific), so an *in-tree* build of a
  non-host arch in a tree already built for another fails (e.g. `Error 127`). Either set
  `BUILD_DIR` to a per-arch output dir (recommended — keeps the source clean), point `LINUX_SRC`
  at a second clean clone, or `make mrproper` in between.
- **`kconf/x86_64.config` is validated under QEMU TCG emulation,** not on real x86 hardware.
- `run-kernel.py` uses `-snapshot`, so writes are discarded on exit and the cloud image stays
  pristine and reusable.
- **Hardening:** build/compile containers run least-privilege (no network, caps dropped,
  read-only rootfs), and the QEMU run is locked down too (`-nodefaults`, no monitor, guest
  egress blocked via slirp `restrict=on` — override `GUEST_NET=open`, seccomp `-sandbox` on
  Linux). Opt into an outer process sandbox with `MK_SANDBOX=auto` (bwrap/systemd-run on
  Linux, Seatbelt on macOS). See [`docs/qemu-hardening.md`](docs/qemu-hardening.md).
