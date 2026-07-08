# Reproducer bundle — spec

A **reproducer** is a single self-contained Markdown file describing a complete
kernel repro: an optional kernel source, userspace programs, kernel modules,
extra Kconfig, and a start script. It is run with:

```bash
./run-kernel.py repro.md
```

`run-kernel.py` builds everything, boots the kernel under QEMU against an Ubuntu
cloud image, runs the start script in the guest over SSH, streams its output, and
**exits with the guest script's status**. A reproducer is "reproduced" when that
exit status is non-zero (or whatever the repro defines as failure).

## File format

A bundle is normal Markdown. Two kinds of content are meaningful; everything else
is prose and ignored.

### 1. Metadata block (optional)

A `---`-delimited block, at column 0 and outside any code fence, holding
`key: value` lines. Recognized keys (others ignored):

| Key              | Meaning                                                          |
|------------------|------------------------------------------------------------------|
| `url`            | git remote to fetch the kernel from                              |
| `commit`         | commit / tag / branch to build (e.g. `v6.12`)                    |
| `patch`          | URL or path to a patch applied on top of `commit`                |
| `thread`         | lore.kernel.org thread URL; its `[PATCH n/m]` series is `git am`'d on top of `commit` |
| `arch`           | target arch (`x86_64` / `arm64`); overrides `ARCH` env and host  |
| `tools`          | space-separated kernel-tree userspace tools to build and ship into the guest (currently `perf`, `bpftool`); the reproducer can then invoke them directly (they're on `PATH`). Built from the job's tree, so version-matched to the kernel. |
| `patch-compare`  | `true` to run twice — with and without `patch:` — in parallel    |
| `thread-compare` | lore thread URL; run baseline vs the thread's series, in parallel|
| `commit-compare` | two whitespace-separated commit-ishes; run the first (baseline) vs the second (patched), in parallel |
| `search-dmesg`   | literal string to hunt for in the serial console (`console.log`); matches are flagged like a `BUG:` and shown at the top of the Issues view. Repeatable; does **not** change the run's pass/fail. |
| `regex-dmesg`    | same as `search-dmesg`, but the value is a regular expression. Repeatable. |
| `search-user`    | literal string to hunt for in userspace output (`exec.log` — the reproducer's own stdout/stderr in the guest); flagged and shown in Issues just like `search-dmesg`. Repeatable; does **not** change pass/fail. |
| `regex-user`     | same as `search-user`, but the value is a regular expression. Repeatable. |
| `summary`        | one-line description of the reproducer, shown in the UI's "More…" examples list. UI-only — ignored by the runner. |
| `tag`            | free-form label for grouping/filtering in the UI's "More…" examples list (e.g. `bpf`, `uaf`). Repeatable. UI-only — ignored by the runner. |

`summary` and `tag` are **UI-only**: they surface in the Examples "More…"
browser (its per-example summary and tag filter) and never change the build or
the run's pass/fail. `tag` is repeatable, like `search-dmesg`.

With no metadata block, the kernel at `LINUX_SRC` (default `~/linux`) is built
as-is. Arch precedence: frontmatter `arch:` > `ARCH` env > host arch.

`search-dmesg` / `regex-dmesg` are scanned against the captured serial console,
and `search-user` / `regex-user` against the reproducer's userspace output,
*after* the run; they only surface matching lines (so you can spot a custom
`pr_info`, a program's own "FAIL"/"ok" line, a known error string, or a sanitizer
line the built-in `BUG:`/oops detection misses) — they never affect the exit status.

```
---
url: git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.12
patch: https://example.com/fix.patch
arch: x86_64
---
```

#### Comparison runs (`patch-compare` / `thread-compare`)

Any of these keys turns one bundle into **two runs, executed in parallel** — a
**baseline** and a **patched** variant — so you can see what a patch (or a
mailing-list series, or a range of commits) changes. They build on separate
worktrees and boot separate guests.

- `patch-compare: true` (requires a patch — either a `patch:` URL or an inline
  `patch` fence) — baseline is `commit:` with the patch *stripped*; patched is
  `commit:` with the patch applied (`git apply`).
- `thread-compare: <lore-thread-url>` — baseline is plain `commit:`; patched is
  `commit:` with the thread's whole patch series applied. The series is fetched as
  the thread mbox (`<url>/t.mbox.gz`) and applied with `git am` (cover letter and
  non-patch replies are dropped).
- `commit-compare: <baseline> <patched>` — two whitespace-separated commit-ishes
  (commit / tag / branch). Baseline builds the first, patched builds the second;
  both come from the canonical tree with no patch applied. Any `commit:` key is
  ignored in this mode.

If more than one compare key is set, precedence is
`patch-compare` > `thread-compare` > `commit-compare`.

With `--log-dir DIR`, each variant writes its own subdir of the usual logs:
`DIR/baseline/` and `DIR/patched/`. The runs are reported verbatim (both exit codes
are printed); the invocation's overall exit status follows the **patched** run. The
dashboard shows the two variants side by side.

```
---
commit: v6.12
patch: https://example.com/fix.patch
patch-compare: true
---
```

```
---
commit: v6.12
thread-compare: https://lore.kernel.org/lkml/cover.123@example/
---
```

```
---
commit-compare: v6.11 v6.12
---
```

A self-contained `patch-compare` with an **inline patch** (no external URL) — the
patch sets `EXTRAVERSION`, so `uname -r` differs between the two runs:

`````
---
commit: v6.19
patch-compare: true
---

# Does this Makefile patch change `uname -r`?

```patch:extraversion.patch
--- a/Makefile
+++ b/Makefile
@@ -2,5 +2,5 @@
 VERSION = 6
 PATCHLEVEL = 19
 SUBLEVEL = 0
-EXTRAVERSION =
+EXTRAVERSION = -patchcompare
 NAME = Baby Opossum Posse
```

```init:init.sh
#!/bin/bash
echo "RELEASE=$(uname -r)"   # baseline: 6.19.0 · patched: 6.19.0-patchcompare
```
`````

### 2. Role-tagged code fences

Fenced blocks whose info string is `role:filename` contribute files to the build.
Multiple blocks per role are allowed.

| Role      | Purpose                                                        |
|-----------|----------------------------------------------------------------|
| `user`    | userspace source/headers, compiled to a guest binary           |
| `module`  | out-of-tree kernel module source, built against the kernel     |
| `kconf`   | extra Kconfig fragment, merged into the kernel config          |
| `patch`   | a unified diff applied to the kernel tree (`git apply`) — an inline alternative to the `patch:` URL |
| `init`    | start script run in the guest; its exit status is the result   |

````
```user:file.c
int main(void) { return 1; }
```

```module:greeter.c
/* ... module source ... */
```

```kconf:extra.config
CONFIG_DEBUG_INFO=y
```

```init:init.sh
#!/bin/bash
set -e
./file          # compiled userspace binary
```
````

## Execution model

1. Resolve kernel tree — `LINUX_SRC` as-is, or a cached git worktree at `commit`
   (fetching `url`) with `patch` and/or a `thread:` patch series applied.
2. Merge `kconf:` fragments and build the kernel + any `module:` sources.
3. Compile `user:` files into guest binaries.
4. Boot the kernel under QEMU (HVF/KVM/TCG) against the Ubuntu cloud image.
5. Copy the artifacts in and run `init:` over SSH, streaming output.
6. Exit with the `init:` script's status.

With `patch-compare` / `thread-compare` / `commit-compare`, steps 1–6 run twice in
parallel (a baseline and a patched variant, each on its own worktree and guest); the overall
exit status is the patched run's.

Guest network egress is restricted by default (override with `GUEST_NET=open`);
the QEMU process can be further confined with `MK_SANDBOX`.

See `examples/greeter.md` for a complete working bundle.
