import { useMemo, useState } from "react";
import MDEditor from "@uiw/react-md-editor";
import { parseBundle } from "../bundle";
import type { Theme } from "../lib/theme";
import { BundlePreview } from "./BundlePreview";
import { Modal } from "./ui/Modal";
import { SegTabs } from "./ui/Tabs";

// The one place to review a bundle before running it: toggle between editing the raw
// markdown and the structured reproducer view (highlighted C/kconf/bash), then run.
// Opened automatically on paste or when an example is picked.
export function BundleModal(
  { bundle, theme, onChange, onRun, onRefine, onClose }:
  { bundle: string; theme: Theme; onChange: (s: string) => void; onRun: () => void;
    onRefine: (bundle: string, note: string) => void; onClose: () => void },
) {
  const [view, setView] = useState<"edit" | "repro">("edit");
  // Optional prompt that guides "Refine" — the agent improves the current bundle with it.
  const [note, setNote] = useState("");
  const parsed = useMemo(() => parseBundle(bundle), [bundle]);

  return (
    <Modal onClose={onClose} label="Edit reproducer bundle">
      <div className="flex items-center justify-between gap-2">
        <SegTabs
          label="bundle view"
          value={view}
          onChange={(v) => setView(v as "edit" | "repro")}
          items={[{ key: "edit", label: "Edit" }, { key: "repro", label: "Reproducer" }]}
        />
        <div className="flex items-center gap-2">
          <input
            className="w-48 rounded-md border border-border bg-bg px-2 py-1 text-sm text-fg outline-none focus:border-accent"
            placeholder="refine prompt (optional)" value={note} onChange={(e) => setNote(e.target.value)} />
          <button className="chip" title="Send this bundle + the prompt to the agent to improve it"
            onClick={() => onRefine(bundle, note)} disabled={!bundle.trim()}>Refine ✨</button>
          <button className="btn" onClick={onRun} disabled={!bundle.trim()}>Run reproducer</button>
        </div>
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
