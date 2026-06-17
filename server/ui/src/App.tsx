import { useEffect, useRef, useState } from "react";
import {
  Bar, BarChart, CartesianGrid, Legend, Line, LineChart,
  ResponsiveContainer, Tooltip, XAxis, YAxis,
} from "recharts";
import {
  eventsUrl, getJob, getLog, getMetrics, getPeaks, gib, hasToken, Job, listJobs,
  mib, Peak, Sample, setToken, submit,
} from "./api";

const PHASES = ["fetch", "configure", "build", "boot", "insmod", "run", "done"];
const LOG_KINDS = ["compile", "dmesg", "exec"] as const;
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
      <h1>mackernel — reproducer runner</h1>
      <div className="cols">
        <div className="left">
          <section className="card">
            <h2>Submit a bundle</h2>
            <textarea value={bundle} onChange={(e) => setBundle(e.target.value)}
              placeholder="paste a SKILL.md-style bundle (---metadata---, user:/module:/kconf:/init: blocks)" />
            <button onClick={onSubmit}>Run reproducer</button>
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
            : <JobDetail id={sel} />}
        </div>
      </div>
    </div>
  );
}

function JobDetail({ id }: { id: number }) {
  const [job, setJob] = useState<Job | null>(null);
  const [samples, setSamples] = useState<Sample[]>([]);
  const [logKind, setLogKind] = useState<LogKind>("exec");
  const [logText, setLogText] = useState("");
  const t0 = useRef<number>(0);

  useEffect(() => {
    setSamples([]); setJob(null);
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
    return () => { live = false; es.close(); };
  }, [id]);

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
            <button key={k} className={logKind === k ? "tab active" : "tab"} onClick={() => setLogKind(k)}>{k}</button>
          ))}
        </div>
        {job?.reaped_ms != null && <p className="muted">logs expired (job dir reclaimed after retention)</p>}
        <pre className="log">{logText}</pre>
      </section>
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
`;
