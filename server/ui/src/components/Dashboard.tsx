import { useEffect, useState } from "react";
import {
  getPeaks, getSummarizer, gib, globalEventsUrl, highlightCss, Job, JobSummary,
  listJobs, LkmlPatch, Peak, submit, SummarizerInfo,
} from "../api";
import { EXAMPLES, upsertMeta } from "../bundle";
import { getTheme, setTheme as persistTheme, Theme } from "../lib/theme";
import { jobFromPath, statusColor, summaryTip } from "../lib/format";
import { ModelSwitcher } from "./ModelSwitcher";
import { BundleModal } from "./BundleModal";
import { SpecModal } from "./SpecModal";
import { LkmlBrowser } from "./LkmlBrowser";
import { PeaksChart } from "./charts";
import { JobDetail } from "./JobDetail";

// Only the newest N jobs are listed, so the panel doesn't grow unbounded.
const JOB_LIMIT = 20;

export function Dashboard() {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [peaks, setPeaks] = useState<Peak[]>([]);
  const [summarizer, setSummarizer] = useState<SummarizerInfo | null>(null);
  const srv = summarizer?.servers ?? [];
  const primaryLabel = srv.find((s) => s.primary)?.label ?? srv[0]?.label ?? "";
  const [view, setView] = useState("");
  useEffect(() => { if (!view && primaryLabel) setView(primaryLabel); }, [primaryLabel]);
  // Selected job mirrors the URL (/job/ID) so jobs are linkable and back/forward work.
  const [sel, setSel] = useState<number | null>(jobFromPath);
  const selectJob = (id: number | null) => {
    setSel(id);
    const path = id == null ? "/" : `/job/${id}`;
    if (location.pathname !== path) history.pushState({}, "", path);
  };
  useEffect(() => {
    const onPop = () => setSel(jobFromPath());
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  const [bundle, setBundle] = useState("");
  const [modalOpen, setModalOpen] = useState(false);
  const [lkmlOpen, setLkmlOpen] = useState(false);
  const [showSpec, setShowSpec] = useState(false);
  const [theme, setTheme] = useState<Theme>(getTheme());
  const toggleTheme = () => {
    const t = theme === "dark" ? "light" : "dark";
    setTheme(t); persistTheme(t);
  };
  const [hlCss, setHlCss] = useState("");
  useEffect(() => { highlightCss().then(setHlCss).catch(() => {}); }, []);

  // Push, not poll: load once, then refetch only when the server pings that the job
  // list changed. Idle = no requests.
  useEffect(() => {
    const tick = async () => {
      try {
        setJobs(await listJobs());
        setPeaks(await getPeaks());
        setSummarizer(await getSummarizer());
      } catch {}
    };
    tick();
    const es = new EventSource(globalEventsUrl());
    es.onmessage = () => { tick(); };
    return () => es.close();
  }, []);

  const onRun = async () => {
    if (!bundle.trim()) return;
    const { id } = await submit(bundle);
    setBundle("");
    setModalOpen(false);
    selectJob(id);
  };

  // Pick a patch in the LKML browser: open its cover letter as a reproducer. Inject a
  // `thread:` key pointing at the lore thread (upsertMeta keeps any existing frontmatter
  // and just sets thread:, or prepends a new block when the cover letter has none), then
  // open the edit/preview/run modal.
  const onPickPatch = (p: LkmlPatch) => {
    setBundle(upsertMeta(p.body, "thread", p.url));
    setLkmlOpen(false);
    setModalOpen(true);
  };

  return (
    <div className="mx-auto max-w-[1200px] px-6 py-4">
      {hlCss && <style>{hlCss}</style>}
      <div className="flex items-center gap-3">
        <h1 className="cursor-pointer hover:text-accent" title="Home" onClick={() => selectJob(null)}>Kernel Reproducer Runner</h1>
        <button className="linkbtn" onClick={() => setShowSpec(true)}>Spec</button>
        <button className="linkbtn" onClick={toggleTheme}>{theme === "dark" ? "☀ Light" : "🌙 Dark"}</button>
        {summarizer && (
          <span className="ml-auto flex items-center gap-1.5 text-[.85em] text-muted">
            🧠
            {!summarizer.loaded ? "warming up…"
              : srv.length > 1 ? (
                <>
                  <ModelSwitcher servers={srv} view={view} onChange={setView} />
                  {`· ${srv.length} models · ${gib(summarizer.mem_bytes)} GB`}
                </>
              ) : `${summarizer.label} · ${gib(summarizer.mem_bytes)} GB`}
          </span>
        )}
      </div>
      {showSpec && <SpecModal onClose={() => setShowSpec(false)} />}
      {modalOpen && (
        <BundleModal bundle={bundle} theme={theme} onChange={setBundle}
          onRun={onRun} onClose={() => setModalOpen(false)} />
      )}
      {lkmlOpen && <LkmlBrowser onPick={onPickPatch} onClose={() => setLkmlOpen(false)} />}
      <div className="grid grid-cols-[380px_1fr] gap-4 items-start">
        <div>
          <section className="card">
            <div className="flex items-center justify-between">
              <h2>Submit a bundle</h2>
              {/* Browse lore.kernel.org on demand: pick a list, pick a patch, open its
                  cover letter as a reproducer (no polling). */}
              <button className="chip" onClick={() => setLkmlOpen(true)}>Browse LKML</button>
            </div>
            {/* Pasting a bundle opens the modal — the one place you edit / preview / run. */}
            <input
              className="mb-2.5 w-full box-border rounded-md border border-border bg-bg p-[9px] font-mono text-fg outline-none focus:border-accent"
              placeholder="paste here"
              onPaste={(e) => {
                const text = e.clipboardData.getData("text");
                if (text.trim()) { e.preventDefault(); setBundle(text); setModalOpen(true); }
              }}
              onChange={() => { /* controlled-but-ephemeral: real text lives in the modal */ }}
              value="" />
            <div className="mb-2.5 flex flex-wrap items-center gap-1.5">
              <span className="text-xs text-muted">Examples:</span>
              {EXAMPLES.map((ex) => (
                <button key={ex.label} className="chip" title={ex.blurb}
                  onClick={() => { setBundle(ex.bundle); setModalOpen(true); }}>
                  {ex.label}
                </button>
              ))}
            </div>
          </section>
          <section className="card">
            <h2>Jobs{jobs.length > JOB_LIMIT && <span className="text-muted"> · newest {JOB_LIMIT} of {jobs.length}</span>}</h2>
            <ul className="m-0 max-h-70 list-none overflow-auto p-0">
              {[...jobs].sort((a, b) => b.id - a.id).slice(0, JOB_LIMIT).map((j) => (
                <li key={j.id} className={"flex cursor-pointer flex-col gap-0.5 rounded-md px-2 py-1.5 " + (sel === j.id ? "bg-subtle" : "")} onClick={() => selectJob(j.id)}>
                  <div className="flex flex-wrap items-center gap-1.5">
                    <span className="inline-block h-2.5 w-2.5 rounded-full" style={{ background: statusColor(j.status) }} />
                    #{j.id}{j.short_title && <span className="font-semibold text-accent break-words" title={summaryTip(j, "title")}> {j.short_title}</span>} <em className="not-italic text-muted">{j.status}</em>
                    {j.phase && j.status === "running" && <span className="text-muted"> · {j.phase}</span>}
                    {j.exit_code != null && <span className="text-muted"> · exit {j.exit_code}</span>}
                    {j.reaped_ms != null && <span className="text-muted"> · logs expired</span>}
                    {j.source_url && (
                      <a className="ml-auto text-[.82em] text-accent no-underline" href={j.source_url} target="_blank" rel="noreferrer"
                        onClick={(e) => e.stopPropagation()}>lore ↗</a>
                    )}
                  </div>
                  {j.title && <div className="pl-3.5 text-[.82em] leading-snug text-muted" title={j.title}>{j.title}</div>}
                  {j.repro_summary && <div className="pl-3.5 text-[.82em] leading-snug text-muted" title={summaryTip(j, "repro") ?? j.repro_summary}>{j.repro_summary}</div>}
                </li>
              ))}
            </ul>
          </section>
          <section className="card">
            <h2>Peak resource usage (per job)</h2>
            <PeaksChart peaks={peaks} />
          </section>
        </div>
        <div>
          {sel == null ? <p className="text-muted">Select a job to see live progress, metrics, and logs.</p>
            : <JobDetail id={sel} summarizerReady={summarizer?.loaded ?? false}
                servers={srv} view={view}
                onEdit={(text) => { setBundle(text); setModalOpen(true); }} />}
        </div>
      </div>
    </div>
  );
}
