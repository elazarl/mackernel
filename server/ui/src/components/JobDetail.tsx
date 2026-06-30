import { useEffect, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import {
  eventsUrl, getJob, getJobSummaries, getLog, getMetrics, getPhases, gib,
  Job, JobSummary, Sample, SummarizerInfo,
} from "../api";
import { compareMode, parseBundle } from "../bundle";
import { jobSummaryTip, phaseList, statusColor, stepClass, summaryTip, toSample } from "../lib/format";
import { ResourceChart } from "./charts";
import { IssuesCard } from "./IssuesCard";
import { LogPane } from "./LogPane";
import { SummaryLine } from "./SummaryLine";
import { BundlePreview } from "./BundlePreview";
import { Modal } from "./ui/Modal";
import { SegTabs } from "./ui/Tabs";

// `run` is the orchestrator log (carries the failure reason even for early crashes);
// `dmesg` the guest ring buffer; `console` the raw QEMU serial. (`issues` is surfaced
// as a card, not a tab.)
const LOG_KINDS = ["fetch", "compile", "console", "dmesg", "exec", "run"] as const;
type LogKind = (typeof LOG_KINDS)[number];

export function JobDetail({ id, summarizerReady, servers, view, onEdit, onRefine }:
  { id: number; summarizerReady: boolean; servers: SummarizerInfo["servers"]; view: string; onEdit: (text: string) => void; onRefine: (id: number) => void }) {
  const [job, setJob] = useState<Job | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [logKind, setLogKind] = useState<LogKind>("exec");
  const [bundleText, setBundleText] = useState("");
  const [maxRepro, setMaxRepro] = useState(false);
  // All backends' summaries; the model switcher picks which one the view shows. The
  // primary keeps the live-streamed columns from `job`.
  const [summaries, setSummaries] = useState<JobSummary[]>([]);
  const srv = servers ?? [];
  const primaryLabel = srv.find((s) => s.primary)?.label ?? srv[0]?.label ?? "";
  const isPrimary = !view || view === primaryLabel;
  const selModel = srv.find((s) => s.label === view)?.model ?? view;
  // React Compiler memoizes this; it rebuilds only when `summaries` changes.
  const byServer = (() => {
    const m = new Map<string, Map<string, JobSummary>>();
    for (const s of summaries) {
      if (!m.has(s.server)) m.set(s.server, new Map());
      m.get(s.server)!.set(s.field, s);
    }
    return m;
  })();
  const cell = (field: string) => byServer.get(view)?.get(field);
  // One-line summary for a non-primary backend (tooltip carries model · time · tokens).
  const serverLine = (field: string, icon: string) => {
    const r = cell(field);
    return r
      ? <p className="summary" title={jobSummaryTip(r)}>{icon} {r.text}</p>
      : <p className="summary"><span className="text-muted">{icon} no summary from {selModel} yet</span></p>;
  };
  const [phaseTs, setPhaseTs] = useState<Record<string, number>>({});
  const [progress, setProgress] = useState<Record<string, number>>({});
  const bundle = parseBundle(bundleText);
  const cmp = compareMode(bundle);
  const t0 = useRef<number>(0);
  const userPicked = useRef(false);

  useEffect(() => {
    setSamples([]); setJob(null); setPhaseTs({}); setProgress({}); setSummaries([]);
    userPicked.current = false;
    let live = true;
    let es: EventSource | null = null;
    const refreshSummaries = () => getJobSummaries(id).then((s) => { if (live) setSummaries(s); }).catch(() => {});
    (async () => {
      const j = await getJob(id); if (!live) return; setJob(j);
      const m = await getMetrics(id); if (!live) return;
      t0.current = m[0]?.ts_ms ?? Date.now();
      setSamples(m);
      refreshSummaries();
      getPhases(id).then((evs) => { if (live) setPhaseTs(Object.fromEntries(evs.map((e) => [e.phase, e.ts_ms]))); }).catch(() => {});
      const terminal = j.status === "done" || j.status === "failed";
      const summariesComplete =
        j.short_title != null && j.repro_summary != null &&
        j.result_summary != null && j.detail != null;
      if (j.reaped_ms != null) return;
      if (terminal && summariesComplete) return;
      es = new EventSource(eventsUrl(id));
      es.onmessage = (e) => {
        try {
          const v = JSON.parse(e.data);
          if (v.kind === "metric") setSamples((s) => [...s, toSample(v)]);
          if (v.kind === "phase" && v.phase && v.ts_ms)
            setPhaseTs((p) => (p[v.phase] ? p : { ...p, [v.phase]: v.ts_ms }));
          if (v.kind === "summary_progress" && v.field)
            setProgress((p) => ({ ...p, [v.field]: v.tokens ?? 0 }));
          if (v.kind === "summary" && v.field)
            setProgress((p) => { const n = { ...p }; delete n[v.field]; return n; });
          if (v.kind === "phase" || v.kind === "done" || v.kind === "summary") getJob(id).then(setJob);
          if (v.kind === "summary" || v.kind === "done" || v.kind === "summaries_done") refreshSummaries();
          if (v.kind === "summaries_done") es?.close();
        } catch {}
      };
    })();
    getLog(id, "bundle").then(setBundleText).catch(() => setBundleText(""));
    return () => { live = false; es?.close(); };
  }, [id]);

  // On failure, jump to the orchestrator log unless the user already picked a tab.
  useEffect(() => {
    if (!userPicked.current && job?.status === "failed") setLogKind("run");
  }, [job?.status]);

  const reproView = bundle.files.length || bundle.meta.length
    ? <BundlePreview parsed={bundle} />
    : <pre className="log">{bundleText}</pre>;

  return (
    <div>
      {cmp ? (
        <div className="grid grid-cols-2 gap-3 items-start">
          <IssuesCard id={id} variant="baseline" label="baseline" status={job?.status} />
          <IssuesCard id={id} variant="patched" label="patched" status={job?.status} />
        </div>
      ) : (
        <IssuesCard id={id} status={job?.status} />
      )}
      <section className="card">
        <h2>Job #{id} {job && <span style={{ color: statusColor(job.status) }}>· {job.status}</span>}
          {cmp && <span className="text-muted"> · {cmp === "thread" ? "thread-compare" : "patch-compare"} (baseline vs patched)</span>}</h2>
        <div className="mb-2 flex flex-wrap gap-1.5">
          {phaseList(job).map((p) => (
            <span key={p} className={"step " + stepClass(job, p)}>{p}</span>
          ))}
        </div>
        {isPrimary
          ? <SummaryLine job={job} field="repro" icon="📝" text={job?.repro_summary ?? null}
              progress={progress} due={job != null && job.status !== "queued"} summarizerReady={summarizerReady} />
          : serverLine("repro", "📝")}
        {isPrimary
          ? <SummaryLine job={job} field="result" icon="✅" text={job?.result_summary ?? null}
              progress={progress} due={job?.status === "done" || job?.status === "failed"} summarizerReady={summarizerReady} />
          : serverLine("result", "✅")}
        {job && (
          <p className="text-muted">
            exit {job.exit_code ?? "—"} · peak RAM {gib(job.ram_peak)} GB · peak disk {gib(job.disk_peak)} GB
          </p>
        )}
      </section>
      {(() => {
        // Detail markdown + tooltip follow the selected model: live column for the
        // primary, the per-server row otherwise.
        const detailText = isPrimary ? job?.detail ?? null : cell("detail")?.text ?? null;
        const detailTip = isPrimary ? summaryTip(job, "detail")
          : (cell("detail") ? jobSummaryTip(cell("detail")!) : undefined);
        const show = detailText != null ||
          (isPrimary && (progress["detail"] !== undefined || job?.status === "done" || job?.status === "failed"));
        if (!show) return null;
        return (
          <section className="card">
            <h2>Job summary{detailTip &&
              <span className="text-muted font-normal text-[0.7em]" title={detailTip}> ⓘ</span>}</h2>
            {detailText != null
              ? <div className="md" title={detailTip}>
                  <ReactMarkdown remarkPlugins={[remarkGfm]}>{detailText}</ReactMarkdown>
                </div>
              : isPrimary
                ? <p className="text-muted">{progress["detail"] !== undefined
                    ? `generating… ${progress["detail"]} tok`
                    : (summarizerReady ? "⏳ pending" : "⏳ summarizer warming up…")}</p>
                : <p className="text-muted">no summary from {selModel} yet</p>}
          </section>
        );
      })()}
      {bundleText.trim() && (
        <section className="card">
          <div className="flex items-center justify-between">
            <h2>Reproducer</h2>
            <span>
              <button className="linkbtn" onClick={() => setMaxRepro(true)}>Maximize</button>
              {" · "}
              <button className="linkbtn" onClick={() => onEdit(bundleText)}>Edit reproducer</button>
              {(job?.status === "done" || job?.status === "failed") && <>
                {" · "}
                <button className="linkbtn" title="Hand this reproducer + its run logs back to the agent to fix"
                  onClick={() => onRefine(id)}>Refine ✨</button>
              </>}
            </span>
          </div>
          {reproView}
        </section>
      )}
      {maxRepro && (
        <Modal onClose={() => setMaxRepro(false)} label="Reproducer">
          <h2>Reproducer</h2>
          {reproView}
        </Modal>
      )}
      <section className="card">
        <h2>Resource usage</h2>
        <ResourceChart samples={samples} t0={t0.current} phaseTs={phaseTs} />
      </section>
      <section className="card">
        <SegTabs
          label="logs"
          value={logKind}
          onChange={(k) => { userPicked.current = true; setLogKind(k as LogKind); }}
          items={LOG_KINDS.map((k) => ({ key: k, label: k }))}
        />
        {job?.reaped_ms != null && <p className="text-muted">logs expired (job dir reclaimed after retention)</p>}
        {cmp ? (
          <div className="grid grid-cols-2 gap-3 items-start">
            <div><div className="my-1 font-mono text-xs text-muted">baseline</div><LogPane id={id} kind={logKind} variant="baseline" status={job?.status} /></div>
            <div><div className="my-1 font-mono text-xs text-muted">patched</div><LogPane id={id} kind={logKind} variant="patched" status={job?.status} /></div>
          </div>
        ) : (
          <LogPane id={id} kind={logKind} status={job?.status} />
        )}
      </section>
    </div>
  );
}
