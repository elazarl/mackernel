import { useEffect, useState } from "react";
import { getLog } from "../api";
import { SegTabs } from "./ui/Tabs";

export type IssueSection = { file: string; blocks: { head: string[]; trace: string[] }[] };

// One issue report: the description is always shown; the call trace (kernel stack)
// is folded by default and revealed with a button.
function IssueBlock({ head, trace }: { head: string[]; trace: string[] }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="mb-2.5">
      <pre className="log">{head.join("\n")}</pre>
      {trace.length > 0 && (
        <>
          <button className="linkbtn my-1" onClick={() => setOpen((o) => !o)}>
            {open ? "▾ hide call trace" : `▸ show call trace (${trace.length} lines)`}
          </button>
          {open && <pre className="log">{trace.join("\n")}</pre>}
        </>
      )}
    </div>
  );
}

// Issues card: server-side grep of the (variant's) logs, one tab per source, call
// traces folded. Renders nothing when empty for a single job; keeps a labeled slot
// for a compare column so the two columns stay aligned.
export function IssuesCard({ id, variant, label, status }:
  { id: number; variant?: string; label?: string; status?: string }) {
  const [issues, setIssues] = useState<IssueSection[]>([]);
  const [issueTab, setIssueTab] = useState("");
  useEffect(() => {
    getLog(id, "issues", variant)
      .then((t) => { try { setIssues(JSON.parse(t)); } catch { setIssues([]); } })
      .catch(() => setIssues([]));
  }, [id, variant, status]);
  const active = issues.find((s) => s.file === issueTab) ?? issues[0];
  if (!issues.length) {
    return label
      ? <section className="bg-card border border-fail rounded-lg p-3.5 mb-4"><h2 className="text-fail">⚠ Issues · {label}</h2><p className="text-muted">none</p></section>
      : null;
  }
  return (
    <section className="bg-card border border-fail rounded-lg p-3.5 mb-4">
      <h2 className="text-fail">⚠ Issues{label ? ` · ${label}` : ""}</h2>
      <SegTabs
        label="issue sources"
        value={active?.file ?? ""}
        onChange={setIssueTab}
        items={issues.map((s) => ({
          key: s.file,
          label: `${s.file.replace(/\.log$/, "")} (${s.blocks.reduce((n, b) => n + b.head.length, 0)})`,
        }))}
      />
      {active?.blocks.map((b, i) => <IssueBlock key={i} head={b.head} trace={b.trace} />)}
    </section>
  );
}
