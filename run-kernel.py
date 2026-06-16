#!/usr/bin/env python3
"""(4) Boot the built kernel under QEMU against an Ubuntu cloud image.

Two modes:

  ./run-kernel.py                 # interactive: boot to a serial console (HVF/KVM/TCG)
  ./run-kernel.py repro.md        # bundle: build+run a self-contained repro, then exit

Interactive mode keeps the original behaviour: QEMU's serial stays on your
terminal (quit with Ctrl-a x); INIT=/bin/bash drops straight to a shell.

Bundle mode reads a SKILL.md-like file describing a whole repro -- an optional
kernel source (git url/commit/patch), userspace C files, kernel module(s), extra
Kconfig, and a start script -- builds it all, boots, runs it in the guest over
SSH, streams the output, and exits with the guest program's status. See the
README for the bundle format.
"""
from __future__ import annotations

import argparse
import hashlib
import os
import re
import subprocess
import sys
import tempfile
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mklib  # noqa: E402
import guestlib as g  # noqa: E402
from guestlib import die, log, run  # noqa: E402

g.TAG = "run-kernel"
HERE = Path(__file__).resolve().parent

META_KEYS = {"url", "commit", "patch", "arch"}
ROLES = ("user", "module", "kconf", "init")


# ----------------------------------------------------------------------------
# Interactive mode (no bundle argument): boot to a serial console.
# ----------------------------------------------------------------------------

def boot_interactive() -> int:
    os.chdir(HERE)
    linux_src = os.environ.get("LINUX_SRC", os.path.expanduser("~/linux"))
    arch = mklib.target_arch()
    prof = mklib.arch_profile(arch)
    accel, cpu = mklib.qemu_accel_cpu(arch)
    kimg = mklib.kernel_image(linux_src, arch)

    img = os.environ.get("IMG", prof["cloud_img"])
    img_url = os.environ.get(
        "IMG_URL", f"https://cloud-images.ubuntu.com/noble/current/{img}")
    ssh_port = os.environ.get("SSH_PORT", "2222")
    seed = os.environ.get("SEED", "seed.iso")
    # Default: boot the real init (systemd) so cloud-init runs. INIT=/bin/bash skips it.
    init = os.environ.get("INIT", "")

    if not kimg.is_file():
        print("kernel image not found, building it first...", flush=True)
        subprocess.run([sys.executable, str(HERE / "build-kernel.py")], check=True)
    g.ensure_cloud_image(Path(img), img_url)
    g.ensure_seed(Path(seed))

    append = f"console={prof['console']} root=/dev/vda1 rw"
    if init:
        append += f" init={init}"

    print(f"=== booting {kimg} ({arch}, accel={accel}) ===")
    print(f"    SSH:  ssh -p {ssh_port} -i id_mackernel mac@127.0.0.1"
          "   (after cloud-init finishes)")
    print("    quit: Ctrl-a x", flush=True)
    # Guest egress isolation (override with GUEST_NET=open); see guestlib.boot_qemu.
    restrict = "" if os.environ.get("GUEST_NET") == "open" else ",restrict=on"
    # -nodefaults (from qemu_hardening_args) suppresses the implicit serial, so the
    # interactive console is wired explicitly with -serial mon:stdio (also gives the
    # Ctrl-a escape) instead of -nographic.
    os.execvp(
        prof["qemu_binary"],
        [
            prof["qemu_binary"],
            *mklib.qemu_hardening_args(),
            "-display", "none",
            "-serial", "mon:stdio",
            "-machine", prof["qemu_machine"],
            "-cpu", cpu, "-accel", accel,
            "-m", "2048", "-smp", "4",
            "-kernel", str(kimg),
            "-drive", f"file={img},if=virtio,format=qcow2",
            "-drive", f"file={seed},if=virtio,format=raw,readonly=on",
            "-netdev", f"user,id=net0,hostfwd=tcp::{ssh_port}-:22{restrict}",
            "-device", "virtio-net-pci,netdev=net0",
            "-object", "rng-builtin,id=rng0",
            "-device", "virtio-rng-pci,rng=rng0",
            "-append", append,
            "-snapshot",
        ],
    )


# ----------------------------------------------------------------------------
# Bundle parsing
# ----------------------------------------------------------------------------

@dataclass
class Bundle:
    meta: dict = field(default_factory=dict)               # url / commit / patch
    files: dict = field(default_factory=lambda: {r: [] for r in ROLES})  # role -> [(name, content)]


