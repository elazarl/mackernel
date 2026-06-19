import { useEffect, useMemo, useRef, useState } from "react";
import ReactMarkdown from "react-markdown";
import remarkGfm from "remark-gfm";
import MDEditor from "@uiw/react-md-editor";
import {
  Bar, BarChart, CartesianGrid, Legend, Line, LineChart,
  ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";
import {
  eventsUrl, getJob, getLog, getMetrics, getPeaks, gib, hasToken, Job, listJobs,
  mib, Peak, Sample, setToken, submit,
} from "./api";
import {
  appendFile, BOILERPLATE, EXAMPLES, githubTree, KERNEL_URL, parseBundle,
  ParsedBundle, rolesOf, upsertMeta,
} from "./bundle";
import specMd from "../../../docs/reproducer-spec.md?raw";

const PHASES = ["fetch", "configure", "build", "boot", "insmod", "run", "done"];
// `run` is the run-kernel.py orchestrator log: it always carries the failure reason
// (a die() message or an uncaught traceback), even for early crashes that never reach
// the phase-specific logs — so it's the reliable place to look when a job fails.
// `issues` is server-side: a grep of every log for error/fatal/panic/sanitizer markers.
// `dmesg` is the guest kernel ring buffer; `console` is the raw QEMU serial capture.
const LOG_KINDS = ["fetch", "compile", "console", "dmesg", "exec", "run", "issues"] as const;
type LogKind = (typeof LOG_KINDS)[number];

const statusColor = (s: string) =>
  s === "done" ? "#3fb950" : s === "failed" ? "#f85149"
    : s === "running" ? "#d29922" : "#8b949e";

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
  const [sel, setSel] = useState<number | null>(null);
  const [bundle, setBundle] = useState("");
  const [showSpec, setShowSpec] = useState(false);

  useEffect(() => {
    const tick = async () => {
      try { setJobs(await listJobs()); setPeaks(await getPeaks()); } catch {}
    };
    tick();
    const h = setInterval(tick, 3000);
    return () => clearInterval(h);
  }, []);

  const onSubmit = async () => {
    if (!bundle.trim()) return;
    const { id } = await submit(bundle);
    setBundle("");
    setSel(id);
    setJobs(await listJobs());
  };

  return (
    <div className="wrap">
      <style>{CSS}</style>
      <div className="topbar">
        <h1>mackernel — reproducer runner</h1>
        <button className="linkbtn" onClick={() => setShowSpec(true)}>Spec</button>
      </div>
      {showSpec && <SpecModal onClose={() => setShowSpec(false)} />}
      <div className="cols">
        <div className="left">
          <section className="card">
            <h2>Submit a bundle</h2>
            <div className="examples">
              <span className="exlabel">Examples:</span>
              {EXAMPLES.map((ex) => (
                <button key={ex.label} className="chip" title={ex.blurb}
                  onClick={() => setBundle(ex.bundle)}>
                  {ex.label}
                </button>
              ))}
            </div>
            <RawTools text={bundle} onChange={setBundle} />
            {/* Editable markdown: source + live rendered preview (toolbar toggles edit/live/preview). */}
            <div data-color-mode="dark">
              <MDEditor value={bundle} onChange={(v) => setBundle(v ?? "")} height={300}
                textareaProps={{ placeholder: "paste a SKILL.md-style bundle (---metadata---, user:/module:/kconf:/init: blocks)" }} />
            </div>
            <button onClick={onSubmit} disabled={!bundle.trim()}>Run reproducer</button>
          </section>
          <section className="card">
            <h2>Jobs</h2>
            <ul className="jobs">
              {jobs.map((j) => (
                <li key={j.id} className={sel === j.id ? "active" : ""} onClick={() => setSel(j.id)}>
                  <span className="dot" style={{ background: statusColor(j.status) }} />
                  #{j.id} <em>{j.status}</em>
                  {j.phase && j.status === "running" && <span className="ph"> · {j.phase}</span>}
                  {j.exit_code != null && <span className="ph"> · exit {j.exit_code}</span>}
                  {j.reaped_ms != null && <span className="ph"> · logs expired</span>}
                </li>
              ))}
            </ul>
          </section>
          <section className="card">
            <h2>Peak resource usage (per job)</h2>
            <ResponsiveContainer width="100%" height={180}>
              <BarChart data={peaks.map((p) => ({ id: `#${p.id}`, RAM: +gib(p.ram_peak), Disk: +gib(p.disk_peak) }))}>
                <CartesianGrid strokeDasharray="3 3" stroke="#30363d" />
                <XAxis dataKey="id" stroke="#8b949e" /><YAxis stroke="#8b949e" unit="G" />
                <Tooltip contentStyle={{ background: "#161b22", border: "1px solid #30363d" }} />
                <Legend /><Bar dataKey="RAM" fill="#58a6ff" /><Bar dataKey="Disk" fill="#bc8cff" />
              </BarChart>
            </ResponsiveContainer>
          </section>
        </div>
        <div className="right">
          {sel == null ? <p className="muted">Select a job to see live progress, metrics, and logs.</p>
            : <JobDetail id={sel} onEdit={(text) => {
                setBundle(text);
                window.scrollTo({ top: 0, behavior: "smooth" });
              }} />}
        </div>
      </div>
    </div>
  );
}

