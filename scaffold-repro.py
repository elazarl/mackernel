#!/usr/bin/env python3
"""Scaffold a reproducer bundle from a patch series, using the `opencode` agent.

Given an LKML thread URL (or a local patch file) and a base commit, this:
  1. materializes a read-only kernel worktree at the base commit (reusing
     run-kernel.py's worktree prep) for the agent to explore,
  2. builds a prompt = the reproducer spec + the patch series + a fixed instruction
     (reproduce the bug the patch fixes; explore how the code is reached; write a
     userspace/module repro, else a printk-triggering one, else explain why),
  3. runs `opencode` (full agent mode, tools on) inside the `-opencode` container
     with the worktree mounted read-only and egress restricted to opencode's
     servers (see docs/opencode-egress.md),
  4. writes the bundle the agent produced to --out.

The produced bundle is a normal reproducer (see docs/reproducer-spec.md) and is run
through the unchanged run-kernel.py pipeline afterwards.

  scaffold-repro.py --thread <lore-url> [--commit <ref>] --out repro.md
  scaffold-repro.py --patch-file fix.patch [--commit <ref>] --out repro.md

With --progress, prints `MKPROGRESS {json}` phase lines (prepare/agent/done) on
stdout for the service layer; opencode's own output is streamed too.
"""
from __future__ import annotations

import argparse
import gzip
import importlib.util
import json
import os
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
import mklib  # noqa: E402
import guestlib as g  # noqa: E402
from guestlib import die, log, run  # noqa: E402

g.TAG = "scaffold"


def _load_run_kernel():
    """Import run-kernel.py as a module (its filename has a dash). Module-level code
    is just imports + defs; main() is guarded, so importing is side-effect-free."""
    spec = importlib.util.spec_from_file_location("run_kernel", HERE / "run-kernel.py")
    mod = importlib.util.module_from_spec(spec)
    # Register before exec so @dataclass (which resolves cls.__module__ via
    # sys.modules) sees the module during its own definition.
    sys.modules[spec.name] = mod
    spec.loader.exec_module(mod)
    return mod


rk = _load_run_kernel()

PROGRESS_SENTINEL = "MKPROGRESS"
_PROGRESS = False
# opencode agent runs are long — the model explores the kernel tree at length and 900s
# wasn't enough to finish even after it had written repro.md. Cap generously (override
# via MK_SCAFFOLD_TIMEOUT).
TIMEOUT = int(os.environ.get("MK_SCAFFOLD_TIMEOUT", "1800"))

# The agent runs against the user's own OpenAI-compatible endpoint (no free tier): base
# URL + API key + model, supplied by the service from the request. opencode reads the
# key from this env var via a `{env:...}` reference in the opencode.json we write.
OPENAI_BASE_URL = os.environ.get("MK_OPENAI_BASE_URL", "").strip()
OPENAI_API_KEY = os.environ.get("MK_OPENAI_API_KEY", "").strip()
MODEL = os.environ.get("MK_OPENCODE_MODEL", "").strip()
# opencode provider id we register in opencode.json; -m <PROVIDER>/<model> selects it.
PROVIDER = "custom"
API_KEY_ENV = "MK_OPENAI_API_KEY"

# The fixed instruction the agent must follow (verbatim from the feature request).
INSTRUCTION = """\
Try to reproduce a bug fixed by these patches, a bug that should happen without \
these patches and won't happen with them.

Try to understand how this code is reached, explore sysfs, procfs, syscalls etc. \
The kernel source is mounted read-only at /linux for you to read.

Then try to write code that triggers a bug avoided by the patch, in userspace or a \
kernel module.

If you can't find a way, at least add a printk to the patch and write a reproducer \
that triggers this printk, so that we'll at least activate this code.

If nothing is possible, explain why in the reproducer.
"""


def progress(phase: str, **extra) -> None:
    if _PROGRESS:
        print(f"{PROGRESS_SENTINEL} {json.dumps({'phase': phase, **extra})}", flush=True)