def parse_bundle(path: Path) -> Bundle:
    """Parse a bundle file: a `---`-delimited metadata block (anywhere, outside
    code fences) plus ```role:filename fenced blocks. Everything else is prose."""
    lines = path.read_text().splitlines()
    b = Bundle()

    # First pass: collect fenced code blocks and remember which line indices are
    # inside a fence (so the metadata scan ignores any `---` that appears in code).
    in_fence_lines: set[int] = set()
    i = 0
    fence_re = re.compile(r"^```+\s*(\S.*)?$")
    while i < len(lines):
        m = fence_re.match(lines[i])
        if m:
            info = (m.group(1) or "").strip()
            start = i
            i += 1
            body = []
            while i < len(lines) and not re.match(r"^```+\s*$", lines[i]):
                body.append(lines[i])
                i += 1
            end = i  # the closing ``` (or EOF)
            for j in range(start, min(end, len(lines) - 1) + 1):
                in_fence_lines.add(j)
            role, _, name = info.partition(":")
            if role in b.files and name:
                b.files[role].append((name.strip(), "\n".join(body) + "\n"))
            i += 1
        else:
            i += 1

    # Second pass: find the metadata block. Consider every line that is exactly
    # `---` and outside a fence; for each consecutive pair, the inner lines must
    # all be `key: value` (or blank) and include >=1 recognized key.
    dashes = [idx for idx, ln in enumerate(lines)
              if ln.strip() == "---" and idx not in in_fence_lines]
    for a, c in zip(dashes, dashes[1:]):
        parsed = _parse_kv(lines[a + 1:c])
        if parsed is not None and (META_KEYS & parsed.keys()):
            b.meta = {k: v for k, v in parsed.items() if k in META_KEYS}
            break
    return b


def _parse_kv(block: list[str]) -> dict | None:
    """Parse lines as `key: value`; return None if any non-blank line isn't kv."""
    kv = {}
    kv_re = re.compile(r"^([A-Za-z_][\w-]*):\s*(.*)$")
    for ln in block:
        if not ln.strip():
            continue
        m = kv_re.match(ln.strip())
        if not m:
            return None
        kv[m.group(1)] = m.group(2).strip()
    return kv


# ----------------------------------------------------------------------------
# Kernel source: single git tree + cached worktrees
# ----------------------------------------------------------------------------

def prepare_kernel_tree(meta: dict, linux_src: Path) -> Path:
    """Resolve the kernel tree to build. With no url/commit/patch, use linux_src
    as-is. Otherwise materialize a cached git worktree at the requested commit
    (adding+fetching the remote for a url) and apply the patch there."""
    url, commit, patch = meta.get("url"), meta.get("commit"), meta.get("patch")
    if not (url or commit or patch):
        return linux_src

    if not (linux_src / ".git").exists():
        die(f"bundle needs a git kernel tree at LINUX_SRC={linux_src}")

    if url:
        remote = "mk-" + hashlib.sha1(url.encode()).hexdigest()[:8]
        existing = run(["git", "-C", str(linux_src), "remote"],
                       capture_output=True, text=True).stdout.split()
        if remote not in existing:
            log(f"adding remote {remote} -> {url}")
            run(["git", "-C", str(linux_src), "remote", "add", remote, url], check=True)
        log(f"fetching {remote} (all refs) ...")
        if run(["git", "-C", str(linux_src), "fetch", "--tags", remote]).returncode != 0:
            die(f"git fetch {remote} failed")

    treeish = commit or "HEAD"
    sha = run(["git", "-C", str(linux_src), "rev-parse", "--verify", f"{treeish}^{{commit}}"],
              capture_output=True, text=True)
    if sha.returncode != 0:
        die(f"commit '{treeish}' not found after fetch (must be reachable from a ref)")
    short = sha.stdout.strip()[:12]

    wt = Path(f"{linux_src}-wt") / short
    if not (wt / ".git").exists():
        wt.parent.mkdir(parents=True, exist_ok=True)
        log(f"creating worktree {wt} @ {short}")
        if run(["git", "-C", str(linux_src), "worktree", "add", "--detach",
                str(wt), short]).returncode != 0:
            die("git worktree add failed")
    else:
        log(f"reusing cached worktree {wt}")

    if patch:
        marker = wt / ".mk-patched"
        if not marker.exists():
            pf = wt / ".mk.patch"
            log(f"downloading patch {patch} ...")
            if run(["curl", "-LfsS", "-o", str(pf), patch]).returncode != 0:
                die("patch download failed")
            log("applying patch ...")
            if run(["git", "-C", str(wt), "apply", str(pf)]).returncode != 0:
                die("git apply failed (clear the cached worktree to retry)")
            marker.write_text(patch + "\n")
    return wt


# ----------------------------------------------------------------------------
# Build a kernel module (.ko) against the (built) kernel tree
# ----------------------------------------------------------------------------

