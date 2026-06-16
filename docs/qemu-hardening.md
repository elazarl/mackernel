# Hardening & sandboxing the QEMU run

The build/compile containers are locked down via `mklib.hardening_args()`. This
note covers the *other* host process mackernel runs: **`qemu-system-*`**, the
emulator that boots the guest. Two threat directions matter:

1. **guest → host**: a malicious/buggy guest (we boot arbitrary kernels + run
   repros, sometimes as root in the guest) trying to escape or reach the host LAN.
2. **qemu → host**: the emulator process itself being exploited (a QEMU CVE) and
   then doing damage on the host.

The hardware-VM boundary (HVF on macOS, KVM on Linux) is the first wall for (1).
Everything below shrinks the blast radius further and constrains (2).

## Implemented (cross-platform, on by default)

Applied in `guestlib.boot_qemu` (used by `run-in-kernel.py` and bundle mode) and in
`run-kernel.py`'s interactive boot:

| Measure | Why |
|---|---|
| `-no-user-config` | ignore host/user qemu config files — reproducible, no surprise devices |
| `-nodefaults` | create **no** implicit devices; only the ones we list (virtio blk/net/rng + serial) exist |
| slirp `restrict=on` | guest can't initiate **any** outbound connection (verified: in-guest TCP/DNS blocked); the host still reaches it via the forwarded SSH port. Override with `GUEST_NET=open`. |
| `-object rng-builtin` | entropy from qemu's own getrandom, no `/dev` backend dependency |
| `-snapshot` + seed `readonly=on` | guest disk writes discarded on exit; the cloud image + seed are never mutated |
| `-display none`, `-monitor none` / `-serial mon:stdio` | no graphical surface; no QMP/monitor control channel exposed |
| `RLIMIT_CORE=0` on the qemu process | a qemu crash won't dump guest RAM to disk |

These are verified on Apple Silicon (HVF) across all three boot paths — see
`.omc/autoresearch/qemu-hardening/`.

## Implemented (Linux, on by default when `host_os()=="linux"`)

| Measure | Why |
|---|---|
| `-sandbox on,obsolete=deny,elevateprivileges=deny,spawn=deny,resourcecontrol=deny` | QEMU's **seccomp** syscall filter: denies obsolete syscalls, setuid/setgid escalation, `fork`/`exec` (no spawning helpers), and `sched_setaffinity`/nice. macOS has no seccomp, so this is Linux-only. |

> Not boot-tested in this repo's CI host (Apple Silicon). Verify on a Linux/KVM box:
> `ARCH=x86_64 ./run-in-kernel.py repro.c` should still boot+run with `-sandbox` active.

## Recommended outer confinement (opt-in, not wired in by default)

The in-qemu flags above don't constrain which **host files/network** the qemu
process can touch. For untrusted guests, wrap the launch:

### Linux

- **cgroup + namespace caps via systemd** (resource DoS + isolation):
  ```bash
  systemd-run --user --scope -p MemoryMax=3G -p CPUQuota=400% -p TasksMax=512 \
      ./run-kernel.py repro.md
  ```
- **bubblewrap** (filesystem allow-list, new net/pid ns) — bind only the repo,
  the kernel tree, and `/dev/kvm`:
  ```bash
  bwrap --ro-bind /usr /usr --ro-bind /lib /lib --dev /dev \
        --dev-bind /dev/kvm /dev/kvm --bind "$PWD" "$PWD" --bind ~/linux ~/linux \
        --unshare-pid --die-with-parent ./run-kernel.py repro.md
  ```
- **Landlock** (kernel ≥5.13): a small launcher that restricts the qemu process to
  read the kernel image + cloud image and read-write only the run dir.
- **AppArmor/SELinux**: reuse libvirt's `virt-aa-helper`/`svirt` profiles, or ship a
  profile allowing only the image paths + the hostfwd socket.
- `-runas <unprivileged-user>` is unnecessary here (we already run rootless) but is
  the standard drop-privileges step when qemu is launched as root.

### macOS

No seccomp/cgroups. Options:

- **Seatbelt** (`sandbox-exec`, deprecated but functional): confine qemu's file and
  network access to the few paths it needs. Starting profile (`mackernel-qemu.sb`):
  ```scheme
  (version 1)
  (deny default)
  (allow process-fork process-exec)
  (allow mach-lookup)
  (allow iokit-open (iokit-user-client-class "AppleHV"))   ; HVF
  (allow file-read*  (subpath "/usr") (subpath "/System") (subpath "/opt/homebrew"))
  (allow file-read*  (literal "/path/to/Image") (literal "/path/to/cloudimg.img")
                     (literal "/path/to/seed.iso") (literal "/path/to/id_mackernel"))
  (allow file-write* (subpath "/path/to/mackernel"))        ; -snapshot scratch, logs
  (allow network-inbound  (local tcp "localhost:2222"))     ; hostfwd
  (allow network-outbound (remote tcp "localhost:*"))
  ```
  Launch: `sandbox-exec -f mackernel-qemu.sb qemu-system-aarch64 …`. Not enabled by
  default because the HVF/IOKit allowances are macOS-version-sensitive and the tool
  is deprecated; treat as a hardening starting point to validate per host.
- **Resource caps**: `ulimit -c 0` (cores, also done in-process) and address-space
  caps; run under a dedicated non-admin login for stronger separation.

## What we deliberately did **not** do

- Run qemu as a different uid via `--user`/`-runas`: redundant under rootless use and
  complicates access to the bind-mounted images.
- Block the hostfwd port: it's the entire point (host→guest SSH); slirp `restrict=on`
  already blocks the dangerous direction (guest→host/LAN).
