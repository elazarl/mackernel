import { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import MDEditor from "@uiw/react-md-editor";
import {
  Bar, BarChart, CartesianGrid, Legend, Line, LineChart, ReferenceLine,
  ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";
import {
  Candidate, eventsUrl, getJob, getLog, getMetrics, getPeaks, getPhases, getSummarizer, gib, globalEventsUrl, hasToken,
  highlight, highlightCss, Job, listCandidates, listJobs, mib, Peak, runCandidate, Sample,
  setToken, submit, SummarizerInfo,
} from "./api";
import {
  compareMode, EXAMPLES, githubTree, KERNEL_URL, parseBundle, ParsedBundle, rolesOf, upsertMeta,
} from "./bundle";
import specMd from "../../../docs/reproducer-spec.md?raw";

const PHASES = ["fetch", "configure", "build", "boot", "insmod", "run", "done"];

// Per-summary generation metadata (from job.summary_meta JSON) → hover tooltip text.
// `took` is a human-readable duration ("17s", "2m 3s") formatted server-side; `ms` is
// kept for older rows that predate it.
type SummaryMeta = { ms: number; took?: string; tokens: number; model: string };
function summaryMeta(job: Job | null, field: string): SummaryMeta | null {
  if (!job?.summary_meta) return null;
  try { return (JSON.parse(job.summary_meta) as Record<string, SummaryMeta>)[field] ?? null; }
  catch { return null; }
}
function summaryTip(job: Job | null, field: string): string | undefined {
  const m = summaryMeta(job, field);
  if (!m) return undefined;
  return `generated in ${m.took ?? `${m.ms} ms`} · ${m.model} · ${m.tokens} tokens`;
}

// One summary line: the text + hover tooltip once it's generated, a live token count
// while it streams, or a "pending" placeholder when it's due but not yet started.
function SummaryLine(
  { job, field, icon, text, progress, due, summarizerReady }:
  { job: Job | null; field: string; icon: string; text: string | null; progress: Record<string, number>; due: boolean; summarizerReady: boolean },
) {
  if (text) return <p className="summary" title={summaryTip(job, field)}>{icon} {text}</p>;
  const tok = progress[field];
  if (tok !== undefined) return <p className="summary"><span className="muted">{icon} generating… {tok} tok</span></p>;
  if (due) return <p className="summary"><span className="muted">{icon} {summarizerReady ? "⏳ pending" : "⏳ summarizer warming up…"}</span></p>;
  return null;
}
// `run` is the run-kernel.py orchestrator log: it always carries the failure reason
// (a die() message or an uncaught traceback), even for early crashes that never reach
// the phase-specific logs — so it's the reliable place to look when a job fails.
// `dmesg` is the guest kernel ring buffer; `console` is the raw QEMU serial capture.
// (`issues` — a server-side grep of all logs for error/panic/sanitizer markers — is
// surfaced separately as a card at the top, not as a log tab.)
const LOG_KINDS = ["fetch", "compile", "console", "dmesg", "exec", "run"] as const;
// Only the newest N jobs are listed, so the panel doesn't grow unbounded.
const JOB_LIMIT = 20;
type LogKind = (typeof LOG_KINDS)[number];

const statusColor = (s: string) =>
  s === "done" ? "#3fb950" : s === "failed" ? "#f85149"
    : s === "running" ? "#d29922" : "#8b949e";

// Theme lives in localStorage and is reflected as data-theme on <html>; the CSS
// palette is driven by variables that a :root[data-theme="light"] block overrides.
type Theme = "dark" | "light";
const THEME_KEY = "mk-theme";
const getTheme = (): Theme => (localStorage.getItem(THEME_KEY) === "light" ? "light" : "dark");
const applyTheme = (t: Theme) => { document.documentElement.dataset.theme = t; };
applyTheme(getTheme()); // run at module load so the Unlock gate is themed too

export function App() {
  const [authed, setAuthed] = useState(hasToken());

  // A rejected token (401) anywhere clears it and bounces back to the unlock gate.
  useEffect(() => {
    const onUnauthorized = () => setAuthed(false);
    window.addEventListener("mk-unauthorized", onUnauthorized);
    return () => window.removeEventListener("mk-unauthorized", onUnauthorized);
  }, []);

  if (!authed) return <Unlock onUnlock={() => setAuthed(true)} />;
  return <Dashboard />;
}

// First-visit gate: ask for the commit hash of the v7.1 tag and use it as the
// bearer token for /api/*. Remounting Dashboard on unlock kicks off a fresh poll.
function Unlock({ onUnlock }: { onUnlock: () => void }) {
  const [value, setValue] = useState("");
  const submitToken = () => {
    if (!value.trim()) return;
    setToken(value);
    onUnlock();
  };
  return (
    <div className="wrap">
      <style>{CSS}</style>
      <h1>mackernel — reproducer runner</h1>
      <section className="card unlock">
        <h2>Unlock</h2>
        <p className="muted">Enter the commit hash of the <code>v7.1</code> tag to continue.</p>
        <input
          type="password"
          value={value}
          autoFocus
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => { if (e.key === "Enter") submitToken(); }}
          placeholder="v7.1 commit hash"
        />
        <button onClick={submitToken}>Unlock</button>
      </section>
    </div>
  );
}

function Dashboard() {
  const [jobs, setJobs] = useState<Job[]>([]);
  const [peaks, setPeaks] = useState<Peak[]>([]);
  const [candidates, setCandidates] = useState<Candidate[]>([]);
  const [summarizer, setSummarizer] = useState<SummarizerInfo | null>(null);
  const [sel, setSel] = useState<number | null>(null);
  const [bundle, setBundle] = useState("");
  const [modalOpen, setModalOpen] = useState(false);
  const [showSpec, setShowSpec] = useState(false);
  const [theme, setTheme] = useState<Theme>(getTheme());
  const toggleTheme = () => {
    const t = theme === "dark" ? "light" : "dark";
    setTheme(t); applyTheme(t); localStorage.setItem(THEME_KEY, t);
  };
  const [hlCss, setHlCss] = useState("");
  useEffect(() => { highlightCss().then(setHlCss).catch(() => {}); }, []);

  // Push, not poll: load once, then refetch only when the server pings that the
  // job list changed. Idle = no requests.
  useEffect(() => {
    const tick = async () => {
      try {
        setJobs(await listJobs());
        setPeaks(await getPeaks());
        setCandidates(await listCandidates());
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
    setSel(id);
  };

  // Run a detected LKML cover letter: the server creates a job from the stored bundle
  // (thread series applied at build time) and we jump to it.
  const onRunCandidate = async (c: Candidate) => {
    const { id } = await runCandidate(c.msgid);
    setSel(id);
  };

  return (
    <div className="wrap">
      <style>{CSS}</style>
      {hlCss && <style>{hlCss}</style>}
      <div className="topbar">
        <h1>mackernel — reproducer runner</h1>
        <button className="linkbtn" onClick={() => setShowSpec(true)}>Spec</button>
        <button className="linkbtn" onClick={toggleTheme}>{theme === "dark" ? "☀ Light" : "🌙 Dark"}</button>
        {summarizer && (
          <span className="muted summarizer" title={`${summarizer.label} summarizer (llama-server), resident size incl. KV cache`}>
            🧠 {summarizer.loaded ? `${summarizer.label} · ${gib(summarizer.mem_bytes)} GB` : "warming up…"}
          </span>
        )}
      </div>
      {showSpec && <SpecModal onClose={() => setShowSpec(false)} />}
      {modalOpen && (
        <BundleModal bundle={bundle} theme={theme} onChange={setBundle}
          onRun={onRun} onClose={() => setModalOpen(false)} />
      )}
      <div className="cols">
        <div className="left">
          <section className="card">
            <h2>Submit a bundle</h2>
            {/* Pasting a bundle opens the modal — the one place you edit / preview / run. */}
            <input className="paste" placeholder="paste here"
              onPaste={(e) => {
                const text = e.clipboardData.getData("text");
                if (text.trim()) { e.preventDefault(); setBundle(text); setModalOpen(true); }
              }}
              onChange={() => { /* controlled-but-ephemeral: real text lives in the modal */ }}
              value="" />
            <div className="examples">
              <span className="exlabel">Examples:</span>
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
              <h2>From LKML <span className="muted">· {candidates.length} candidate{candidates.length === 1 ? "" : "s"}</span></h2>
              <ul className="jobs">
                {candidates.map((c) => (
                  <li key={c.msgid}>
                    <div className="jobrow">
                      <a href={c.source_url} target="_blank" rel="noreferrer"
                        onClick={(e) => e.stopPropagation()}>{c.title || c.msgid}</a>
                      {c.list && <span className="ph"> · {c.list}</span>}
                    </div>
                    <div className="candactions">
                      {c.job_id != null
                        ? <button className="chip" onClick={() => setSel(c.job_id!)}>view job #{c.job_id}</button>
                        : <button className="chip" onClick={() => onRunCandidate(c)}>Run</button>}
                    </div>
                  </li>
                ))}
              </ul>
            </section>
          )}
          <section className="card">
            <h2>Jobs{jobs.length > JOB_LIMIT && <span className="muted"> · newest {JOB_LIMIT} of {jobs.length}</span>}</h2>
            <ul className="jobs">
              {[...jobs].sort((a, b) => b.id - a.id).slice(0, JOB_LIMIT).map((j) => (
                <li key={j.id} className={sel === j.id ? "active" : ""} onClick={() => setSel(j.id)}>
                  <div className="jobrow">
                    <span className="dot" style={{ background: statusColor(j.status) }} />
                    #{j.id}{j.short_title && <span className="shorttitle" title={summaryTip(j, "title")}> {j.short_title}</span>} <em>{j.status}</em>
                    {j.phase && j.status === "running" && <span className="ph"> · {j.phase}</span>}
                    {j.exit_code != null && <span className="ph"> · exit {j.exit_code}</span>}
                    {j.reaped_ms != null && <span className="ph"> · logs expired</span>}
                    {j.source_url && (
                      <a className="srclink" href={j.source_url} target="_blank" rel="noreferrer"
                        onClick={(e) => e.stopPropagation()}>lore ↗</a>
                    )}
                  </div>
                  {j.title && <div className="jobsum" title={j.title}>{j.title}</div>}
                  {j.repro_summary && <div className="jobsum" title={summaryTip(j, "repro") ?? j.repro_summary}>{j.repro_summary}</div>}
                </li>
              ))}
            </ul>
          </section>
          <section className="card">
            <h2>Peak resource usage (per job)</h2>
            <ResponsiveContainer width="100%" height={180}>
              <BarChart data={peaks.map((p) => ({ id: `#${p.id}`, RAM: +gib(p.ram_peak), Disk: +gib(p.disk_peak) }))}>
                <CartesianGrid strokeDasharray="3 3" stroke="#8b949e" strokeOpacity={0.3} />
                <XAxis dataKey="id" stroke="#8b949e" /><YAxis stroke="#8b949e" unit="G" />
                <Tooltip contentStyle={{ background: "var(--card)", border: "1px solid var(--border)" }} />
                <Legend /><Bar dataKey="RAM" fill="#58a6ff" /><Bar dataKey="Disk" fill="#bc8cff" />
              </BarChart>
            </ResponsiveContainer>
          </section>
        </div>
        <div className="right">
          {sel == null ? <p className="muted">Select a job to see live progress, metrics, and logs.</p>
            : <JobDetail id={sel} summarizerReady={summarizer?.loaded ?? false}
                onEdit={(text) => { setBundle(text); setModalOpen(true); }} />}
        </div>
      </div>
    </div>
  );
}

type IssueSection = { file: string; blocks: { head: string[]; trace: string[] }[] };

function JobDetail({ id, summarizerReady, onEdit }: { id: number; summarizerReady: boolean; onEdit: (text: string) => void }) {
  const [job, setJob] = useState<Job | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [logKind, setLogKind] = useState<LogKind>("exec");
  const [bundleText, setBundleText] = useState("");
  const [maxRepro, setMaxRepro] = useState(false);
  // Phase start times (ms) keyed by phase name — used to mark the timeline.
  const [phaseTs, setPhaseTs] = useState<Record<string, number>>({});
  // Live token count per summary field while it streams (cleared when the summary lands).
  const [progress, setProgress] = useState<Record<string, number>>({});
  const bundle = useMemo(() => parseBundle(bundleText), [bundleText]);
  // A patch-compare / thread-compare job ran baseline + patched; show them side by side.
  const cmp = useMemo(() => compareMode(bundle), [bundle]);
  const t0 = useRef<number>(0);
  const userPicked = useRef(false);

  useEffect(() => {
    setSamples([]); setJob(null); setPhaseTs({}); setProgress({});
    userPicked.current = false;
    let live = true;
    let es: EventSource | null = null;
    (async () => {
      const j = await getJob(id); if (!live) return; setJob(j);
      const m = await getMetrics(id); if (!live) return;
      t0.current = m[0]?.ts_ms ?? Date.now();
      setSamples(m);
      // Stored phase timestamps — so marks show on terminal jobs that never open the SSE.
      getPhases(id).then((evs) => { if (live) setPhaseTs(Object.fromEntries(evs.map((e) => [e.phase, e.ts_ms]))); }).catch(() => {});
      // Stream while the job runs, OR while end-stage summaries (generated after the
      // job is terminal) are still pending — the server keeps the per-job channel open
      // until they finish and closes it with a `summaries_done` event. Skip reaped jobs
      // and terminal jobs whose summaries are all in.
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
          if (v.kind === "metric") setSamples((s) => [...s, v as any].map(toSample));
          if (v.kind === "phase" && v.phase && v.ts_ms)
            setPhaseTs((p) => (p[v.phase] ? p : { ...p, [v.phase]: v.ts_ms }));
          if (v.kind === "summary_progress" && v.field)
            setProgress((p) => ({ ...p, [v.field]: v.tokens ?? 0 }));
          if (v.kind === "summary" && v.field)
            setProgress((p) => { const n = { ...p }; delete n[v.field]; return n; });
          if (v.kind === "phase" || v.kind === "done" || v.kind === "summary") getJob(id).then(setJob);
          if (v.kind === "summaries_done") es?.close();
        } catch {}
      };
    })();
    getLog(id, "bundle").then(setBundleText).catch(() => setBundleText(""));
    return () => { live = false; es?.close(); };
  }, [id]);

  // On failure, jump to the orchestrator log (the reliable failure reason) unless the
  // user has already chosen a tab themselves.
  useEffect(() => {
    if (!userPicked.current && job?.status === "failed") setLogKind("run");
  }, [job?.status]);

  const data = samples.map((s) => ({
    t: Math.max(0, Math.round(((s.ts_ms ?? (s as any).ts_ms) - t0.current) / 1000)),
    RAM: +mib(s.rss_bytes ?? (s as any).rss),
    Disk: +mib(s.disk_bytes ?? (s as any).disk),
  }));
  const marks = Object.entries(phaseTs)
    .map(([phase, ts]) => ({ phase, t: Math.max(0, Math.round((ts - t0.current) / 1000)) }))
    .sort((a, b) => a.t - b.t);
  // Stagger labels onto lower rows so close-together phases don't overlap.
  const maxT = data.length ? data[data.length - 1].t : 0;
  const GAP = Math.max(1, maxT * 0.13); // ponytail: ~label-width as a fraction of
                                        // the time axis; bump if labels still touch
  const rowLastT: number[] = [];
  const marksRows = marks.map((m) => {
    let r = rowLastT.findIndex((last) => m.t - last >= GAP);
    if (r === -1) r = rowLastT.length;        // need a new row
    rowLastT[r] = m.t;
    return { ...m, row: r };
  });

  return (
    <div>
      {cmp ? (
        <div className="sidebyside">
          <IssuesCard id={id} variant="baseline" label="baseline" status={job?.status} />
          <IssuesCard id={id} variant="patched" label="patched" status={job?.status} />
        </div>
      ) : (
        <IssuesCard id={id} status={job?.status} />
      )}
      <section className="card">
        <h2>Job #{id} {job && <span style={{ color: statusColor(job.status) }}>· {job.status}</span>}
          {cmp && <span className="muted"> · {cmp === "thread" ? "thread-compare" : "patch-compare"} (baseline vs patched)</span>}</h2>
        <div className="stepper">
          {PHASES.map((p) => (
            <span key={p} className={"step " + stepClass(job, p)}>{p}</span>
          ))}
        </div>
        <SummaryLine job={job} field="repro" icon="📝" text={job?.repro_summary ?? null}
          progress={progress} due={job != null && job.status !== "queued"} summarizerReady={summarizerReady} />
        <SummaryLine job={job} field="result" icon="✅" text={job?.result_summary ?? null}
          progress={progress} due={job?.status === "done" || job?.status === "failed"} summarizerReady={summarizerReady} />
        {job && (
          <p className="muted">
            exit {job.exit_code ?? "—"} · peak RAM {gib(job.ram_peak)} GB · peak disk {gib(job.disk_peak)} GB
          </p>
        )}
      </section>
      {(job?.detail || progress["detail"] !== undefined ||
        job?.status === "done" || job?.status === "failed") && (
        <section className="card">
          <h2>Job summary{summaryTip(job, "detail") &&
            <span className="muted" style={{ fontWeight: "normal", fontSize: "0.7em" }}
              title={summaryTip(job, "detail")}> ⓘ</span>}</h2>
          {job?.detail
            ? <div className="md" title={summaryTip(job, "detail")}>
                <ReactMarkdown remarkPlugins={[remarkGfm]}>{job.detail}</ReactMarkdown>
              </div>
            : <p className="muted">{progress["detail"] !== undefined
                ? `generating… ${progress["detail"]} tok`
                : (summarizerReady ? "⏳ pending" : "⏳ summarizer warming up…")}</p>}
        </section>
      )}
      {bundleText.trim() && (
        <section className="card">
          <div className="cardhead">
            <h2>Reproducer</h2>
            <span>
              <button className="linkbtn" onClick={() => setMaxRepro(true)}>Maximize</button>
              {" · "}
              <button className="linkbtn" onClick={() => onEdit(bundleText)}>Edit reproducer</button>
            </span>
          </div>
          {bundle.files.length || bundle.meta.length
            ? <BundlePreview parsed={bundle} />
            : <pre className="log">{bundleText}</pre>}
        </section>
      )}
      {maxRepro && (
        <div className="modal" onClick={() => setMaxRepro(false)}>
          <div className="modal-body" onClick={(e) => e.stopPropagation()}>
            <button className="modal-close" onClick={() => setMaxRepro(false)} aria-label="close">×</button>
            <h2>Reproducer</h2>
            {bundle.files.length || bundle.meta.length
              ? <BundlePreview parsed={bundle} />
              : <pre className="log">{bundleText}</pre>}
          </div>
        </div>
      )}
      <section className="card">
        <h2>Resource usage</h2>
        <ResponsiveContainer width="100%" height={220}>
          <LineChart data={data}>
            <CartesianGrid strokeDasharray="3 3" stroke="#8b949e" strokeOpacity={0.3} />
            <XAxis dataKey="t" type="number" domain={[0, "dataMax"]} stroke="#8b949e" unit="s" />
            <YAxis stroke="#8b949e" unit="M" />
            <Tooltip contentStyle={{ background: "var(--card)", border: "1px solid var(--border)" }} />
            <Legend />
            {marksRows.map((m) => (
              <ReferenceLine key={m.phase} x={m.t} stroke="#8b949e" strokeDasharray="4 3"
                label={({ viewBox }: any) => {
                  const nearRight = maxT > 0 && m.t > maxT * 0.85;
                  const x = (viewBox.x as number) + (nearRight ? -3 : 3);
                  const y = (viewBox.y as number) + 4 + m.row * 12; // +4 clears the top Y tick
                  return (
                    <text x={x} y={y} fill="#8b949e" fontSize={10}
                      textAnchor={nearRight ? "end" : "start"} dominantBaseline="hanging">
                      {m.phase}
                    </text>
                  );
                }} />
            ))}
            <Line type="monotone" dataKey="RAM" stroke="#58a6ff" dot={false} isAnimationActive={false} />
            <Line type="monotone" dataKey="Disk" stroke="#bc8cff" dot={false} isAnimationActive={false} />
          </LineChart>
        </ResponsiveContainer>
      </section>
      <section className="card">
        <div className="tabs">
          {LOG_KINDS.map((k) => (
            <button key={k} className={logKind === k ? "tab active" : "tab"}
              onClick={() => { userPicked.current = true; setLogKind(k); }}>{k}</button>
          ))}
        </div>
        {job?.reaped_ms != null && <p className="muted">logs expired (job dir reclaimed after retention)</p>}
        {cmp ? (
          <div className="sidebyside">
            <div><div className="fname">baseline</div><LogPane id={id} kind={logKind} variant="baseline" status={job?.status} /></div>
            <div><div className="fname">patched</div><LogPane id={id} kind={logKind} variant="patched" status={job?.status} /></div>
          </div>
        ) : (
          <LogPane id={id} kind={logKind} status={job?.status} />
        )}
      </section>
    </div>
  );
}