def build_modules(modfiles, tree: Path, arch: str, image: str, is_local: bool) -> list[Path]:
    """Build each module .c into its own .ko via `make -C <tree> M=<dir> modules`.
    Returns the host paths of the .ko files (in declaration order)."""
    moddir = Path(tempfile.mkdtemp(prefix=".mk-mod-", dir=HERE))
    stems = []
    for name, content in modfiles:
        (moddir / name).write_text(content)
        stems.append(Path(name).stem)
    (moddir / "Makefile").write_text("".join(f"obj-m += {s}.o\n" for s in stems))

    prof = mklib.arch_profile(arch)
    pull = ["--pull=never"] if is_local else []
    mklib.ensure_pulled(image, is_local, mklib.platform_args(arch))
    ka = prof["kernel_arch"]
    # `make Image` doesn't emit Module.symvers (the exported-symbol table external
    # modules link against), so build the in-tree `modules` target first to
    # generate it (cheap: vmlinux is already built, this is just modpost), then
    # build the out-of-tree module.
    cmd = [
        "podman", "run", "--rm", *pull, *mklib.platform_args(arch),
        *mklib.hardening_args(),
        "-v", f"{tree}:/linux", "-v", f"{moddir}:/mod", "-w", "/mod", image,
        "bash", "-c",
        f'set -e; make -C /linux ARCH={ka} CC=gcc-15 modules; '
        f'make -C /linux M=/mod ARCH={ka} CC=gcc-15 modules',
    ]
    log(f"building module(s) {', '.join(s + '.ko' for s in stems)} ...")
    if run(cmd).returncode != 0:
        die("kernel module build failed")
    kos = [moddir / f"{s}.ko" for s in stems]
    for ko in kos:
        if not ko.exists():
            die(f"module build produced no {ko.name}")
    return kos


# ----------------------------------------------------------------------------
# Bundle mode
# ----------------------------------------------------------------------------

def fetch_bundle(src: str) -> Path:
    """Resolve a bundle source to a local file. A local path is returned as-is;
    an http(s) URL is downloaded (lkml/gist page URLs are rewritten to their raw
    form so we get the bundle text, not HTML)."""
    if not re.match(r"^https?://", src):
        p = Path(src)
        if not p.is_file():
            die(f"no such bundle file: {src}")
        return p
    url = src.rstrip("/")
    if ("lore.kernel.org" in url or "gist.github.com" in url) and not url.endswith("/raw"):
        url += "/raw"
    dest = Path(tempfile.mkdtemp(prefix=".mk-fetch-", dir=HERE)) / "bundle.md"
    log(f"fetching bundle from {url} ...")
    if run(["curl", "-LfsS", "-o", str(dest), url]).returncode != 0:
        die("bundle download failed")
    return dest