def patches_from_thread(thread: str) -> str:
    """Fetch a lore thread's patch series as text (reusing run-kernel.py's mbox
    selection), without applying it. Returns the concatenated patch mails."""
    base = thread.rstrip("/")
    if base.endswith("/raw"):
        base = base[: -len("/raw")]
    mbox_url = base + "/t.mbox.gz"
    tmp = Path(tempfile.mkdtemp(prefix="mk-scaffold-thread-"))
    gz = tmp / "t.mbox.gz"
    log(f"downloading thread mbox {mbox_url} ...")
    if run(["curl", "-LfsS", "-A", rk.LORE_UA, "-o", str(gz), mbox_url]).returncode != 0:
        die("thread mbox download failed")
    mbox = tmp / "t.mbox"
    try:
        with gzip.open(gz, "rb") as f:
            mbox.write_bytes(f.read())
    except OSError as e:
        die(f"thread mbox decompress failed: {e}")
    msgs = rk.select_thread_patches(mbox)
    if not msgs:
        die("thread contained no applicable [PATCH] mails with diffs")
    parts = []
    for m in msgs:
        subj = " ".join((m.get("Subject") or "").split())
        parts.append(f"=== {subj} ===\n{rk._message_text(m)}")
    return "\n\n".join(parts)


def build_prompt_md(spec: str, patches: str) -> str:
    """The full instruction file the agent reads (PROMPT.md). Holds the bundle spec,
    the patch series, and the fixed instruction; the agent writes ./repro.md."""
    return f"""\
# Scaffold a kernel reproducer

You are writing a **reproducer bundle**. Produce a single file `./repro.md` in the
current directory that follows the bundle spec below exactly, so it can be run with
`run-kernel.py repro.md`.

## Constraints

Work as a single agent: do NOT spawn sub-agents or use a task/explore delegation
tool. Do all source exploration yourself with your own read/grep/list tools. (The
model backend allows only one request at a time, so a sub-agent would stall.)

## Your task

{INSTRUCTION}

Set `patch-compare: true` (or `thread-compare:`) in the bundle so the runner builds
the kernel both without and with the fix and shows the difference. Put the fix's
patch into a `patch:` fence (or use `thread-compare:` with the thread URL) as the
spec describes.

When done, the bundle MUST be written to `./repro.md` and nothing else is needed.

## Reproducer bundle spec

{spec}

## The patch series that fixes the bug

```
{patches}
```
"""


def build_refine_prompt_md(spec: str, patches: str | None, note: str = "") -> str:
    """PROMPT.md for a refine run: the agent's PRIOR reproducer failed; it must read the
    prior run's logs (mounted as files, not inlined — they can be huge) and fix it."""
    patch_section = f"""\

## The patch series that fixes the bug

```
{patches}
```
""" if patches else ""
    note_section = f"""\

## Additional context from the user

The user added this guidance for the fix — treat it as important:

{note.strip()}
""" if note.strip() else ""
    return f"""\
# Refine a kernel reproducer that failed

You previously wrote the reproducer bundle in `./prev-repro.md`. It was run through
`run-kernel.py` and did **NOT** succeed.

The logs from that failed run are in `./prev-logs/` — read them with your own tools:
- `run.log` — the orchestrator log; carries the failure reason even for early crashes.
- `dmesg`, `console` — the guest kernel ring buffer and raw serial output.
- `compile`, `fetch`, `exec` — the build/fetch/run stages.
(Compare jobs nest these under `prev-logs/baseline/` and `prev-logs/patched/`.)

## Your task

Read `./prev-repro.md` and the logs in `./prev-logs/`, diagnose why the reproducer
failed (didn't build, didn't boot, didn't trigger the bug, wrong patch fence, …), then
write a **corrected** bundle to `./repro.md` following the spec below. Keep the
`patch-compare:`/`thread-compare:` setup the prior bundle used so the runner still builds
without and with the fix.
{note_section}
## Constraints

Work as a single agent: do NOT spawn sub-agents or use a task/explore delegation tool.
Do all source exploration yourself. The kernel source is mounted read-only at `/linux`.

When done, the corrected bundle MUST be written to `./repro.md` and nothing else is needed.

## Reproducer bundle spec

{spec}
{patch_section}"""