// One log pane: fetches the chosen log kind (optionally for a compare variant) and
// refetches as the job advances. Compare mode renders two of these side by side.
function LogPane({ id, kind, variant, status }: { id: number; kind: LogKind; variant?: string; status?: string }) {
  const [text, setText] = useState("");
  useEffect(() => { getLog(id, kind, variant).then(setText); }, [id, kind, variant, status]);
  return <pre className="log">{text}</pre>;
}

// Issues card: server-side grep of the (variant's) logs, one tab per source, call
// traces folded. Renders nothing when empty for a single job; keeps a labeled slot
// for a compare column so the two columns stay aligned.
function IssuesCard({ id, variant, label, status }: { id: number; variant?: string; label?: string; status?: string }) {
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
      ? <section className="card"><h2>⚠ Issues · {label}</h2><p className="muted">none</p></section>
      : null;
  }
  return (
    <section className="card issues-card">
      <h2>⚠ Issues{label ? ` · ${label}` : ""}</h2>
      <div className="tabs">
        {issues.map((s) => (
          <button key={s.file} className={active?.file === s.file ? "tab active" : "tab"}
            onClick={() => setIssueTab(s.file)}>
            {s.file.replace(/\.log$/, "")} ({s.blocks.reduce((n, b) => n + b.head.length, 0)})
          </button>
        ))}
      </div>
      {active?.blocks.map((b, i) => <IssueBlock key={i} head={b.head} trace={b.trace} />)}
    </section>
  );
}

