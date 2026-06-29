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
import gzip
import hashlib
import json
import mailbox
import os
import re
import subprocess
import sys
import tempfile
import threading
from dataclasses import dataclass, field
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import mklib  # noqa: E402
import guestlib as g  # noqa: E402
from guestlib import die, log, run  # noqa: E402

g.TAG = "run-kernel"
HERE = Path(__file__).resolve().parent

# Machine-readable progress for the service layer: when --progress is set, each
# phase transition prints one `MKPROGRESS {json}` line to stdout (harmless extra
# output for CLI users; the service greps for the sentinel).
_PROGRESS = False
PROGRESS_SENTINEL = "MKPROGRESS"
# Compare mode runs two variants in parallel threads; serialize the progress/log
# writes so their MKPROGRESS lines (which the service greps) never interleave.
_PRINT_LOCK = threading.Lock()


def progress(phase: str, **extra) -> None:
    if _PROGRESS:
        with _PRINT_LOCK:
            print(f"{PROGRESS_SENTINEL} {json.dumps({'phase': phase, **extra})}", flush=True)

META_KEYS = {"url", "commit", "patch", "arch", "thread", "patch-compare", "thread-compare", "compiler"}
# gcc versions for which a build image is published (see Containerfile / CI). A
# bundle's `compiler:` key selects one; an unsupported value falls back to the default.
SUPPORTED_GCC = {"13", "14", "15"}
# gcc-14 (gnu17) by default for compatibility with older kernels: gcc-15 defaults
# to C23 and fails on pre-~6.7 kernels' realmode/boot units. A bundle can still
# opt into another supported version via the `compiler:` key.
DEFAULT_GCC = "14"
ROLES = ("user", "module", "kconf", "patch", "init")

# Hardened mode (always on): a bundle never chooses its own kernel remote. Any
# bundle that requests a remote tree (url/commit/patch) is forced to build from
# Linus's tree; a bundle's own `url:` is ignored. Metadata-less bundles still
# build LINUX_SRC as-is (no fetch).
KERNEL_URL = "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"


def enforce_hardened(meta: dict) -> dict:
    """Force the kernel source to KERNEL_URL whenever the bundle requests a remote
    tree (url/commit/patch/thread/thread-compare). Returns the (mutated) meta dict."""
    if (meta.get("url") or meta.get("commit") or meta.get("patch")
            or meta.get("thread") or meta.get("thread-compare")):
        if meta.get("url") and meta["url"] != KERNEL_URL:
            log(f"hardened: ignoring bundle url {meta['url']!r}; forcing {KERNEL_URL}")
        meta["url"] = KERNEL_URL
    return meta


def _stage(path: Path, content: str) -> Path:
    """Write a staged bundle file, creating parent dirs first. Role filenames may
    contain '/' (e.g. `kconf:drivers/misc/Kconfig`), so the target's parent dir
    might not exist yet -- without this, write_text() raises FileNotFoundError."""
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content)
    return path


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
    qbin = mklib.qemu_binary(arch)
    qemu = [
        qbin,
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
    ]
    # Optional outer sandbox (MK_SANDBOX); interactive=True keeps the controlling tty.
    cmd = mklib.sandbox_prefix(arch, run_dir=HERE, files=[kimg, img, seed],
                               interactive=True, qemu_bin=qbin) + qemu
    os.execvp(cmd[0], cmd)


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
    # `---` at column 0 (no leading indent) and outside a fence; for each
    # consecutive pair, the inner lines must all be `key: value` (or blank) and
    # include >=1 recognized key. The column-0 rule matches YAML front-matter and
    # git cover-letter convention while ignoring `---` inside an indented markdown
    # code block (e.g. a documentation example of the metadata syntax).
    dashes = [idx for idx, ln in enumerate(lines)
              if ln.rstrip() == "---" and idx not in in_fence_lines]
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

