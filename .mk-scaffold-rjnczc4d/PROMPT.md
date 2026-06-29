# Scaffold a kernel reproducer

You are writing a **reproducer bundle**. Produce a single file `./repro.md` in the
current directory that follows the bundle spec below exactly, so it can be run with
`run-kernel.py repro.md`.

## Constraints

Work as a single agent: do NOT spawn sub-agents or use a task/explore delegation
tool. Do all source exploration yourself with your own read/grep/list tools. (The
model backend allows only one request at a time, so a sub-agent would stall.)

## Your task

Try to reproduce a bug fixed by these patches, a bug that should happen without these patches and won't happen with them.

Try to understand how this code is reached, explore sysfs, procfs, syscalls etc. The kernel source is mounted read-only at /linux for you to read.

Then try to write code that triggers a bug avoided by the patch, in userspace or a kernel module.

If you can't find a way, at least add a printk to the patch and write a reproducer that triggers this printk, so that we'll at least activate this code.

If nothing is possible, explain why in the reproducer.


Set `patch-compare: true` (or `thread-compare:`) in the bundle so the runner builds
the kernel both without and with the fix and shows the difference. Put the fix's
patch into a `patch:` fence (or use `thread-compare:` with the thread URL) as the
spec describes.

When done, the bundle MUST be written to `./repro.md` and nothing else is needed.

## Reproducer bundle spec

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
| `patch-compare`  | `true` to run twice — with and without `patch:` — in parallel    |
| `thread-compare` | lore thread URL; run baseline vs the thread's series, in parallel|
| `search-dmesg`   | literal string to hunt for in the serial console (`console.log`); matches are flagged like a `BUG:` and shown at the top of the Issues view. Repeatable; does **not** change the run's pass/fail. |
| `regex-dmesg`    | same as `search-dmesg`, but the value is a regular expression. Repeatable. |

With no metadata block, the kernel at `LINUX_SRC` (default `~/linux`) is built
as-is. Arch precedence: frontmatter `arch:` > `ARCH` env > host arch.

`search-dmesg` / `regex-dmesg` are scanned against the captured serial console
*after* the run; they only surface matching lines (so you can spot a custom
`pr_info`, a known error string, or a sanitizer line the built-in `BUG:`/oops
detection misses) — they never affect the exit status.

```
---
url: git://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git
commit: v6.12
patch: https://example.com/fix.patch
arch: x86_64
---
```

#### Comparison runs (`patch-compare` / `thread-compare`)

Either key turns one bundle into **two runs, executed in parallel** — a **baseline**
and a **patched** variant — so you can see what a patch (or a mailing-list series)
changes. They build on separate worktrees and boot separate guests.

- `patch-compare: true` (requires a patch — either a `patch:` URL or an inline
  `patch` fence) — baseline is `commit:` with the patch *stripped*; patched is
  `commit:` with the patch applied (`git apply`).
- `thread-compare: <lore-thread-url>` — baseline is plain `commit:`; patched is
  `commit:` with the thread's whole patch series applied. The series is fetched as
  the thread mbox (`<url>/t.mbox.gz`) and applied with `git am` (cover letter and
  non-patch replies are dropped).

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

With `patch-compare` / `thread-compare`, steps 1–6 run twice in parallel (a
baseline and a patched variant, each on its own worktree and guest); the overall
exit status is the patched run's.

Guest network egress is restricted by default (override with `GUEST_NET=open`);
the QEMU process can be further confined with `MK_SANDBOX`.

See `examples/greeter.md` for a complete working bundle.


## The patch series that fixes the bug

```
=== [PATCH bpf] bpf: Reject negative head_room in __bpf_skb_change_head ===
Yinhao et al. recently reported:

  Our fuzzing tool was able to create a BPF program which triggered
  the below BUG condition inside pskb_expand_head.

  [   23.016047][T10006] kernel BUG at net/core/skbuff.c:2232!
  [...]
  [   23.017301][T10006] RIP: 0010:pskb_expand_head+0x1519/0x1530
  [...]
  [   23.021249][T10006] Call Trace:
  [   23.021387][T10006]  <TASK>
  [   23.021507][T10006]  ? __pfx_pskb_expand_head+0x10/0x10
  [   23.021725][T10006]  __bpf_skb_change_head+0x22a/0x520
  [   23.021939][T10006]  bpf_skb_change_head+0x34/0x1b0
  [   23.022143][T10006]  ___bpf_prog_run+0xf70/0xb670
  [   23.022342][T10006]  __bpf_prog_run32+0xed/0x140
  [...]

The problem is that in __bpf_skb_change_head() we need to reject a
negative head_room as otherwise this propagates all the way to the
pskb_expand_head() from skb_cow(). For example, if the BPF test infra
passes a skb with gso_skb:1 to the BPF helper with a negative head_room
of -22, then this gets passed into skb_cow(). __skb_cow() in this
example calculates a delta of -86 which gets aligned to -64, and then
triggers BUG_ON(nhead < 0). Thus, reject malformed negative input.

Fixes: 3a0af8fd61f9 ("bpf: BPF for lightweight tunnel infrastructure")
Reported-by: Yinhao Hu <dddddd@hust.edu.cn>
Reported-by: Kaiyan Mei <M202472210@hust.edu.cn>
Reviewed-by: Dongliang Mu <dzm91@hust.edu.cn>
Signed-off-by: Daniel Borkmann <daniel@iogearbox.net>
---
 net/core/filter.c | 3 ++-
 1 file changed, 2 insertions(+), 1 deletion(-)

diff --git a/net/core/filter.c b/net/core/filter.c
index 76628df1fc82..fa06c5a08e22 100644
--- a/net/core/filter.c
+++ b/net/core/filter.c
@@ -3877,7 +3877,8 @@ static inline int __bpf_skb_change_head(struct sk_buff *skb, u32 head_room,
 	u32 new_len = skb->len + head_room;
 	int ret;
 
-	if (unlikely(flags || (!skb_is_gso(skb) && new_len > max_len) ||
+	if (unlikely(flags || (int)head_room < 0 ||
+		     (!skb_is_gso(skb) && new_len > max_len) ||
 		     new_len < skb->len))
 		return -EINVAL;
 
-- 
2.43.0


```