// The one place to review a bundle before running it: toggle between editing the
// raw markdown and the structured reproducer view (highlighted C/kconf/bash), then
// run. Opened automatically on paste or when an example is picked.
function BundleModal(
  { bundle, theme, onChange, onRun, onClose }:
  { bundle: string; theme: Theme; onChange: (s: string) => void; onRun: () => void; onClose: () => void },
) {
  const [view, setView] = useState<"edit" | "repro">("edit");
  const parsed = useMemo(() => parseBundle(bundle), [bundle]);
  const cmp = compareMode(parsed);
  const threadVal = parsed.meta.find((m) => m.key === "thread-compare")?.value ?? "";
  return (
    <div className="modal" onClick={onClose}>
      <div className="modal-body" onClick={(e) => e.stopPropagation()}>
        <button className="modal-close" onClick={onClose} aria-label="close">×</button>
        <div className="cardhead">
          <div className="tabs">
            <button className={view === "edit" ? "tab active" : "tab"} onClick={() => setView("edit")}>Edit</button>
            <button className={view === "repro" ? "tab active" : "tab"} onClick={() => setView("repro")}>Reproducer</button>
          </div>
          <button onClick={onRun} disabled={!bundle.trim()}>Run reproducer</button>
        </div>
        {/* Compare toggles: write into the frontmatter so the run produces baseline +
            patched side by side. patch-compare needs a patch: in the bundle. */}
        <div className="bartools">
          <label><input type="checkbox" checked={cmp === "patch"}
            onChange={(e) => onChange(upsertMeta(bundle, "patch-compare", e.target.checked ? "true" : "false"))} />
            {" "}Compare with / without the patch</label>
          <span className="barsep" />
          <label>Compare vs lore thread:{" "}
            <input type="text" className="threadurl" placeholder="https://lore.kernel.org/…" value={threadVal}
              onChange={(e) => onChange(upsertMeta(bundle, "thread-compare", e.target.value.trim()))} /></label>
        </div>
        {view === "edit" ? (
          <div data-color-mode={theme}>
            <MDEditor value={bundle} onChange={(v) => onChange(v ?? "")} height={460} />
          </div>
        ) : (
          <BundlePreview parsed={parsed} />
        )}
      </div>
    </div>
  );
}