def prepare_kernel_tree(meta: dict, linux_src: Path, log_path: Path | None = None,
                        wt_root: Path | None = None) -> Path:
    """Resolve the kernel tree to build. With no url/commit/patch/thread, use
    linux_src as-is. Otherwise materialize a cached git worktree at the requested
    commit (adding+fetching the remote for a url) and apply the patch / `git am` the
    thread series there. `wt_root` overrides the worktree cache root (compare mode
    gives each variant its own root so patched/unpatched trees never collide). When
    `log_path` is given, the fetch/worktree/patch step is captured there and kept
    as the job's fetch.log (only created when an actual fetch/prepare happens)."""
    url, commit, patch = meta.get("url"), meta.get("commit"), meta.get("patch")
    # `thread`: a lore thread URL whose series is git-am'd on top. Set by a `thread:`
    # frontmatter key, or injected by compare_variants for thread-compare's patched run.
    thread = meta.get("thread")
    no_remote = not (url or commit or patch or thread)
    if no_remote and not (linux_src / ".git").exists():
        return linux_src  # non-git tree: nothing to isolate, build it as-is

    if not (linux_src / ".git").exists():
        die(f"bundle needs a git kernel tree at LINUX_SRC={linux_src}")
    # Even a no-metadata bundle builds in a per-job worktree (at the current HEAD)
    # rather than linux_src directly, so concurrent jobs get isolated .config +
    # build artifacts instead of clobbering each other in the shared tree. The
    # service sets a per-job MK_WT_ROOT; below, treeish defaults to HEAD and the
    # url/patch/thread steps are all skipped when there's no remote work.

    # Capture this step's git output into fetch.log (in addition to live logging).
    # flog() flushes before each captured subprocess so the lines stay ordered.
    fl = open(log_path, "w") if log_path else None
    cap = {"stdout": fl, "stderr": subprocess.STDOUT} if fl else {}

    def flog(msg: str) -> None:
        log(msg)
        if fl:
            print(f"[fetch] {msg}", file=fl, flush=True)

    def have_commit(treeish: str) -> bool:
        return run(["git", "-C", str(linux_src), "rev-parse", "--verify", "-q",
                    f"{treeish}^{{commit}}"], capture_output=True, text=True).returncode == 0

    try:
        if url:
            remote = "mk-" + hashlib.sha1(url.encode()).hexdigest()[:8]
            existing = run(["git", "-C", str(linux_src), "remote"],
                           capture_output=True, text=True).stdout.split()
            if remote not in existing:
                flog(f"adding remote {remote} -> {url}")
                run(["git", "-C", str(linux_src), "remote", "add", remote, url], check=True)
            # Fetch only when the requested commit isn't already present locally.
            if commit and have_commit(commit):
                flog(f"commit {commit} already present, skipping fetch")
            else:
                flog(f"fetching {remote} (all refs) ...")
                if run(["git", "-C", str(linux_src), "fetch", "--tags", remote], **cap).returncode != 0:
                    die(f"git fetch {remote} failed")

        treeish = commit or "HEAD"
        sha = run(["git", "-C", str(linux_src), "rev-parse", "--verify", f"{treeish}^{{commit}}"],
                  capture_output=True, text=True)
        if sha.returncode != 0:
            die(f"commit '{treeish}' not found after fetch (must be reachable from a ref)")
        short = sha.stdout.strip()[:12]
        flog(f"resolved {treeish} -> {short}")

        # Worktree cache root: an explicit wt_root (compare mode: per-variant root)
        # wins; else MK_WT_ROOT isolates concurrent jobs (the service gives each job
        # its own root so same-commit builds don't collide); default sits next to the
        # source tree.
        wt_root = wt_root or Path(os.environ.get("MK_WT_ROOT") or f"{linux_src}-wt")
        wt = wt_root / short
        # Drop registrations whose dirs were deleted out from under git (e.g. a job's
        # work dir was cleaned), so `worktree add` to that path doesn't fail.
        run(["git", "-C", str(linux_src), "worktree", "prune"], **cap)
        if not (wt / ".git").exists():
            wt.parent.mkdir(parents=True, exist_ok=True)
            flog(f"creating worktree {wt} @ {short}")
            if run(["git", "-C", str(linux_src), "worktree", "add", "--detach",
                    str(wt), short], **cap).returncode != 0:
                die("git worktree add failed")
        else:
            flog(f"reusing cached worktree {wt}")

        if patch:
            marker = wt / ".mk-patched"
            if not marker.exists():
                pf = wt / ".mk.patch"
                flog(f"downloading patch {patch} ...")
                if run(["curl", "-LfsS", "-o", str(pf), patch], **cap).returncode != 0:
                    die("patch download failed")
                flog("applying patch ...")
                if run(["git", "-C", str(wt), "apply", str(pf)], **cap).returncode != 0:
                    die("git apply failed (clear the cached worktree to retry)")
                marker.write_text(patch + "\n")
        if thread:
            marker = wt / ".mk-thread-patched"
            if not marker.exists():
                apply_thread(wt, thread, flog, cap)
                marker.write_text(thread + "\n")
        return wt
    finally:
        if fl:
            fl.close()


