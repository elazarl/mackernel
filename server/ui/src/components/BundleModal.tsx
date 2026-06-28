import { useMemo, useState } from "react";
import MDEditor from "@uiw/react-md-editor";
import { compareMode, parseBundle, upsertMeta } from "../bundle";
import type { Theme } from "../lib/theme";
import { BundlePreview } from "./BundlePreview";
import { Modal } from "./ui/Modal";
import { SegTabs } from "./ui/Tabs";

// The one place to review a bundle before running it: toggle between editing the raw
// markdown and the structured reproducer view (highlighted C/kconf/bash), then run.
// Opened automatically on paste or when an example is picked.
export function BundleModal(
  { bundle, theme, onChange, onRun, onClose }:
  { bundle: string; theme: Theme; onChange: (s: string) => void; onRun: () => void; onClose: () => void },
) {
  const [view, setView] = useState<"edit" | "repro">("edit");
  const parsed = useMemo(() => parseBundle(bundle), [bundle]);
  const cmp = compareMode(parsed);
  const threadVal = parsed.meta.find((m) => m.key === "thread-compare")?.value ?? "";

  return (
    <Modal onClose={onClose} label="Edit reproducer bundle">
      <div className="flex items-center justify-between">
        <SegTabs
          label="bundle view"
          value={view}
          onChange={(v) => setView(v as "edit" | "repro")}
          items={[{ key: "edit", label: "Edit" }, { key: "repro", label: "Reproducer" }]}
        />
        <button className="btn" onClick={onRun} disabled={!bundle.trim()}>Run reproducer</button>
      </div>
      {/* Compare toggles: write into the frontmatter so the run produces baseline +
          patched side by side. patch-compare needs a patch: in the bundle. */}
      <div className="mb-2 flex flex-wrap items-center gap-1.5">
        <label className="inline-flex items-center gap-1.5 text-sm text-muted">
          <input type="checkbox" checked={cmp === "patch"}
            onChange={(e) => onChange(upsertMeta(bundle, "patch-compare", e.target.checked ? "true" : "false"))} />
          Compare with / without the patch
        </label>
        <span className="mx-0.5 w-px self-stretch bg-border" />
        <label className="inline-flex items-center gap-1.5 text-sm text-muted">
          Compare vs lore thread:
          <input type="text" placeholder="https://lore.kernel.org/…" value={threadVal}
            onChange={(e) => onChange(upsertMeta(bundle, "thread-compare", e.target.value.trim()))}
            className="w-70 rounded-md border border-border bg-bg px-1.5 py-1 font-mono text-xs text-fg" />
        </label>
      </div>
      {view === "edit" ? (
        <div data-color-mode={theme}>
          <MDEditor value={bundle} onChange={(v) => onChange(v ?? "")} height={460} />
        </div>
      ) : (
        <BundlePreview parsed={parsed} />
      )}
    </Modal>
  );
}