function JobDetail({ id, onEdit }: { id: number; onEdit: (text: string) => void }) {
  const [job, setJob] = useState<Job | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [logKind, setLogKind] = useState<LogKind>("exec");
  const [logText, setLogText] = useState("");
  const [bundleText, setBundleText] = useState("");
  const bundle = useMemo(() => parseBundle(bundleText), [bundleText]);
  const t0 = useRef<number>(0);
  const userPicked = useRef(false);

  useEffect(() => {
    setSamples([]); setJob(null);
    userPicked.current = false;
    let live = true;
    (async () => {
      const j = await getJob(id); if (!live) return; setJob(j);
      const m = await getMetrics(id); if (!live) return;
      t0.current = m[0]?.ts_ms ?? Date.now();
      setSamples(m);
    })();
    const es = new EventSource(eventsUrl(id));
    es.onmessage = (e) => {
      try {
        const v = JSON.parse(e.data);
        if (v.kind === "metric") setSamples((s) => [...s, v as any].map(toSample));
        if (v.kind === "phase" || v.kind === "done") getJob(id).then(setJob);
      } catch {}
    };
    getLog(id, "bundle").then(setBundleText).catch(() => setBundleText(""));
    return () => { live = false; es.close(); };
  }, [id]);

  // On failure, jump to the orchestrator log (the reliable failure reason) unless the
  // user has already chosen a tab themselves.
  useEffect(() => {
    if (!userPicked.current && job?.status === "failed") setLogKind("run");
  }, [job?.status]);

  useEffect(() => { getLog(id, logKind).then(setLogText); }, [id, logKind, job?.status]);

  const data = samples.map((s) => ({
    t: Math.max(0, Math.round(((s.ts_ms ?? (s as any).ts_ms) - t0.current) / 1000)),
    RAM: +mib(s.rss_bytes ?? (s as any).rss),
    Disk: +mib(s.disk_bytes ?? (s as any).disk),
  }));

  return (
    <div>
      <section className="card">
        <h2>Job #{id} {job && <span style={{ color: statusColor(job.status) }}>· {job.status}</span>}</h2>
        <div className="stepper">
          {PHASES.map((p) => (
            <span key={p} className={"step " + stepClass(job, p)}>{p}</span>
          ))}
        </div>
        {job && (
          <p className="muted">
            exit {job.exit_code ?? "—"} · peak RAM {gib(job.ram_peak)} GB · peak disk {gib(job.disk_peak)} GB
          </p>
        )}
      </section>
      {bundleText.trim() && (
        <section className="card">
          <div className="cardhead">
            <h2>Reproducer</h2>
            <button className="linkbtn" onClick={() => onEdit(bundleText)}>Edit reproducer</button>
          </div>
          {bundle.files.length || bundle.meta.length
            ? <BundlePreview parsed={bundle} />
            : <pre className="log">{bundleText}</pre>}
        </section>
      )}
      <section className="card">
        <h2>Resource usage</h2>
        <ResponsiveContainer width="100%" height={220}>
          <LineChart data={data}>
            <CartesianGrid strokeDasharray="3 3" stroke="#30363d" />
            <XAxis dataKey="t" stroke="#8b949e" unit="s" />
            <YAxis stroke="#8b949e" unit="M" />
            <Tooltip contentStyle={{ background: "#161b22", border: "1px solid #30363d" }} />
            <Legend />
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
        <pre className="log">{logText}</pre>
      </section>
    </div>
  );
}

