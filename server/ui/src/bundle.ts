// Client-side parser for reproducer bundles (see docs/reproducer-spec.md). Used to
// render a structured preview (frontmatter metadata + per-role code tabs) instead of
// the raw text. Display-only: the raw bundle is still POSTed and re-parsed server-side.

export interface BundleMeta { key: string; value: string; }
export interface BundleFile { role: string; name: string; body: string; }
export interface ParsedBundle { meta: BundleMeta[]; files: BundleFile[]; }

// Recognized metadata keys and the canonical tab order (per the spec). Roles not in
// this list still get a tab, ordered after the known ones.
const RECOGNIZED_META = ["url", "commit", "patch", "arch"];
export const ROLE_ORDER = ["user", "module", "kconf", "init"];

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

// Append a ```role:name fenced block (blank-line separated) to the bundle text.
export function appendFile(text: string, role: string, name: string, body: string): string {
  const block = "```" + role + ":" + name + "\n" + body.replace(/\n+$/, "") + "\n```\n";
  const base = text.replace(/\s+$/, "");
  return base === "" ? block : base + "\n\n" + block;
}
