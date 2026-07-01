import { useEffect, useState } from "react";
import {
  Candidate, getPeaks, getSummarizer, gib, globalEventsUrl, highlightCss, Job, JobSummary,
  listCandidates, listJobs, LkmlPatch, Peak, refineJob, refineText, runCandidate, startScaffold, submit, SummarizerInfo,
} from "../api";
import { EXAMPLES, upsertMeta } from "../bundle";
import { getTheme, setTheme as persistTheme, Theme } from "../lib/theme";
import { jobFromPath, statusColor, summaryTip } from "../lib/format";
import { ModelSwitcher } from "./ModelSwitcher";
import { BundleModal } from "./BundleModal";
import { SpecModal } from "./SpecModal";
import { LkmlBrowser } from "./LkmlBrowser";
import { OpenAISettings } from "./OpenAISettings";
import { Modal } from "./ui/Modal";
import { hasCreds } from "../lib/creds";
import { startTour, tourSeen, markTourSeen } from "../lib/tour";
import { PeaksChart } from "./charts";
import { JobDetail } from "./JobDetail";

// Only the newest N jobs are listed, so the panel doesn't grow unbounded.
const JOB_LIMIT = 20;

export function Dashboard() {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [peaks, setPeaks] = useState<Peak[]>([]);
  const [candidates, setCandidates] = useState<Candidate[]>([]);
  const [summarizer, setSummarizer] = useState<SummarizerInfo | null>(null);
  const srv = summarizer?.servers ?? [];
  const primaryLabel = srv.find((s) => s.primary)?.label ?? srv[0]?.label ?? "";
  // Derived: shows the primary backend until the user picks another (no effect needed).
  const [viewSel, setView] = useState("");
  const view = viewSel || primaryLabel;
  // Selected job mirrors the URL (/job/ID) so jobs are linkable and back/forward work.
  const [sel, setSel] = useState<number | null>(jobFromPath);
  const selectJob = (id: number | null) => {
    setSel(id);
    const path = id == null ? "/" : `/job/${id}`;
    if (location.pathname !== path) history.pushState({}, "", path);
  };
  const [bundle, setBundle] = useState("");
  const [modalOpen, setModalOpen] = useState(false);
  const [lkmlOpen, setLkmlOpen] = useState(false);
  // Spec mirrors the URL (/spec) so it's linkable and back/forward work, like jobs.
  const [showSpec, setShowSpec] = useState(() => location.pathname === "/spec");
  const openSpec = (open: boolean) => {
    setShowSpec(open);
    const path = open ? "/spec" : (sel == null ? "/" : `/job/${sel}`);
    if (location.pathname !== path) history.pushState({}, "", path);
  };
  useEffect(() => {
    const onPop = () => { setSel(jobFromPath()); setShowSpec(location.pathname === "/spec"); };
    window.addEventListener("popstate", onPop);
    return () => window.removeEventListener("popstate", onPop);
  }, []);
  // Settings opens automatically when a scaffold is attempted without OpenAI creds.
  const [showSettings, setShowSettings] = useState(false);
  // Scaffold prompt modal: holds the lore thread to scaffold + the user's optional prompt.
  const [scaffoldThread, setScaffoldThread] = useState<string | null>(null);
  const [scaffoldNote, setScaffoldNote] = useState("");
  const [theme, setTheme] = useState<Theme>(getTheme());
  const toggleTheme = () => {
    const t = theme === "dark" ? "light" : "dark";
    setTheme(t); persistTheme(t);
  };
  const [hlCss, setHlCss] = useState("");
  useEffect(() => { highlightCss().then(setHlCss).catch(() => {}); }, []);
  // First visit: auto-start the guided tour once (Dashboard only mounts post-unlock).
  useEffect(() => {
    if (tourSeen()) return;
    markTourSeen();
    const t = setTimeout(() => startTour({ selectJob }), 500); // let the first paint settle
    return () => clearTimeout(t);
  }, []);

  // Push, not poll: load once, then refetch only when the server pings that the job
  // list changed. Idle = no requests. `connected` tracks the SSE link for the header.
  const [connected, setConnected] = useState(true);
  useEffect(() => {
    const tick = async () => {
      try {
        const [j, p, c, s] = await Promise.all([listJobs(), getPeaks(), listCandidates(), getSummarizer()]);
        setJobs(j); setPeaks(p); setCandidates(c); setSummarizer(s);
      } catch {}
    };
    tick();
    const es = new EventSource(globalEventsUrl());
    es.onmessage = () => { tick(); };
    es.onopen = () => setConnected(true);
    es.onerror = () => setConnected(false);
    return () => es.close();
  }, []);

  const onRun = async () => {
    if (!bundle.trim()) return;
    const { id } = await submit(bundle);
    setBundle("");
    setModalOpen(false);
    selectJob(id);
  };

  // Refine from the editor: hand the current bundle text (+ optional prompt) to the agent
  // to improve it (no parent job, no logs). Needs OpenAI creds.
  const onRefineText = async (bundleText: string, note: string) => {
    if (!hasCreds()) { setShowSettings(true); return; }
    const { id } = await refineText(bundleText, note);
    setModalOpen(false);
    selectJob(id);
  };

  // Run a detected LKML cover letter: the server creates a job from the stored bundle
  // (thread series applied at build time) and we jump to it.
  const onRunCandidate = async (c: Candidate) => {
    const { id } = await runCandidate(c.msgid);
    selectJob(id);
  };

  // Scaffold a lore thread: open the prompt modal (where the user can add optional context),
  // then create the background job. Needs OpenAI creds — bounce to settings if missing.
  const openScaffold = (thread: string) => {
    if (!hasCreds()) { setShowSettings(true); return; }
    setScaffoldNote("");
    setScaffoldThread(thread);
  };
  const doScaffold = async () => {
    if (!scaffoldThread) return;
    const { id } = await startScaffold({ thread: scaffoldThread, note: scaffoldNote });
    setScaffoldThread(null);
    selectJob(id);
  };
  const onScaffoldCandidate = (c: Candidate) => openScaffold(c.source_url);

  // Pick a patch in the LKML browser: open its cover letter as a reproducer. Inject a
  // `thread:` key pointing at the lore thread (upsertMeta keeps any existing
  // frontmatter), then open the edit/preview/run modal.
  const onPickPatch = (p: LkmlPatch) => {
    setBundle(upsertMeta(p.body, "thread", p.url));
    setLkmlOpen(false);
    setModalOpen(true);
  };
  // Scaffold from a browsed patch: close the browser, then open the scaffold prompt modal.
  const onScaffoldPatch = (p: LkmlPatch) => {
    setLkmlOpen(false);
    openScaffold(p.url);
  };

  // Refine a job: hand its reproducer + logs (plus optional user context) back to the
  // agent to fix, open the child job.
  const onRefine = async (id: number, note?: string) => {
    if (!hasCreds()) { setShowSettings(true); return; }
    const { id: child } = await refineJob(id, note);
    selectJob(child);
  };

  return (
    <div className="mx-auto max-w-[1200px] px-6 py-4">
      {hlCss && <style>{hlCss}</style>}
      <div className="flex items-center gap-3">
        <h1 className="cursor-pointer hover:text-accent" title="Home" onClick={() => selectJob(null)}>Kernel Reproducer Runner</h1>
        <button className="linkbtn" data-tour="spec" onClick={() => openSpec(true)}>Spec</button>
        <button className="linkbtn" onClick={toggleTheme}>{theme === "dark" ? "☀ Light" : "🌙 Dark"}</button>
        <button className="linkbtn" title="Scaffold model settings (OpenAI endpoint + key)" onClick={() => setShowSettings(true)}>⚙ Settings</button>
        <button className="linkbtn" title="How to use this site" onClick={() => startTour({ selectJob })}>❓ Tour</button>
        {!connected && <span className="text-fail text-[.85em]" title="lost the live stream — reconnecting…">⚠ offline</span>}
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
      {showSpec && <SpecModal onClose={() => openSpec(false)} />}
      {showSettings && <OpenAISettings onClose={() => setShowSettings(false)} />}
      {scaffoldThread && (
        <Modal onClose={() => setScaffoldThread(null)} label="Scaffold a reproducer">
          <h2>Scaffold a reproducer ✨</h2>
          <p className="text-muted mb-2">
            The agent reads the patch series and the kernel source, writes a reproducer
            bundle, then runs it. Add any context to guide it (optional) — e.g. which bug to
            target, a subsystem to focus on, or how to trigger it.
          </p>
          <textarea
            className="mb-3 w-full box-border rounded-md border border-border bg-bg p-[9px] font-mono text-fg outline-none focus:border-accent"
            rows={5} autoFocus placeholder="optional context for the agent…"
            value={scaffoldNote} onChange={(e) => setScaffoldNote(e.target.value)} />
          <div className="flex justify-end">
            <button className="btn" onClick={doScaffold}>Scaffold</button>
          </div>
        </Modal>
      )}
      {modalOpen && (
        <BundleModal bundle={bundle} theme={theme} onChange={setBundle}
          onRun={onRun} onRefine={onRefineText} onClose={() => setModalOpen(false)} />
      )}
      {lkmlOpen && <LkmlBrowser onPick={onPickPatch} onScaffold={onScaffoldPatch} onClose={() => setLkmlOpen(false)} />}
      <div className="grid grid-cols-[380px_1fr] gap-4 items-start">
        <div>
          <section className="card" data-tour="submit">
            <div className="flex items-center justify-between">
              <h2>Submit a bundle</h2>
              {/* Browse lore.kernel.org on demand: pick a list, pick a patch, open its
                  cover letter as a reproducer (or Scaffold it) — no polling. */}
              <button className="chip" data-tour="lkml" onClick={() => setLkmlOpen(true)}>Browse LKML</button>
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
            <div className="mb-2.5 flex flex-wrap items-center gap-1.5" data-tour="examples">
              <span className="text-xs text-muted">Examples:</span>
              {EXAMPLES.map((ex) => (
                <button key={ex.label} className="chip" title={ex.blurb}
                  onClick={() => { setBundle(ex.bundle); setModalOpen(true); }}>
                  {ex.label}
                </button>
              ))}
            </div>
          </section>
          {candidates.length > 0 && (
            <section className="card">
              <h2>From LKML <span className="text-muted">· {candidates.length} candidate{candidates.length === 1 ? "" : "s"}</span></h2>
              <ul className="m-0 max-h-70 list-none overflow-auto p-0">
                {candidates.map((c) => (
                  <li key={c.msgid} className="flex flex-col gap-0.5 rounded-md px-2 py-1.5">
                    <div className="flex flex-wrap items-center gap-1.5">
                      <a className="text-accent" href={c.source_url} target="_blank" rel="noreferrer"
                        onClick={(e) => e.stopPropagation()}>{c.title || c.msgid}</a>
                      {c.list && <span className="text-muted"> · {c.list}</span>}
                    </div>
                    <div className="pl-3.5 flex flex-wrap gap-1.5">
                      {c.job_id != null
                        ? <button className="chip mt-1" onClick={() => selectJob(c.job_id!)}>view job #{c.job_id}</button>
                        : <button className="chip mt-1" onClick={() => onRunCandidate(c)}>Run</button>}
                      {/* Scaffold: let opencode write a reproducer from this series. */}
                      <button className="chip mt-1" title="Let opencode write a reproducer for this series"
                        onClick={() => onScaffoldCandidate(c)}>Scaffold ✨</button>
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          )}
          <section className="card" data-tour="jobs">
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
        <div data-tour="detail">
          {sel == null ? <p className="text-muted">Select a job to see live progress, metrics, and logs.</p>
            : <JobDetail id={sel} summarizerReady={summarizer?.loaded ?? false}
                servers={srv} view={view} onRefine={onRefine}
                onEdit={(text) => { setBundle(text); setModalOpen(true); }} />}
        </div>
      </div>
    </div>
  );
}