// One issue report: the description is always shown; the call trace (kernel stack)
// is folded by default and revealed with a button.
function IssueBlock({ head, trace }: { head: string[]; trace: string[] }) {
  const [open, setOpen] = useState(false);
  return (
    <div className="issue-block">
      <pre className="log">{head.join("\n")}</pre>
      {trace.length > 0 && (
        <>
          <button className="linkbtn" onClick={() => setOpen((o) => !o)}>
            {open ? "▾ hide call trace" : `▸ show call trace (${trace.length} lines)`}
          </button>
          {open && <pre className="log">{trace.join("\n")}</pre>}
        </>
      )}
    </div>
  );
}

// Pick an arborium language from a bundle file. Only c/bash are built into the
// server; everything else renders as plain text.
function langOf(name: string, role: string): string | null {
  if (/\.(c|h)$/i.test(name)) return "c";
  // kconfig (.config) is A=y with # comments — bash highlights those well enough.
  if (role === "init" || role === "kconf" || /\.(sh|bash|config)$/i.test(name)) return "bash";
  return null;
}

// A code block, syntax-highlighted server-side via arborium. Falls back to plain
// text while the highlight request is in flight or for unsupported languages.
function Code({ body, lang }: { body: string; lang: string | null }) {
  const [html, setHtml] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    if (lang) highlight(lang, body).then((h) => { if (live) setHtml(h); });
    else setHtml(null);
    return () => { live = false; };
  }, [body, lang]);
  return html
    ? <pre className="arb log"><code dangerouslySetInnerHTML={{ __html: html }} /></pre>
    : <pre className="log">{body}</pre>;
}