# ----------------------------------------------------------------------------
# Apply a whole patch series from a lore.kernel.org thread
# ----------------------------------------------------------------------------

def _decode_part(part) -> str:
    """Decoded text of one email part (best effort)."""
    payload = part.get_payload(decode=True)
    if payload is None:
        return part.get_payload() or ""
    charset = part.get_content_charset() or "utf-8"
    try:
        return payload.decode(charset, errors="replace")
    except (LookupError, ValueError):
        return payload.decode("utf-8", errors="replace")


def _message_text(msg) -> str:
    """Plain-text body of a mail (joining text/plain parts of a multipart)."""
    if msg.is_multipart():
        return "\n".join(_decode_part(p) for p in msg.walk()
                         if p.get_content_type() == "text/plain")
    return _decode_part(msg)


_PATCH_IDX_RE = re.compile(r"\[PATCH[^\]]*?\b(\d+)\s*/\s*\d+\]", re.IGNORECASE)


def select_thread_patches(mbox_path: Path) -> list:
    """From a public-inbox thread mbox, return the messages to `git am`, in series
    order. Keep only `[PATCH ...]` mails that actually carry a diff -- this drops the
    `0/N` cover letter (no diff) and plain replies/acks -- and order them by the n/m
    in the subject (so 1/3, 2/3, 3/3 apply in order; ties keep mbox order)."""
    selected = []  # (series_index, mbox_order, message)
    for order, msg in enumerate(mailbox.mbox(str(mbox_path))):
        subject = " ".join((msg.get("Subject") or "").split())
        if "[patch" not in subject.lower():
            continue
        if "\ndiff --git " not in "\n" + _message_text(msg):
            continue
        m = _PATCH_IDX_RE.search(subject)
        selected.append((int(m.group(1)) if m else 0, order, msg))
    selected.sort(key=lambda t: (t[0], t[1]))
    return [msg for _, _, msg in selected]


def apply_thread(wt: Path, thread: str, flog, cap) -> None:
    """Fetch the lore thread mbox for `thread`, keep the `[PATCH n/m]` mails that
    carry a diff (dropping the `0/N` cover letter and non-patch replies), order them
    by series index, and `git am` them onto the worktree.

    Known limitation: a thread carrying multiple revisions (v1 + v2) is not
    de-duplicated -- every patch mail with a diff is applied. `b4 am` would be the
    upgrade path if that becomes a problem in practice."""
    base = thread.rstrip("/")
    if base.endswith("/raw"):
        base = base[: -len("/raw")]
    mbox_url = base + "/t.mbox.gz"

    gz = wt / ".mk-thread.mbox.gz"
    flog(f"downloading thread mbox {mbox_url} ...")
    if run(["curl", "-LfsS", "-o", str(gz), mbox_url], **cap).returncode != 0:
        die("thread mbox download failed")
    raw = wt / ".mk-thread.mbox"
    try:
        with gzip.open(gz, "rb") as f:
            raw.write_bytes(f.read())
    except OSError as e:
        die(f"thread mbox decompress failed: {e}")

    patches = select_thread_patches(raw)
    if not patches:
        die("thread contained no applicable [PATCH] mails with diffs")

    am_box = wt / ".mk-thread-am.mbox"
    am_box.unlink(missing_ok=True)
    out = mailbox.mbox(str(am_box))
    out.lock()
    for msg in patches:
        out.add(msg)
    out.flush()
    out.unlock()
    out.close()

    flog(f"applying {len(patches)} patch(es) from thread ...")
    if run(["git", "-C", str(wt), "am", "-3", str(am_box)], **cap).returncode != 0:
        run(["git", "-C", str(wt), "am", "--abort"], **cap)
        die(f"git am failed applying thread series ({len(patches)} patch(es)); "
            "clear the cached worktree to retry")