def run_bundle(src, args) -> int:
    os.chdir(HERE)
    bundle_path = fetch_bundle(str(src))
    b = parse_bundle(bundle_path)

    # Bundle builds are in-tree in the (cached) worktree, so a kernel module can
    # build against /linux directly; BUILD_DIR would split that out, so ignore it.
    os.environ.pop("BUILD_DIR", None)

    # Target arch: frontmatter `arch:` wins, else ARCH env, else host arch. Set it
    # in the environment so the configure/build subprocesses agree.
    if b.meta.get("arch"):
        os.environ["ARCH"] = mklib.normalize_arch(b.meta["arch"])
    arch = mklib.target_arch()
    base_src = Path(os.environ.get("LINUX_SRC", os.path.expanduser("~/linux")))
    tree = prepare_kernel_tree(b.meta, base_src)
    log(f"kernel tree: {tree}")

    scratch = Path(tempfile.mkdtemp(prefix=".mk-bundle-", dir=HERE))

    # 1. (Re)configure + build the kernel. Reconfigure when the bundle carries
    #    kconf fragments or the tree isn't configured yet.
    env = {**os.environ, "LINUX_SRC": str(tree)}
    fragments = []
    for idx, (name, content) in enumerate(b.files["kconf"]):
        fp = scratch / f"frag{idx}-{name}"
        fp.write_text(content)
        fragments.append(fp)
    need_config = bool(fragments) or not mklib.config_path(tree).is_file()
    if need_config:
        frag_args = []
        for fp in fragments:
            frag_args += ["--fragment", str(fp)]
        log("configuring kernel ...")
        if run([sys.executable, str(HERE / "configure-kernel.py"), *frag_args],
               cwd=HERE, env=env).returncode != 0:
            die("kernel configure failed")
    if bool(fragments) or not mklib.kernel_image(tree, arch).is_file():
        log("building kernel ...")
        if run([sys.executable, str(HERE / "build-kernel.py")], cwd=HERE, env=env).returncode != 0:
            die("kernel build failed")

    image, is_local = mklib.resolve_image(arch)

    # 2. Cloud image + seed.
    img = Path(os.environ.get("IMG", mklib.arch_profile(arch)["cloud_img"]))
    img_url = os.environ.get(
        "IMG_URL", f"https://cloud-images.ubuntu.com/noble/current/{img.name}")
    seed = Path(os.environ.get("SEED", "seed.iso"))
    g.ensure_cloud_image(img, img_url)
    g.ensure_seed(seed)

    # 3. Compile userspace C into one static binary; copy other user: files too.
    user_files = []
    for name, content in b.files["user"]:
        p = scratch / name
        p.write_text(content)
        user_files.append(p)
    binary = None
    if any(p.suffix == ".c" for p in user_files):
        cs = [p for p in user_files if p.suffix == ".c"]
        binname = cs[0].stem if len(cs) == 1 else bundle_path.stem
        binary = g.compile_c(user_files, binname, image, is_local,
                             mklib.platform_args(arch), [])

    # 4. Build kernel module(s).
    kos = build_modules(b.files["module"], tree, arch, image, is_local) if b.files["module"] else []

    # 5. Init script (+ any non-.c user data files) staged for the guest.
    init_name = None
    init_path = None
    if b.files["init"]:
        init_name, content = b.files["init"][0]
        init_path = scratch / init_name
        init_path.write_text(content)
    data_files = [p for p in user_files if p.suffix != ".c"]

    # 6. Boot and run.
    key = Path(os.environ.get("SSH_KEY", "id_mackernel"))
    user = os.environ.get("GUEST_USER", "mac")
    port = g.free_port(args.ssh_port)
    if port != args.ssh_port:
        log(f"port {args.ssh_port} busy, using {port} instead")
    boot_log = HERE / "run-kernel-boot.log"
    proc = g.boot_qemu(arch, tree, img, seed, port, boot_log)
    rc = 1
    gdir = "/tmp/mkbundle"
    try:
        g.wait_for_ssh(port, key, user, args.boot_timeout)
        g.ssh_run(port, key, user, f"rm -rf {gdir} && mkdir -p {gdir}")

        payload = ([binary] if binary else []) + kos + data_files + \
                  ([init_path] if init_path else [])
        if payload:
            log(f"copying {len(payload)} file(s) into guest:{gdir} ...")
            if g.scp_to_guest(port, key, user, payload, f"{gdir}/") != 0:
                die("scp into the guest failed")

        # scp doesn't preserve the executable bit, so restore it on the binary and
        # init script (the init script may exec the binary).
        execs = [p.name for p in ([binary] if binary else []) + ([init_path] if init_path else [])]
        if execs:
            g.ssh_run(port, key, user, "chmod +x " + " ".join(f"{gdir}/{e}" for e in execs))

        for ko in kos:
            log(f"insmod {ko.name} ...")
            if g.ssh_run(port, key, user, f"sudo insmod {gdir}/{ko.name}") != 0:
                die(f"insmod {ko.name} failed (see boot log / dmesg)")

        # What to run: init script, else the user binary, else (module-only) dmesg.
        if init_name:
            cmd = f"cd {gdir} && ./{init_name}"
        elif binary:
            cmd = f"cd {gdir} && ./{binary.name}"
        elif kos:
            cmd = "sudo dmesg | tail -n 40"
        else:
            die("bundle has nothing to run (no user binary, init script, or module)")
        print("\033[1;32m---------------- guest output ----------------\033[0m", flush=True)
        rc = g.ssh_run(port, key, user, cmd)
        print("\033[1;32m----------------------------------------------\033[0m", flush=True)
        log(f"guest command exited with status {rc}")
    finally:
        if args.keep_running:
            log(f"--keep-running: QEMU still up. SSH: ssh -p {port} -i {key} {user}@127.0.0.1")
            log(f"  kill it with: kill {proc.pid}")
        else:
            g.teardown(proc)
    return rc


def main() -> int:
    ap = argparse.ArgumentParser(
        description="Boot the mackernel interactively, or build+run a bundle file.",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument("bundle", nargs="?",
                    help="optional bundle file or URL (lkml/gist); omit for interactive boot")
    ap.add_argument("--ssh-port", type=int, default=int(os.environ.get("SSH_PORT", "2222")),
                    help="host port forwarded to guest:22 (bundle mode; auto-bumped if busy)")
    ap.add_argument("--boot-timeout", type=int, default=180,
                    help="seconds to wait for guest SSH in bundle mode (default 180)")
    ap.add_argument("--keep-running", action="store_true",
                    help="leave QEMU running after the bundle finishes")
    args = ap.parse_args()

    if args.bundle is None:
        return boot_interactive()
    return run_bundle(args.bundle, args)


if __name__ == "__main__":
    sys.exit(main())
