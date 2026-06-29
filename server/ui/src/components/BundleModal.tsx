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
  { bundle, theme, onChange, onRun, onClose }:
  { bundle: string; theme: Theme; onChange: (s: string) => void; onRun: () => void; onClose: () => void },
) {
  const [view, setView] = useState<"edit" | "repro">("edit");
  const parsed = useMemo(() => parseBundle(bundle), [bundle]);

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
