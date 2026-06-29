import { useEffect, useRef, useState } from "react";
import {
  getScaffold, getScaffoldBundle, getScaffoldLog, scaffoldEventsUrl, startScaffold,
} from "../api";
import { Modal } from "./ui/Modal";

// Runs the opencode agent to scaffold a reproducer bundle from a patch series
// (a lore thread or an inline patch), streaming its progress. On success it hands
// the generated bundle to onDone, which opens it in the normal edit/run modal.
const PHASES = ["prepare", "agent", "done"] as const;

export function ScaffoldModal(
  { req, onDone, onClose }:
  { req: { thread?: string; patch?: string; commit?: string };
    onDone: (bundle: string) => void; onClose: () => void },
) {
  const [phase, setPhase] = useState<string>("");
  const [log, setLog] = useState("");
  const [error, setError] = useState("");
  const started = useRef(false); // guard StrictMode double-invoke

  useEffect(() => {
    if (started.current) return;
    started.current = true;
    let live = true;
    let es: EventSource | null = null;
    (async () => {
      let id: number;
      try {
        ({ id } = await startScaffold(req));
      } catch {
        if (live) setError("Couldn't start the scaffolder.");
        return;
      }
      const refreshLog = () => getScaffoldLog(id).then((t) => { if (live) setLog(t); }).catch(() => {});
      es = new EventSource(scaffoldEventsUrl(id));
      es.onmessage = async (e) => {
        try {
          const v = JSON.parse(e.data);
          if (v.kind === "phase" && v.phase) setPhase(v.phase);
          if (v.kind === "phase" || v.kind === "log") refreshLog();
          if (v.kind === "done") {
            es?.close();
            await refreshLog();
            if (!live) return;
            if (v.status === "done") {
              const bundle = await getScaffoldBundle(id);
              if (live && bundle.trim()) onDone(bundle);
              else if (live) setError("The agent finished but produced no bundle.");
            } else {
              const s = await getScaffold(id).catch(() => null);
              setError(s?.error || v.error || "Scaffolding failed.");
            }
          }
        } catch {}
      };
    })();
    return () => { live = false; es?.close(); };
    // Run once: the scaffold is kicked off on mount (started ref guards re-entry).
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, []);

  return (
    <Modal onClose={onClose} label="Scaffold a reproducer">
      <h2>Scaffolding a reproducer ✨</h2>
      <p className="text-muted">
        Asking opencode to read the patch series and write a reproducer. This explores
        the kernel source and can take a few minutes.
      </p>
      <div className="mb-2 flex flex-wrap gap-1.5">
        {PHASES.map((p) => {
          const done = PHASES.indexOf(p) < PHASES.indexOf(phase as typeof PHASES[number]);
          const active = p === phase;
          return (
            <span key={p}
              className={"step " + (active ? "cur" : done ? "done" : "")}>{p}</span>
          );
        })}
      </div>
      {error && <p className="text-[crimson]">{error}</p>}
      <pre className="log max-h-[420px] overflow-auto">{log || "starting…"}</pre>
    </Modal>
  );
}
