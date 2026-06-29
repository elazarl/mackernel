// Client-side parser for reproducer bundles (see docs/reproducer-spec.md). Used to
// render a structured preview (frontmatter metadata + per-role code tabs) instead of
// the raw text. Display-only: the raw bundle is still POSTed and re-parsed server-side.

export interface BundleMeta { key: string; value: string; }
export interface BundleFile { role: string; name: string; body: string; }
export interface ParsedBundle { meta: BundleMeta[]; files: BundleFile[]; }

// Recognized metadata keys and the canonical tab order (per the spec). Roles not in
// this list still get a tab, ordered after the known ones.
const RECOGNIZED_META = ["url", "commit", "patch", "thread", "arch", "patch-compare", "thread-compare"];
export const ROLE_ORDER = ["user", "module", "kconf", "patch", "init"];

const FENCE_OPEN = /^(`{3,})(.*)$/;     // ```role:filename  (or any info string)
const ROLE_INFO = /^(\w+):(.+)$/;       // role:filename
const KV = /^([A-Za-z][\w-]*):\s*(.+)$/;

export function parseBundle(text: string): ParsedBundle {
  const lines = text.split(/\r?\n/);
  const meta: BundleMeta[] = [];
  const files: BundleFile[] = [];

  let i = 0;
  let fence: string | null = null; // backtick run that opened the current fence
  let cur: BundleFile | null = null;
  let metaParsed = false;

  while (i < lines.length) {
    const line = lines[i];

    if (fence === null) {
      // Outside a code fence.
      const open = line.match(FENCE_OPEN);
      if (open) {
        fence = open[1];
        const info = open[2].trim();
        const role = info.match(ROLE_INFO);
        cur = role ? { role: role[1], name: role[2].trim(), body: "" } : null;
        i++;
        continue;
      }

      // Frontmatter: a `---` at column 0 (not indented — the spec is explicit), with
      // key:value lines, closed by another column-0 `---`. Only the first one counts.
      if (!metaParsed && line === "---") {
        const block: string[] = [];
        let j = i + 1;
        let closed = false;
        while (j < lines.length) {
          if (lines[j] === "---") { closed = true; break; }
          block.push(lines[j]);
          j++;
        }
        const nonBlank = block.filter((l) => l.trim() !== "");
        if (closed && nonBlank.length > 0 && nonBlank.every((l) => KV.test(l.trim()))) {
          for (const l of nonBlank) {
            const m = l.trim().match(KV)!;
            if (RECOGNIZED_META.includes(m[1])) meta.push({ key: m[1], value: m[2].trim() });
          }
          metaParsed = true;
          i = j + 1; // skip past the closing ---
          continue;
        }
        // Not a metadata block (e.g. a thematic break) — fall through as prose.
      }
      i++;
    } else {
      // Inside a code fence: close on a line of >= the opening number of backticks.
      if (new RegExp("^`{" + fence.length + ",}\\s*$").test(line)) {
        if (cur) files.push(cur);
        cur = null;
        fence = null;
        i++;
        continue;
      }
      if (cur) cur.body += (cur.body ? "\n" : "") + line;
      i++;
    }
  }

  files.sort((a, b) => roleRank(a.role) - roleRank(b.role));
  return { meta, files };
}

function roleRank(role: string): number {
  const idx = ROLE_ORDER.indexOf(role);
  return idx === -1 ? ROLE_ORDER.length : idx;
}

// Does a parsed bundle request a baseline-vs-patched comparison? Returns the mode
// ("patch" strips the bundle's patch:; "thread" git-ams a lore series) or null. Both
// modes produce baseline/patched runs, so the UI renders either side by side.
const TRUTHY = ["1", "true", "yes", "on"];
export function compareMode(parsed: ParsedBundle): "patch" | "thread" | null {
  const get = (k: string) => parsed.meta.find((m) => m.key === k)?.value;
  const pc = get("patch-compare");
  // A patch can come from the patch: key or an inline ```patch:… fence.
  const hasPatch = !!get("patch") || parsed.files.some((f) => f.role === "patch");
  if (pc && TRUTHY.includes(pc.trim().toLowerCase()) && hasPatch) return "patch";
  if (get("thread-compare")) return "thread";
  return null;
}

// Distinct roles present, in canonical order — the set of tabs to show.
export function rolesOf(parsed: ParsedBundle): string[] {
  const seen: string[] = [];
  for (const f of parsed.files) if (!seen.includes(f.role)) seen.push(f.role);
  return seen.sort((a, b) => roleRank(a) - roleRank(b));
}

// --- hardened-mode constants (must match KERNEL_URL in run-kernel.py) ----------
// The kernel is always built from Linus's tree, so a commit tree-ish maps to GitHub.
export const KERNEL_URL =
  "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git";
export const githubTree = (ref: string) =>
  `https://github.com/torvalds/linux/tree/${encodeURIComponent(ref)}`;

// --- raw-mode editing helpers --------------------------------------------------

// Line indices of the column-0 `---…---` frontmatter block (outside any fence), or
// null. Reuses the fence tracking from parseBundle so `---` inside code is ignored.
function frontmatterRange(lines: string[]): { open: number; close: number } | null {
  const dashes: number[] = [];
  let fence: string | null = null;
  for (let i = 0; i < lines.length; i++) {
    const line = lines[i];
    if (fence === null) {
      const m = line.match(FENCE_OPEN);
      if (m) { fence = m[1]; continue; }
      if (line === "---") dashes.push(i);
    } else if (new RegExp("^`{" + fence.length + ",}\\s*$").test(line)) {
      fence = null;
    }
  }
  return dashes.length >= 2 ? { open: dashes[0], close: dashes[1] } : null;
}

// Set a frontmatter key: update it in place if present, insert into the existing
// block, or prepend a new `---` block when the bundle has none.
export function upsertMeta(text: string, key: string, value: string): string {
  const lines = text.split(/\r?\n/);
  const kv = `${key}: ${value}`;
  const keyRe = new RegExp(`^${key}\\s*:`);
  const fm = frontmatterRange(lines);
  if (fm) {
    for (let i = fm.open + 1; i < fm.close; i++) {
      if (keyRe.test(lines[i].trim())) { lines[i] = kv; return lines.join("\n"); }
    }
    lines.splice(fm.close, 0, kv);
    return lines.join("\n");
  }
  return `---\n${kv}\n---\n\n${text}`;
}

// Boilerplate for the "add file" buttons, keyed by role.
export const BOILERPLATE: Record<string, { name: string; body: string }> = {
  user: { name: "repro.c", body: '#include <stdio.h>\n\nint main(void) {\n    return 0;\n}\n' },
  module: {
    name: "mod.c",
    body: '#include <linux/module.h>\n#include <linux/kernel.h>\n\n' +
      'static int __init mod_init(void) {\n    pr_info("mod: loaded\\n");\n    return 0;\n}\n' +
      'static void __exit mod_exit(void) { }\n\n' +
      'module_init(mod_init);\nmodule_exit(mod_exit);\nMODULE_LICENSE("GPL");\n',
  },
  kconf: { name: "extra.config", body: 'CONFIG_DEBUG_INFO=y\n' },
  init: { name: "init.sh", body: '#!/bin/bash\nset -e\n' },
};

// Ready-to-run example reproducers, loadable from the UI with one click. Each is a
// complete bundle string (see docs/reproducer-spec.md); `label`/`blurb` describe it.
export interface Example { label: string; blurb: string; bundle: string; }

export const EXAMPLES: Example[] = [
  {
    label: "greeter",
    blurb: "userspace + module + init; pinned to v6.19",
    bundle: `---
commit: v6.19
---

# greeter — minimal bundle

Builds a tiny userspace program and a kernel module, boots, loads the module,
and runs the start script. The program exits 1, which becomes the run's status.

\`\`\`user:file.c
#include <stdio.h>
#include "file.h"

int main(void) {
    printf("hello from userspace, returning %d\\n", R);
    return R;
}
\`\`\`

\`\`\`user:file.h
#define R 1
\`\`\`

\`\`\`module:greeter.c
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

static int __init greeter_init(void)
{
    pr_info("greeter: module loaded\\n");
    return 0;
}

static void __exit greeter_exit(void)
{
    pr_info("greeter: module unloaded\\n");
}

module_init(greeter_init);
module_exit(greeter_exit);
MODULE_LICENSE("GPL");
MODULE_DESCRIPTION("mackernel bundle example module");
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
set -e
echo "loaded modules:"; lsmod | grep greeter || true
echo "kernel says:"; sudo dmesg | grep greeter || true
./file
\`\`\`
`,
  },
  {
    label: "null-deref panic",
    blurb: "module dereferences NULL on load; pinned to v6.12",
    bundle: `---
commit: v6.12
---

# null-deref — a module that oopses on insmod

Pins the kernel to the v6.12 tag, then loads a module whose init deliberately
writes through a NULL pointer. The oops shows up in \`dmesg\`.

\`\`\`kconf:extra.config
CONFIG_PRINTK_CALLER=y
\`\`\`

\`\`\`module:oops.c
#include <linux/module.h>
#include <linux/kernel.h>
#include <linux/init.h>

static int __init oops_init(void)
{
    int *p = NULL;
    pr_info("oops: about to dereference NULL\\n");
    *p = 0xdead;          /* NULL pointer dereference */
    return 0;
}

static void __exit oops_exit(void) { }

module_init(oops_init);
module_exit(oops_exit);
MODULE_LICENSE("GPL");
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
# Reproduced (exit non-zero) if the module's NULL deref left an oops in dmesg.
if sudo dmesg | grep -i -A20 'BUG:\\|Oops:\\|null pointer'; then
    echo "oops detected — reproduced"; exit 1
fi
echo "no oops found"; exit 0
\`\`\`
`,
  },
  {
    label: "userspace syscall",
    blurb: "userspace-only repro exercising a syscall; pinned to v6.19",
    bundle: `---
commit: v6.19
---

# uname — userspace-only bundle

Pinned to v6.19, no module — just a C program run in the guest. Reports the
running kernel release via the \`uname(2)\` syscall.

\`\`\`user:repro.c
#include <stdio.h>
#include <sys/utsname.h>

int main(void) {
    struct utsname u;
    if (uname(&u) != 0) { perror("uname"); return 1; }
    printf("kernel: %s %s\\n", u.sysname, u.release);
    return 0;
}
\`\`\`
`,
  },
  {
    label: "patch-compare",
    blurb: "v6.12 baseline vs an inline patch, side by side",
    bundle: `---
commit: v6.12
patch-compare: true
---

# patch-compare — baseline vs patched, in parallel

Builds and runs v6.12 twice: **baseline** (patch stripped) and **patched** (patch
applied), side by side. The patch is inline and self-contained — it bumps
\`EXTRAVERSION\`, so \`uname -r\` differs between the two runs. Edit the fence to try
your own change, or swap it for a \`patch:\` URL.

\`\`\`patch:extraversion.patch
--- a/Makefile
+++ b/Makefile
@@ -2,5 +2,5 @@
 VERSION = 6
 PATCHLEVEL = 12
 SUBLEVEL = 0
-EXTRAVERSION =
+EXTRAVERSION = -patched
 NAME = Baby Opossum Posse
\`\`\`

\`\`\`init:init.sh
#!/bin/bash
set -e
echo "kernel release: $(uname -r)"   # baseline: 6.12.0 · patched: 6.12.0-patched
\`\`\`
`,
  },
  {
    label: "thread-compare",
    blurb: "v6.18 baseline vs a real LKML series, side by side",
    bundle: `---
commit: v6.18
thread-compare: https://lore.kernel.org/r/20260116111906.3413346-2-Qing-wu.Li@leica-geosystems.com.cn
---

# thread-compare — baseline vs an LKML series, in parallel

Downloads the patch series from a real lore thread (a two-patch i2c-imx
block-read fix) as an mbox and \`git am\`s it onto v6.18 for the **patched** run;
**baseline** is plain v6.18 — so the two source trees differ by exactly that
series. Replace the \`thread-compare:\` URL with the lore thread you want to
evaluate.

\`\`\`init:init.sh
#!/bin/bash
set -e
echo "kernel release: $(uname -r)"
sudo dmesg | tail -n 20
\`\`\`
`,
  },
];

// Append a ```role:name fenced block (blank-line separated) to the bundle text.
export function appendFile(text: string, role: string, name: string, body: string): string {
  const block = "```" + role + ":" + name + "\n" + body.replace(/\n+$/, "") + "\n```\n";
  const base = text.replace(/\s+$/, "");
  return base === "" ? block : base + "\n\n" + block;
}