# ----------------------------------------------------------------------------
# Build a kernel module (.ko) against the (built) kernel tree
# ----------------------------------------------------------------------------

def build_modules(modfiles, tree: Path, arch: str, image: str, is_local: bool,
                  log_file=None) -> list[Path]:
    """Build each module .c into its own .ko via `make -C <tree> M=<dir> modules`.
    Returns the host paths of the .ko files (in declaration order)."""
    moddir = Path(tempfile.mkdtemp(prefix=".mk-mod-", dir=HERE))
    stems = []
    for name, content in modfiles:
        _stage(moddir / name, content)
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
    # Cross prefix (empty for a native build) so the .ko is built by the same
    # toolchain path as the kernel -- natively cross-compiled on an x86_64 host
    # rather than emulated. CC tracks MK_GCC (cross binary is e.g. aarch64-linux-gnu-gcc-14).
    cross = mklib.cross_compile(arch)
    gcc = os.environ.get("MK_GCC", DEFAULT_GCC)
    cmd = [
        "podman", "run", "--rm", *pull, *mklib.platform_args(arch),
        *mklib.hardening_args(arch),
        "-v", mklib.volume(tree, "/linux"), "-v", mklib.volume(moddir, "/mod"),
        "-w", "/mod", image,
        "bash", "-c",
        f'set -e; make -C /linux ARCH={ka} CROSS_COMPILE={cross} CC={cross}gcc-{gcc} HOSTCC=gcc-{gcc} modules; '
        f'make -C /linux M=/mod ARCH={ka} CROSS_COMPILE={cross} CC={cross}gcc-{gcc} HOSTCC=gcc-{gcc} modules',
    ]
    log(f"building module(s) {', '.join(s + '.ko' for s in stems)} ...")
    cap = {"stdout": log_file, "stderr": subprocess.STDOUT} if log_file else {}
    if log_file:
        print(f"\n=== building module(s): {', '.join(s + '.ko' for s in stems)} ===",
              file=log_file, flush=True)
    if run(cmd, **cap).returncode != 0:
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


# ----------------------------------------------------------------------------
# Output-tree cache key: invalidate the cached .config + kernel image when the
# reproducer, the build logic / kconf fragments, or the target arch change.
# ----------------------------------------------------------------------------
# need_config (below) skips reconfigure for a bundle with no kconf block, and the
# build is skipped when the image already exists -- so edits to kconf/base.config,
# to these scripts, or a switch of arch would otherwise silently reuse a stale
# build. We stamp a key file in the output tree; on a mismatch we delete .config +
# the image, which flips both gates back on for one fresh reconfigure + build.

def _repo_hash() -> str:
    """Hash the build logic + kconf fragments (the "run-kernel.py repo")."""
    h = hashlib.sha256()
    for f in sorted(HERE.glob("*.py")) + sorted((HERE / "kconf").glob("*")):
        h.update(f.read_bytes())
    return h.hexdigest()


def _bundle_hash(b) -> str:
    """Hash the reproducer. Bundles can come from a URL, so there is no stable
    on-disk path -- hash the parsed meta + files instead."""
    blob = json.dumps({"meta": b.meta, "files": b.files}, sort_keys=True)
    return hashlib.sha256(blob.encode()).hexdigest()


def invalidate_stale_outputs(b, tree: Path, arch: str) -> None:
    key = f"{_bundle_hash(b)} {_repo_hash()} {arch}\n"
    keyfile = Path(mklib.output_root(tree)) / ".mk-cache-key"
    if keyfile.is_file() and keyfile.read_text() == key:
        return
    for stale in (mklib.config_path(tree), mklib.kernel_image(tree, arch)):
        stale.unlink(missing_ok=True)
    keyfile.write_text(key)