def write_opencode_config(work: Path) -> None:
    """Register the user's endpoint as an opencode custom provider in /work/opencode.json
    (cwd is /work, which opencode searches). `@ai-sdk/openai-compatible` is bundled in
    the opencode binary, so no npm fetch happens. The API key stays a `{env:...}` ref so
    it never lands on disk; the base URL and model name are not secret."""
    cfg = {
        "$schema": "https://opencode.ai/config.json",
        "provider": {
            PROVIDER: {
                "npm": "@ai-sdk/openai-compatible",
                "options": {"baseURL": OPENAI_BASE_URL, "apiKey": f"{{env:{API_KEY_ENV}}}"},
                "models": {MODEL: {}},
            }
        },
    }
    (work / "opencode.json").write_text(json.dumps(cfg, indent=2))


def run_opencode(work: Path, wt: Path, arch: str, image: str, is_local: bool) -> None:
    """podman run opencode (agent mode) in `work` (mounted at /work) with the
    worktree at /linux:ro and egress restricted to the provider's servers. Streams its
    output to stdout (the service tees it to a log); kills it after TIMEOUT."""
    mklib.ensure_pulled(image, is_local, mklib.platform_args(arch))
    write_opencode_config(work)
    mounts = ["-v", mklib.volume(wt, "/linux", ro=True),
              "-v", mklib.volume(work, "/work")]

    # The long prompt lives in /work/PROMPT.md; the argv prompt just points the agent
    # at it (keeps us well under ARG_MAX and lets the agent read it with its tools).
    argv_prompt = ("Read ./PROMPT.md in the current directory and follow its "
                   "instructions exactly. Write the finished reproducer bundle to "
                   "./repro.md.")
    # --dangerously-skip-permissions: the agent runs unattended, so it can't answer
    # opencode's interactive permission prompts (it auto-rejects them, e.g. reading
    # the mounted /linux source). The hardened container IS the sandbox (read-only
    # root, dropped caps, egress restricted to opencode), so opencode's own prompt
    # layer is redundant here -- skip it so the agent can read source and write repro.md.
    # Name the container so we can kill it by name: `proc.kill()` only kills the
    # podman client, leaving the container running under conmon (an orphan that also
    # holds the free-tier's single concurrency slot).
    name = f"mk-scaffold-{os.getpid()}"
    # Pass the API key through to the container by name (no `=value`), so it stays out of
    # podman's argv (visible in `ps`); the value is in our env, set by the service.
    cmd = [
        "podman", "run", "--rm", "--name", name, *mklib.platform_args(arch),
        *mklib.scaffold_args(arch), "-e", API_KEY_ENV, *mounts, "-w", "/work", image,
        "opencode", "run", "--dangerously-skip-permissions", "-m", f"{PROVIDER}/{MODEL}", argv_prompt,
    ]
    log("running opencode agent ...")
    proc = subprocess.Popen(cmd, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)

    def kill_container() -> None:
        subprocess.run(["podman", "kill", name],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)

    # Stream the agent's output; kill the container if it overruns the timeout.
    timed_out = False
    deadline = time.monotonic() + TIMEOUT
    assert proc.stdout is not None
    try:
        for line in proc.stdout:
            sys.stdout.write(line)
            sys.stdout.flush()
            if time.monotonic() > deadline:
                timed_out = True
                kill_container()
                proc.kill()
                break
    finally:
        # Leaving the loop with the container still up (timeout, or an exception
        # bubbling out) would orphan it -- kill it by name first.
        if proc.poll() is None:
            kill_container()
    rc = proc.wait()
    if timed_out:
        die(f"opencode agent timed out after {TIMEOUT}s")
    if rc != 0:
        die(f"opencode agent exited with status {rc}")


