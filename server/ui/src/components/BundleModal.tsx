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
  // Collected in a popped modal (below) rather than an inline box.
  const [note, setNote] = useState("");
  const [refineOpen, setRefineOpen] = useState(false);
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
          <button className="chip" title="Send this bundle to the agent to improve it"
            onClick={() => setRefineOpen(true)} disabled={!bundle.trim()}>Refine ✨</button>
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
      {refineOpen && (
        <Modal onClose={() => setRefineOpen(false)} label="Refine reproducer">
          <h2>Refine reproducer ✨</h2>
          <p className="text-muted mb-2">
            The agent gets this bundle and is asked to improve it. Add any context that should
            guide the fix (optional) — e.g. what actually failed, a config to enable, or where
            to focus.
          </p>
          <textarea
            className="mb-3 w-full box-border rounded-md border border-border bg-bg p-[9px] font-mono text-fg outline-none focus:border-accent"
            rows={5} autoFocus placeholder="optional context for the agent…"
            value={note} onChange={(e) => setNote(e.target.value)} />
          <div className="flex justify-end">
            <button className="btn"
              onClick={() => { setRefineOpen(false); onRefine(bundle, note); setNote(""); }}>
              Refine
            </button>
          </div>
        </Modal>
      )}
    </Modal>
  );
}
