import { useState } from "react";
import { DISK_EXAMPLES, EXAMPLE_TAGS } from "../bundle";
import { Modal } from "./ui/Modal";

// The "More…" examples browser: lists every on-disk examples/*.md (loaded at build
// time in bundle.ts) with its authored summary + tags, filterable by tag. Picking one
// hands its raw bundle back to the Dashboard, which opens it in the BundleModal.
export function ExamplesModal(
  { onPick, onClose }: { onPick: (bundle: string) => void; onClose: () => void },
) {
  // Selected tag filters. Empty = show all; otherwise an example must carry every
  // selected tag (AND facet).
  const [active, setActive] = useState<Set<string>>(new Set());
  const toggle = (t: string) =>
    setActive((prev) => {
      const next = new Set(prev);
      if (next.has(t)) next.delete(t);
      else next.add(t);
      return next;
    });

  const shown = DISK_EXAMPLES.filter((e) => [...active].every((t) => e.tags.includes(t)));

  return (
    <Modal onClose={onClose} label="Browse examples">
      <h2>Examples <span className="text-muted">· {shown.length} of {DISK_EXAMPLES.length}</span></h2>

      <div className="mb-3 flex flex-wrap items-center gap-1.5">
        {EXAMPLE_TAGS.map((t) => (
          <button key={t}
            className={`chip ${active.has(t) ? "border-accent text-accent" : ""}`}
            onClick={() => toggle(t)}>
            {t}
          </button>
        ))}
        {active.size > 0 && (
          <button className="linkbtn ml-1" onClick={() => setActive(new Set())}>clear</button>
        )}
      </div>

      <ul className="m-0 flex list-none flex-col gap-2 p-0">
        {shown.map((e) => (
          <li key={e.label}>
            <button
              className="w-full cursor-pointer rounded-md border border-border bg-subtle p-2.5 text-left hover:border-accent"
              onClick={() => { onPick(e.bundle); onClose(); }}>
              <div className="font-semibold text-fg">{e.label}</div>
              {e.summary && <div className="mt-0.5 text-muted">{e.summary}</div>}
              {e.tags.length > 0 && (
                <div className="mt-1.5 flex flex-wrap gap-1">
                  {e.tags.map((t) => <span key={t} className="step">{t}</span>)}
                </div>
              )}
            </button>
          </li>
        ))}
        {shown.length === 0 && <li className="text-muted">No examples match those tags.</li>}
      </ul>
    </Modal>
  );
}
