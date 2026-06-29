// Pure formatting / status helpers shared across the dashboard, extracted from the
// old single-file App so components stay presentational.
import type { Job, JobSummary, Sample } from "../api";

export const PHASES = ["fetch", "configure", "build", "boot", "insmod", "run", "done"];

// Scaffold jobs prepend a "scaffold" stage (the opencode agent generating the bundle)
// before the normal pipeline; other jobs use PHASES as-is.
export const phaseList = (job: Job | null): string[] =>
  job?.source === "scaffold" ? ["scaffold", ...PHASES] : PHASES;

// Selected job is encoded in the path as /job/ID; null = nothing selected (root).
export const jobFromPath = (): number | null => {
  const m = location.pathname.match(/^\/job\/(\d+)/);
  return m ? +m[1] : null;
};

export const statusColor = (s: string) =>
  s === "done" ? "#3fb950" : s === "failed" ? "#f85149"
    : s === "running" ? "#d29922" : "#8b949e";

// Per-summary generation metadata (from job.summary_meta JSON) → hover tooltip text.
// `took` is a human-readable duration formatted server-side; `ms` is kept for older rows.
export type SummaryMeta = { ms: number; took?: string; tokens: number; model: string };

export function summaryMeta(job: Job | null, field: string): SummaryMeta | null {
  if (!job?.summary_meta) return null;
  try { return (JSON.parse(job.summary_meta) as Record<string, SummaryMeta>)[field] ?? null; }
  catch { return null; }
}

export function summaryTip(job: Job | null, field: string): string | undefined {
  const m = summaryMeta(job, field);
  if (!m) return undefined;
  return `generated in ${m.took ?? `${m.ms} ms`} · ${m.model} · ${m.tokens} tokens`;
}

// Per-server summary tooltip: model · duration · tokens.
export function jobSummaryTip(s: JobSummary): string {
  return [s.model, s.ms != null ? `${Math.round(s.ms / 1000)}s` : null,
    s.tokens != null ? `${s.tokens} tok` : null].filter(Boolean).join(" · ");
}

// SSE metric frames arrive with compact keys (rss/disk); the REST /metrics endpoint
// sends the Sample shape (rss_bytes/disk_bytes). Normalize both to Sample at the
// boundary so everything downstream sees one shape.
type MetricFrame = { ts_ms: number; rss: number; disk: number };
export function toSample(v: Sample | MetricFrame): Sample {
  return "rss_bytes" in v ? v : { ts_ms: v.ts_ms, rss_bytes: v.rss, disk_bytes: v.disk };
}

export function stepClass(job: Job | null, phase: string): string {
  if (!job) return "";
  const list = phaseList(job);
  const cur = list.indexOf(job.phase ?? "");
  const idx = list.indexOf(phase);
  if (job.status === "failed" && idx === cur) return "fail";
  if (idx < cur || job.status === "done") return "done";
  if (idx === cur) return "cur";
  return "";
}