// Raw-mode toolbar: edit commit/arch frontmatter and insert file boilerplate.
function RawTools({ text, onChange }: { text: string; onChange: (s: string) => void }) {
  const meta = useMemo(() => parseBundle(text), [text]);
  const valueOf = (k: string) => meta.meta.find((m) => m.key === k)?.value;
  const editMeta = (k: string) => {
    const v = window.prompt(`Set ${k}:`, valueOf(k) ?? "");
    if (v != null && v.trim() !== "") onChange(upsertMeta(text, k, v.trim()));
  };
  const addFile = (role: string) => {
    const b = BOILERPLATE[role];
    onChange(appendFile(text, role, b.name, b.body));
  };
  const label = (k: string) => (valueOf(k) ? `${k}: ${valueOf(k)}` : `+ ${k}`);
  return (
    <div className="bartools">
      <button className="chip" onClick={() => editMeta("commit")}>{label("commit")}</button>
      <button className="chip" onClick={() => editMeta("arch")}>{label("arch")}</button>
      <span className="barsep" />
      <button className="chip" onClick={() => addFile("user")}>+ C</button>
      <button className="chip" onClick={() => addFile("module")}>+ module</button>
      <button className="chip" onClick={() => addFile("kconf")}>+ kconf</button>
      <button className="chip" onClick={() => addFile("init")}>+ init</button>
    </div>
  );
}