def build_boot_run(b, tree: Path, arch: str, args, log_dir: Path | None,
                   img: Path, seed: Path, image: str, is_local: bool,
                   bundle_stem: str, ssh_port: int | None = None,
                   variant: str | None = None) -> int:
    """Configure+build the kernel in `tree`, compile userspace/modules, boot it under
    QEMU, and run the bundle in the guest; return the guest command's exit status.
    A `variant` tags the progress lines (compare mode runs two of these in parallel,
    each with its own `tree`/`log_dir`/`ssh_port`)."""
    if log_dir:
        log_dir.mkdir(parents=True, exist_ok=True)

    def prog(phase: str, **extra) -> None:
        progress(phase, **({"variant": variant} if variant else {}), **extra)

    scratch = Path(tempfile.mkdtemp(prefix=".mk-bundle-", dir=HERE))

    # 1. (Re)configure + build the kernel. Reconfigure when the bundle carries
    #    kconf fragments or the tree isn't configured yet.
    env = {**os.environ, "LINUX_SRC": str(tree)}
    fragments = []
    for idx, (name, content) in enumerate(b.files["kconf"]):
        # The fragment's on-disk name is synthetic (configure-kernel.py merges by
        # path), so flatten to the basename -- a `kconf:drivers/misc/Kconfig` block
        # becomes frag0-Kconfig rather than a nested path.
        fp = _stage(scratch / f"frag{idx}-{Path(name).name}", content)
        fragments.append(fp)
    # Drop a stale .config / image when the reproducer, these scripts, or arch
    # changed, so the gates below trigger a fresh reconfigure + build.
    invalidate_stale_outputs(b, tree, arch)
    # configure + build output -> compile.log (append) when a log dir is set.
    clog = open(log_dir / "compile.log", "w") if log_dir else None
    cap = {"stdout": clog, "stderr": subprocess.STDOUT} if clog else {}
    need_config = bool(fragments) or not mklib.config_path(tree).is_file()
    if need_config:
        frag_args = []
        for fp in fragments:
            frag_args += ["--fragment", str(fp)]
        log("configuring kernel ...")
        prog("configure")
        if run([sys.executable, str(HERE / "configure-kernel.py"), *frag_args],
               cwd=HERE, env=env, **cap).returncode != 0:
            die("kernel configure failed")
    if bool(fragments) or not mklib.kernel_image(tree, arch).is_file():
        log("building kernel ...")
        prog("build")
        if run([sys.executable, str(HERE / "build-kernel.py")], cwd=HERE, env=env,
               **cap).returncode != 0:
            die("kernel build failed")
    # Keep compile.log open through the userspace + module container builds below so
    # their podman output is captured there too (closed after step 3).

    # 2. Compile userspace C into one static binary; copy other user: files too.
    user_files = []
    for name, content in b.files["user"]:
        user_files.append(_stage(scratch / name, content))
    binary = None
    if any(p.suffix == ".c" for p in user_files):
        cs = [p for p in user_files if p.suffix == ".c"]
        binname = cs[0].stem if len(cs) == 1 else bundle_stem
        binary = g.compile_c(user_files, binname, image, is_local,
                             arch, [], log_file=clog)

    # 3. Build kernel module(s).
    kos = build_modules(b.files["module"], tree, arch, image, is_local,
                        log_file=clog) if b.files["module"] else []
    if clog:
        clog.close()

    # 4. Init script (+ any non-.c user data files) staged for the guest.
    init_name = None
    init_path = None
    if b.files["init"]:
        init_name, content = b.files["init"][0]
        init_path = _stage(scratch / init_name, content)
    data_files = [p for p in user_files if p.suffix != ".c"]

    # 5. Boot and run.
    key = Path(os.environ.get("SSH_KEY", "id_mackernel"))
    user = os.environ.get("GUEST_USER", "mac")
    port = ssh_port if ssh_port is not None else g.free_port(args.ssh_port)
    if port != args.ssh_port:
        log(f"port {args.ssh_port} busy, using {port} instead")
    boot_log = (log_dir / "console.log") if log_dir else (HERE / "run-kernel-boot.log")
    prog("boot")
    proc = g.boot_qemu(arch, tree, img, seed, port, boot_log)
    rc = 1
    gdir = "/tmp/mkbundle"
    # TCG (foreign-arch emulation, no KVM) boots ~10x slower and is easily starved
    # by concurrent builds/the summarizer, so triple the SSH wait when emulating.
    boot_timeout = args.boot_timeout
    if mklib.qemu_accel_cpu(arch)[0] == "tcg":
        boot_timeout *= 3
        log(f"emulated (tcg) boot: extending SSH timeout to {boot_timeout}s")
    try:
        g.wait_for_ssh(port, key, user, boot_timeout)
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

        if kos:
            prog("insmod")
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
        prog("run")
        # Guest output -> exec.log when a log dir is set, else streamed to stdout.
        if log_dir:
            with open(log_dir / "exec.log", "w") as elog:
                rc = g.ssh_run(port, key, user, cmd, stdout=elog, stderr=subprocess.STDOUT)
        else:
            print("\033[1;32m---------------- guest output ----------------\033[0m", flush=True)
            rc = g.ssh_run(port, key, user, cmd)
            print("\033[1;32m----------------------------------------------\033[0m", flush=True)
        log(f"guest command exited with status {rc}")
        # Capture the guest kernel ring buffer for the dmesg tab (best effort:
        # the guest may be wedged after a crash, in which case console.log has it).
        if log_dir:
            with open(log_dir / "dmesg.log", "w") as dlog:
                g.ssh_run(port, key, user, "sudo dmesg", stdout=dlog, stderr=subprocess.STDOUT)
        prog("done", exit=rc)
    finally:
        if args.keep_running:
            log(f"--keep-running: QEMU still up. SSH: ssh -p {port} -i {key} {user}@127.0.0.1")
            log(f"  kill it with: kill {proc.pid}")
        else:
            g.teardown(proc)
    return rc