// Structured view of a pasted bundle: frontmatter metadata + one tab per role present.
function BundlePreview({ parsed }: { parsed: ParsedBundle }) {
  const roles = useMemo(() => rolesOf(parsed), [parsed]);
  const [tab, setTab] = useState(roles[0] ?? "");
  useEffect(() => { if (!roles.includes(tab)) setTab(roles[0] ?? ""); }, [roles, tab]);

  const get = (k: string) => parsed.meta.find((m) => m.key === k)?.value;
  const commit = get("commit"), arch = get("arch"), patch = get("patch"), url = get("url");
  const cmp = compareMode(parsed), threadCompare = get("thread-compare");
  const requestsKernel = !!(commit || patch || url || threadCompare);

  return (
    <div className="preview">
      {parsed.meta.length ? (
        <dl className="meta">
          {/* A bundle that requests a kernel always builds Linus's tree,
              so its commit tree-ish links to GitHub. */}
          {requestsKernel && <div><dt>repo</dt>
            <dd>torvalds/linux</dd></div>}
          {commit && <div><dt>commit</dt><dd>
            <a href={githubTree(commit)} target="_blank" rel="noreferrer">{commit}</a>
          </dd></div>}
          {arch && <div><dt>arch</dt><dd>{arch}</dd></div>}
          {patch && <div><dt>patch</dt><dd>{patch}</dd></div>}
          {cmp === "patch" && <div><dt>compare</dt><dd>baseline vs patched (with / without patch)</dd></div>}
          {threadCompare && <div><dt>thread-compare</dt><dd>baseline vs series · {threadCompare}</dd></div>}
          {url && url !== KERNEL_URL && <div><dt>url</dt>
            <dd className="ignored"><s>{url}</s> · ignored</dd></div>}
        </dl>
      ) : (
        <p className="muted">no metadata block (builds LINUX_SRC as-is)</p>
      )}

      {roles.length ? (
        <>
          <div className="tabs">
            {roles.map((r) => (
              <button key={r} className={tab === r ? "tab active" : "tab"} onClick={() => setTab(r)}>{r}</button>
            ))}
          </div>
          {parsed.files.filter((f) => f.role === tab).map((f, idx) => (
            <div key={idx}>
              <div className="fname">{f.name}</div>
              <Code body={f.body} lang={langOf(f.name, f.role)} />
            </div>
          ))}
        </>
      ) : (
        <p className="muted">no code blocks detected</p>
      )}
    </div>
  );
}