def main() -> int:
    ap = argparse.ArgumentParser(description="Scaffold a reproducer bundle via opencode.")
    # Source is required for a fresh scaffold but optional for --refine (the prior
    # reproducer carries the patch); validated below.
    src = ap.add_mutually_exclusive_group(required=False)
    src.add_argument("--thread", help="lore.kernel.org thread URL (its [PATCH] series)")
    src.add_argument("--patch-file", help="local unified-diff file to use as the patch")
    ap.add_argument("--commit", default="", help="base commit/tag to explore (default: tree HEAD)")
    ap.add_argument("--out", required=True, help="write the generated bundle here")
    ap.add_argument("--log-dir", help="write scaffold.log here (service mode)")
    ap.add_argument("--progress", action="store_true", help="emit MKPROGRESS phase lines")
    # Refine: fix a prior failed reproducer using its run logs instead of writing one fresh.
    ap.add_argument("--refine", action="store_true", help="refine a prior reproducer (see --prev-repro/--prev-logs)")
    ap.add_argument("--prev-repro", help="the prior reproducer bundle to fix (refine mode)")
    ap.add_argument("--prev-logs", help="dir of the prior run's logs the agent reads (refine mode)")
    ap.add_argument("--refine-note", default="", help="free-text user context to weave into the refine prompt")
    args = ap.parse_args()

    if args.refine:
        if not args.prev_repro or not Path(args.prev_repro).is_file() \
                or not Path(args.prev_repro).read_text(errors="replace").strip():
            die("--refine needs a non-empty --prev-repro")
    elif not args.thread and not args.patch_file:
        die("one of --thread / --patch-file is required")

    global _PROGRESS
    _PROGRESS = args.progress
    os.chdir(HERE)

    # No free tier: the user's OpenAI-compatible endpoint + key + model are required
    # (the service forwards them as env from the request). Fail clearly if any is unset.
    missing = [n for n, v in (("MK_OPENAI_BASE_URL", OPENAI_BASE_URL),
                              ("MK_OPENAI_API_KEY", OPENAI_API_KEY),
                              ("MK_OPENCODE_MODEL", MODEL)) if not v]
    if missing:
        die(f"missing OpenAI credentials: {', '.join(missing)}")

    arch = mklib.target_arch()
    image, is_local = mklib.resolve_image(arch, opencode=True)
    base_src = Path(os.environ.get("LINUX_SRC", os.path.expanduser("~/linux")))

    log_dir = Path(args.log_dir).resolve() if args.log_dir else None
    if log_dir:
        log_dir.mkdir(parents=True, exist_ok=True)

    # 1. Worktree at the base commit for the agent to explore (read-only). The bug to
    #    reproduce lives in the UNpatched code, so we don't apply the patch here; the
    #    patch diff is supplied to the agent as text in the prompt instead.
    progress("prepare")
    # With a commit, fetch+worktree it from Linus's tree (hardened: always that tree);
    # with none, prepare_kernel_tree builds a worktree at the local tree's HEAD.
    meta = {"url": rk.KERNEL_URL, "commit": args.commit} if args.commit else {}
    wt = rk.prepare_kernel_tree(meta, base_src,
                                log_path=(log_dir / "fetch.log") if log_dir else None)
    log(f"kernel tree for exploration: {wt}")

    # 2. Patch text: from the thread (fetched, not applied) or the local file. Optional in
    #    refine mode — there the patch already lives inside ./prev-repro.md.
    if args.thread:
        patches = patches_from_thread(args.thread)
    elif args.patch_file:
        patches = Path(args.patch_file).read_text(errors="replace")
    else:
        patches = None

    # 3. Work dir, mounted rw at /work; the worktree is mounted ro at /linux. Created
    #    under HERE (podman needs a host-shared path) and removed on exit so these
    #    scratch dirs don't pile up in the repo / on the server.
    work = Path(tempfile.mkdtemp(prefix=".mk-scaffold-", dir=HERE))
    try:
        spec = (HERE / "docs" / "reproducer-spec.md").read_text()
        if args.refine:
            # Bring the prior reproducer + its logs into /work so the agent reads them.
            shutil.copyfile(args.prev_repro, work / "prev-repro.md")
            if args.prev_logs and Path(args.prev_logs).is_dir():
                shutil.copytree(args.prev_logs, work / "prev-logs")
            (work / "PROMPT.md").write_text(build_refine_prompt_md(spec, patches, args.refine_note))
        else:
            (work / "PROMPT.md").write_text(build_prompt_md(spec, patches))
        # Seed the patch as a file too, so the agent can drop it straight into a fence.
        if patches:
            (work / "fix.patch").write_text(patches)

        # 4. Run the agent.
        progress("agent")
        run_opencode(work, wt, arch, image, is_local)

        # 5. Collect the bundle the agent wrote.
        produced = work / "repro.md"
        if not produced.is_file() or not produced.read_text().strip():
            die("opencode did not produce a repro.md")
        out = Path(args.out)
        out.parent.mkdir(parents=True, exist_ok=True)
        out.write_text(produced.read_text())
        log(f"wrote bundle to {out}")
        progress("done")
    finally:
        shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    sys.exit(main())