def _truthy(v) -> bool:
    return str(v).strip().lower() in {"1", "true", "yes", "on"}


def compare_variants(meta: dict) -> tuple[dict, dict] | None:
    """If `meta` requests a comparison, return (baseline_meta, patched_meta); else None.
    patch-compare strips the bundle's own `patch:` for the baseline; thread-compare
    git-ams a lore thread's series (via an internal `thread` key) for the patched
    variant. patch-compare wins if both are set."""
    if _truthy(meta.get("patch-compare")) and meta.get("patch"):
        baseline = {k: v for k, v in meta.items() if k != "patch"}
        return baseline, dict(meta)
    if meta.get("thread-compare"):
        baseline = {k: v for k, v in meta.items() if k != "thread-compare"}
        return baseline, {**baseline, "thread": meta["thread-compare"]}
    return None


def run_bundle(src, args) -> int:
    os.chdir(HERE)
    # Optional per-job log dir: configure+build -> compile.log, serial -> dmesg.log,
    # guest output -> exec.log. Without it, behaviour is unchanged (inherits stdio).
    log_dir = Path(args.log_dir).resolve() if args.log_dir else None
    if log_dir:
        log_dir.mkdir(parents=True, exist_ok=True)
    progress("fetch")
    bundle_path = fetch_bundle(str(src))
    b = parse_bundle(bundle_path)
    # Inline patch: a ```patch:foo.patch fence stages its diff to a file and is then
    # treated exactly like patch: (a local file:// URL) -- so a bundle can carry the
    # patch to apply (and to patch-compare against) without an external URL.
    if b.files["patch"] and not b.meta.get("patch"):
        pf = Path(tempfile.mkdtemp(prefix="mk-patch-")) / "inline.patch"
        pf.write_text("".join(content for _, content in b.files["patch"]))
        b.meta["patch"] = pf.as_uri()
    enforce_hardened(b.meta)  # always build from Linus's tree; ignore bundle url

    # Bundle builds are in-tree in the (cached) worktree, so a kernel module can
    # build against /linux directly; BUILD_DIR would split that out, so ignore it.
    os.environ.pop("BUILD_DIR", None)

    # Target arch: frontmatter `arch:` wins, else ARCH env, else host arch. Set it
    # in the environment so the configure/build subprocesses agree.
    if b.meta.get("arch"):
        os.environ["ARCH"] = mklib.normalize_arch(b.meta["arch"])
    arch = mklib.target_arch()
    base_src = Path(os.environ.get("LINUX_SRC", os.path.expanduser("~/linux")))

    # Compiler: frontmatter `compiler:` picks the gcc build image (default 14).
    # gcc-15 defaults to C23 and fails on pre-~6.7 kernels' realmode/boot units;
    # gcc-13/14 (gnu17) build them. MK_GCC is read by build-kernel.py.
    gcc = str(b.meta.get("compiler", DEFAULT_GCC)).strip()
    if gcc not in SUPPORTED_GCC:
        log(f"compiler {gcc!r} unsupported; using gcc-{DEFAULT_GCC} "
            f"(have: {', '.join(sorted(SUPPORTED_GCC))})")
        gcc = DEFAULT_GCC
    os.environ["MK_GCC"] = gcc
    log(f"building with gcc-{gcc}")

    # Shared resources, resolved+materialized once (compare mode's two threads must
    # not race on the cloud-image download or the podman pull).
    image, is_local = mklib.resolve_image(arch, gcc)
    mklib.ensure_pulled(image, is_local, mklib.platform_args(arch))
    img = Path(os.environ.get("IMG", mklib.arch_profile(arch)["cloud_img"]))
    img_url = os.environ.get(
        "IMG_URL", f"https://cloud-images.ubuntu.com/noble/current/{img.name}")
    seed = Path(os.environ.get("SEED", "seed.iso"))
    g.ensure_cloud_image(img, img_url)
    g.ensure_seed(seed)
    stem = bundle_path.stem

    # Compare mode: build+boot+run a baseline (no patch/series) and a patched variant
    # in parallel.
    variants = compare_variants(b.meta)
    if variants is not None:
        baseline_meta, patched_meta = variants
        # ponytail: two fixed variants, not an N-way matrix; parallel doubles peak
        # RAM/CPU (two kernel builds + two QEMUs) -- the explicit ask.
        wt_base = Path(os.environ.get("MK_WT_ROOT") or f"{base_src}-wt")
        bdir = (log_dir / "baseline") if log_dir else None
        pdir = (log_dir / "patched") if log_dir else None
        # Create the per-variant log dirs up front: prepare_kernel_tree writes
        # <variant>/fetch.log below, before build_boot_run would create them.
        for d in (bdir, pdir):
            if d:
                d.mkdir(parents=True, exist_ok=True)
        # Tree prep touches the shared .git, so do both sequentially (never concurrent);
        # the build/boot/run that follows is per-worktree and safely parallel.
        tb = prepare_kernel_tree(baseline_meta, base_src,
                                 log_path=(bdir / "fetch.log") if bdir else None,
                                 wt_root=wt_base / "baseline")
        tp = prepare_kernel_tree(patched_meta, base_src,
                                 log_path=(pdir / "fetch.log") if pdir else None,
                                 wt_root=wt_base / "patched")
        log(f"compare: baseline tree {tb}; patched tree {tp}")
        # Distinct ports; tiny TOCTOU window -- a clash just fails that boot, not silently.
        p_b = g.free_port(args.ssh_port)
        p_p = g.free_port(p_b + 1)
        out: dict[str, int] = {}

        def go(name, tree, ld, port, variant):
            try:
                out[name] = build_boot_run(b, tree, arch, args, ld, img, seed,
                                           image, is_local, stem, port, variant)
            except SystemExit as e:  # die() raises SystemExit; keep the other variant alive
                out[name] = e.code if isinstance(e.code, int) else 1

        threads = [threading.Thread(target=go, args=a) for a in (
            ("base", tb, bdir, p_b, "baseline"),
            ("patch", tp, pdir, p_p, "patched"),
        )]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
        rc_b, rc_p = out.get("base", 1), out.get("patch", 1)
        log(f"compare: baseline exit={rc_b}, patched exit={rc_p}")
        progress("done", exit=rc_p)  # authoritative final line; overall exit = patched
        return rc_p

    tree = prepare_kernel_tree(b.meta, base_src,
                               log_path=(log_dir / "fetch.log") if log_dir else None)
    log(f"kernel tree: {tree}")
    return build_boot_run(b, tree, arch, args, log_dir, img, seed, image, is_local, stem)


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
    ap.add_argument("--log-dir",
                    help="write compile.log / dmesg.log / exec.log into this dir (service mode)")
    ap.add_argument("--progress", action="store_true",
                    help="emit 'MKPROGRESS {json}' phase lines on stdout (service mode)")
    args = ap.parse_args()

    global _PROGRESS
    _PROGRESS = args.progress

    if args.bundle is None:
        return boot_interactive()
    return run_bundle(args.bundle, args)


if __name__ == "__main__":
    sys.exit(main())