// Modal overlay rendering the embedded reproducer spec (docs/reproducer-spec.md).
function SpecModal({ onClose }: { onClose: () => void }) {
  return (
    <div className="modal" onClick={onClose}>
      <div className="modal-body" onClick={(e) => e.stopPropagation()}>
        <button className="modal-close" onClick={onClose} aria-label="close">×</button>
        <div className="md"><ReactMarkdown remarkPlugins={[remarkGfm]}>{specMd}</ReactMarkdown></div>
      </div>
    </div>
  );
}

function toSample(v: any): Sample {
  return v.rss_bytes != null ? v : { ts_ms: v.ts_ms, rss_bytes: v.rss, disk_bytes: v.disk };
}
function stepClass(job: Job | null, phase: string): string {
  if (!job) return "";
  const cur = PHASES.indexOf(job.phase ?? "");
  const idx = PHASES.indexOf(phase);
  if (job.status === "failed" && idx === cur) return "fail";
  if (idx < cur || job.status === "done") return "done";
  if (idx === cur) return "cur";
  return "";
}

const CSS = `
  :root {
    color-scheme: dark;
    --bg: #0d1117; --card: #161b22; --subtle: #21262d; --border: #30363d;
    --fg: #c9d1d9; --muted: #8b949e; --accent: #58a6ff; --tab-active: #1f6feb;
    --overlay: rgba(1, 4, 9, .7);
  }
  :root[data-theme="light"] {
    color-scheme: light;
    --bg: #ffffff; --card: #f6f8fa; --subtle: #eaeef2; --border: #d0d7de;
    --fg: #1f2328; --muted: #656d76; --accent: #0969da; --tab-active: #0969da;
    --overlay: rgba(140, 149, 159, .4);
  }
  body { margin: 0; background: var(--bg); color: var(--fg); font: 14px/1.5 system-ui, sans-serif; }
  .wrap { max-width: 1200px; margin: 0 auto; padding: 16px 24px; }
  h1 { font-size: 20px; } h2 { font-size: 14px; text-transform: uppercase; color: var(--muted); letter-spacing: .04em; margin: 0 0 10px; }
  .cols { display: grid; grid-template-columns: 380px 1fr; gap: 16px; align-items: start; }
  .card { background: var(--card); border: 1px solid var(--border); border-radius: 8px; padding: 14px; margin-bottom: 16px; }
  textarea { width: 100%; height: 140px; box-sizing: border-box; background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; font-family: ui-monospace, monospace; padding: 8px; }
  .paste { width: 100%; box-sizing: border-box; background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; font-family: ui-monospace, monospace; padding: 9px; margin-bottom: 10px; }
  .paste:focus { outline: none; border-color: var(--accent); }
  .unlock { max-width: 460px; } .unlock code { color: var(--fg); }
  .unlock input { width: 100%; box-sizing: border-box; background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; font-family: ui-monospace, monospace; padding: 8px; }
  button { background: #238636; color: #fff; border: 0; border-radius: 6px; padding: 7px 14px; cursor: pointer; margin-top: 8px; }
  .jobs { list-style: none; margin: 0; padding: 0; max-height: 280px; overflow: auto; }
  .jobs li { padding: 6px 8px; border-radius: 6px; cursor: pointer; display: flex; flex-direction: column; gap: 2px; }
  .jobs li.active { background: var(--subtle); }
  .jobrow { display: flex; align-items: center; gap: 6px; flex-wrap: wrap; }
  .jobsum { font-size: .82em; color: var(--muted); line-height: 1.3; padding-left: 14px; }
  .jobs em { color: var(--muted); font-style: normal; } .ph { color: var(--muted); }
  .srclink { margin-left: auto; font-size: .82em; color: var(--accent, #58a6ff); text-decoration: none; }
  .candactions { padding-left: 14px; } .candactions .chip { margin-top: 4px; }
  .summary { background: var(--subtle); border-left: 3px solid var(--accent, #58a6ff);
             padding: 8px 10px; border-radius: 6px; margin: 8px 0; line-height: 1.4; }
  .detail { white-space: pre-wrap; line-height: 1.5; margin: 0; }
  .summarizer { margin-left: auto; font-size: .85em; }
  .shorttitle { color: var(--accent, #58a6ff); font-weight: 600; min-width: 0; overflow-wrap: anywhere; }
  .dot { width: 9px; height: 9px; border-radius: 50%; display: inline-block; }
  .muted { color: var(--muted); }
  .issues-card { border-color: #f85149; } .issues-card h2 { color: #f85149; }
  .issue-block { margin-bottom: 10px; } .issue-block .linkbtn { margin: 4px 0; }
  .stepper { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 8px; }
  .step { padding: 3px 9px; border-radius: 999px; border: 1px solid var(--border); color: var(--muted); font-size: 12px; }
  .step.cur { border-color: #d29922; color: #d29922; } .step.done { border-color: #3fb950; color: #3fb950; } .step.fail { border-color: #f85149; color: #f85149; }
  .tabs { display: flex; gap: 4px; margin-bottom: 8px; }
  .tab { background: var(--subtle); color: var(--fg); margin: 0; } .tab.active { background: var(--tab-active); color: #fff; }
  .log { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 10px; max-height: 360px; overflow: auto; white-space: pre-wrap; font-family: ui-monospace, monospace; font-size: 12px; }
  .topbar { display: flex; align-items: baseline; gap: 12px; }
  .cardhead { display: flex; align-items: center; justify-content: space-between; }
  .linkbtn { background: none; border: 0; color: var(--accent); cursor: pointer; padding: 0; margin: 0; font-size: 13px; text-decoration: underline; }
  .preview { margin-bottom: 8px; }
  .meta { margin: 0 0 8px; } .meta > div { display: flex; gap: 8px; padding: 2px 0; }
  .meta dt { color: var(--muted); min-width: 64px; } .meta dd { margin: 0; font-family: ui-monospace, monospace; word-break: break-all; }
  .fname { color: var(--muted); font-family: ui-monospace, monospace; font-size: 12px; margin: 8px 0 2px; }
  .examples { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin-bottom: 10px; }
  .exlabel { color: var(--muted); font-size: 12px; }
  .bartools { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin-bottom: 8px; }
  .bartools label { display: inline-flex; align-items: center; gap: 5px; color: var(--muted); font-size: 13px; }
  .threadurl { background: var(--bg); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; font-family: ui-monospace, monospace; font-size: 12px; padding: 4px 7px; width: 280px; }
  .sidebyside { display: grid; grid-template-columns: 1fr 1fr; gap: 12px; align-items: start; }
  .chip { background: var(--subtle); color: var(--fg); border: 1px solid var(--border); border-radius: 6px; padding: 3px 9px; font-size: 12px; margin: 0; font-family: ui-monospace, monospace; cursor: pointer; }
  .chip:hover { border-color: var(--accent); }
  .barsep { width: 1px; align-self: stretch; background: var(--border); margin: 0 2px; }
  .ignored { color: var(--muted); }
  .modal { position: fixed; inset: 0; background: var(--overlay); display: flex; align-items: flex-start; justify-content: center; padding: 40px 16px; overflow: auto; z-index: 10; }
  .modal-body { background: var(--card); border: 1px solid var(--border); border-radius: 8px; max-width: 820px; width: 100%; padding: 20px 28px 28px; position: relative; }
  .modal-close { position: absolute; top: 6px; right: 12px; background: none; border: 0; color: var(--muted); font-size: 24px; line-height: 1; cursor: pointer; margin: 0; padding: 4px; }
  .md { color: var(--fg); } .md a { color: var(--accent); }
  .md h1, .md h2, .md h3 { color: var(--fg); text-transform: none; letter-spacing: 0; }
  .md h1 { font-size: 20px; } .md h2 { font-size: 16px; } .md h3 { font-size: 14px; }
  .md table { border-collapse: collapse; margin: 8px 0; font-size: 13px; }
  .md th, .md td { border: 1px solid var(--border); padding: 4px 9px; text-align: left; }
  .md th { background: var(--subtle); }
  .md code { background: var(--bg); border: 1px solid var(--border); border-radius: 4px; padding: 1px 4px; font-family: ui-monospace, monospace; font-size: 12px; }
  .md pre { background: var(--bg); border: 1px solid var(--border); border-radius: 6px; padding: 10px; overflow: auto; }
  .md pre code { border: 0; padding: 0; background: none; }
`;
