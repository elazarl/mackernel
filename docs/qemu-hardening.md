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

## Outer confinement (opt-in via `MK_SANDBOX`)

The in-qemu flags above don't constrain which **host files/network** the qemu
process can touch. For untrusted guests, set `MK_SANDBOX` to wrap the launch
(default `off`; applies to both `run-kernel.py` and `run-in-kernel.py`):

```bash
MK_SANDBOX=auto          ./run-kernel.py repro.md   # host default: seatbelt (mac) / bwrap+systemd (Linux)
MK_SANDBOX=bwrap         ./run-in-kernel.py repro.c # Linux: filesystem + PID-namespace isolation
MK_SANDBOX=bwrap+systemd ./run-in-kernel.py repro.c # + cgroup resource caps
MK_SANDBOX=seatbelt      ./run-in-kernel.py repro.c # macOS: Seatbelt profile
```

A requested-but-unavailable sandbox (e.g. `bwrap` on macOS, or a tool not on
`PATH`) is a hard error — it never silently runs unsandboxed. Built in
`mklib.sandbox_prefix()`.

### Linux (`bwrap`, `systemd`, `bwrap+systemd`)

- **bubblewrap** (`bwrap`): read-only-binds the system dirs (`/usr`, `/etc`,
  `/lib*`, `/bin`, `/sbin`, `/opt` — covering qemu's libs + `/usr/share/{qemu,
  seabios,edk2}` firmware) and a fresh `/dev` with `/dev/kvm` passed through;
  read-write only the run dir + a tmpfs `/tmp` (so `$HOME`, incl. `~/.ssh`, is
  hidden). New PID namespace, `--die-with-parent`, `--new-session` when headless.
  **The host network namespace is kept** (no `--unshare-net`) because slirp
  `hostfwd` binds a host-loopback port for SSH.
- **systemd** (`systemd-run --user --scope`): cgroup caps `MemoryMax=3G`,
  `CPUQuota=400%`, `TasksMax=512` (resource-DoS guard). Rootless user scope.
  `bwrap+systemd` nests bwrap inside the scope.
- Landlock was considered but dropped — no rootless CLI launcher; bwrap already
  provides the filesystem allow-list.

### macOS (`seatbelt`)

`sandbox-exec` with a generated Seatbelt profile (deny-by-default): allows reads
of the system dirs (`/usr`, `/System`, `/Library`, `/opt/homebrew`, dyld) + the
kernel/cloud/seed files, writes only under the run dir + `$TMPDIR`, `iokit-open`
of `AppleHV` for HVF, and localhost networking for the SSH hostfwd — so qemu
can't read `~/.ssh` or write across `$HOME`. The sandbox is inherited by the
exec'd qemu. Best-effort: the `sandbox-exec` CLI is deprecation-labeled (the
underlying Seatbelt mechanism — used by Nix/Bazel/Chrome — is not), and the
HVF/IOKit allowances are macOS-version-sensitive; `MK_SANDBOX=off` is the escape
hatch. (App Sandbox — a code-signed launcher exec'ing qemu — was rejected: coarse
entitlement-only file rules, fragile HVF entitlement-on-exec semantics, and it
adds code-signing to the build.)

## What we deliberately did **not** do

- Run qemu as a different uid via `--user`/`-runas`: redundant under rootless use and
  complicates access to the bind-mounted images.
- Block the hostfwd port: it's the entire point (host→guest SSH); slirp `restrict=on`
  already blocks the dangerous direction (guest→host/LAN).