// Structured view of a pasted bundle: frontmatter metadata + one tab per role present.
function BundlePreview({ parsed }: { parsed: ParsedBundle }) {
  const roles = useMemo(() => rolesOf(parsed), [parsed]);
  const [tab, setTab] = useState(roles[0] ?? "");
  useEffect(() => { if (!roles.includes(tab)) setTab(roles[0] ?? ""); }, [roles, tab]);

  const get = (k: string) => parsed.meta.find((m) => m.key === k)?.value;
  const commit = get("commit"), arch = get("arch"), patch = get("patch"), url = get("url");
  const requestsKernel = !!(commit || patch || url);

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
              <pre className="log">{f.body}</pre>
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
  :root { color-scheme: dark; }
  body { margin: 0; background: #0d1117; color: #c9d1d9; font: 14px/1.5 system-ui, sans-serif; }
  .wrap { max-width: 1200px; margin: 0 auto; padding: 16px 24px; }
  h1 { font-size: 20px; } h2 { font-size: 14px; text-transform: uppercase; color: #8b949e; letter-spacing: .04em; margin: 0 0 10px; }
  .cols { display: grid; grid-template-columns: 380px 1fr; gap: 16px; align-items: start; }
  .card { background: #161b22; border: 1px solid #30363d; border-radius: 8px; padding: 14px; margin-bottom: 16px; }
  textarea { width: 100%; height: 140px; box-sizing: border-box; background: #0d1117; color: #c9d1d9; border: 1px solid #30363d; border-radius: 6px; font-family: ui-monospace, monospace; padding: 8px; }
  .unlock { max-width: 460px; } .unlock code { color: #c9d1d9; }
  .unlock input { width: 100%; box-sizing: border-box; background: #0d1117; color: #c9d1d9; border: 1px solid #30363d; border-radius: 6px; font-family: ui-monospace, monospace; padding: 8px; }
  button { background: #238636; color: #fff; border: 0; border-radius: 6px; padding: 7px 14px; cursor: pointer; margin-top: 8px; }
  .jobs { list-style: none; margin: 0; padding: 0; max-height: 280px; overflow: auto; }
  .jobs li { padding: 6px 8px; border-radius: 6px; cursor: pointer; display: flex; align-items: center; gap: 6px; }
  .jobs li.active { background: #21262d; }
  .jobs em { color: #8b949e; font-style: normal; } .ph { color: #8b949e; }
  .dot { width: 9px; height: 9px; border-radius: 50%; display: inline-block; }
  .muted { color: #8b949e; }
  .stepper { display: flex; flex-wrap: wrap; gap: 6px; margin-bottom: 8px; }
  .step { padding: 3px 9px; border-radius: 999px; border: 1px solid #30363d; color: #8b949e; font-size: 12px; }
  .step.cur { border-color: #d29922; color: #d29922; } .step.done { border-color: #3fb950; color: #3fb950; } .step.fail { border-color: #f85149; color: #f85149; }
  .tabs { display: flex; gap: 4px; margin-bottom: 8px; }
  .tab { background: #21262d; color: #c9d1d9; margin: 0; } .tab.active { background: #1f6feb; }
  .log { background: #0d1117; border: 1px solid #30363d; border-radius: 6px; padding: 10px; max-height: 360px; overflow: auto; white-space: pre-wrap; font-family: ui-monospace, monospace; font-size: 12px; }
  .topbar { display: flex; align-items: baseline; gap: 12px; }
  .cardhead { display: flex; align-items: center; justify-content: space-between; }
  .linkbtn { background: none; border: 0; color: #58a6ff; cursor: pointer; padding: 0; margin: 0; font-size: 13px; text-decoration: underline; }
  .preview { margin-bottom: 8px; }
  .meta { margin: 0 0 8px; } .meta > div { display: flex; gap: 8px; padding: 2px 0; }
  .meta dt { color: #8b949e; min-width: 64px; } .meta dd { margin: 0; font-family: ui-monospace, monospace; word-break: break-all; }
  .fname { color: #8b949e; font-family: ui-monospace, monospace; font-size: 12px; margin: 8px 0 2px; }
  .examples { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin-bottom: 10px; }
  .exlabel { color: #8b949e; font-size: 12px; }
  .bartools { display: flex; flex-wrap: wrap; align-items: center; gap: 6px; margin-bottom: 8px; }
  .chip { background: #21262d; color: #c9d1d9; border: 1px solid #30363d; border-radius: 6px; padding: 3px 9px; font-size: 12px; margin: 0; font-family: ui-monospace, monospace; cursor: pointer; }
  .chip:hover { border-color: #58a6ff; }
  .barsep { width: 1px; align-self: stretch; background: #30363d; margin: 0 2px; }
  .ignored { color: #8b949e; }
  .modal { position: fixed; inset: 0; background: rgba(1, 4, 9, .7); display: flex; align-items: flex-start; justify-content: center; padding: 40px 16px; overflow: auto; z-index: 10; }
  .modal-body { background: #161b22; border: 1px solid #30363d; border-radius: 8px; max-width: 820px; width: 100%; padding: 20px 28px 28px; position: relative; }
  .modal-close { position: absolute; top: 6px; right: 12px; background: none; border: 0; color: #8b949e; font-size: 24px; line-height: 1; cursor: pointer; margin: 0; padding: 4px; }
  .md { color: #c9d1d9; } .md a { color: #58a6ff; }
  .md h1, .md h2, .md h3 { color: #c9d1d9; text-transform: none; letter-spacing: 0; }
  .md h1 { font-size: 20px; } .md h2 { font-size: 16px; } .md h3 { font-size: 14px; }
  .md table { border-collapse: collapse; margin: 8px 0; font-size: 13px; }
  .md th, .md td { border: 1px solid #30363d; padding: 4px 9px; text-align: left; }
  .md th { background: #21262d; }
  .md code { background: #0d1117; border: 1px solid #30363d; border-radius: 4px; padding: 1px 4px; font-family: ui-monospace, monospace; font-size: 12px; }
  .md pre { background: #0d1117; border: 1px solid #30363d; border-radius: 6px; padding: 10px; overflow: auto; }
  .md pre code { border: 0; padding: 0; background: none; }
`;
