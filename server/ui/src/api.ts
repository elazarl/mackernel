export interface Job {
  id: number;
  created_ms: number;
  started_ms: number | null;
  finished_ms: number | null;
  status: string;
  phase: string | null;
  exit_code: number | null;
  ram_peak: number;
  disk_peak: number;
  reaped_ms: number | null;
}
export interface Sample { ts_ms: number; rss_bytes: number; disk_bytes: number; }
export interface Peak { id: number; ram_peak: number; disk_peak: number; status: string; }

export function token(): string {
  return localStorage.getItem("mk_token") || "";
}
function headers(): HeadersInit {
  const t = token();
  return t ? { Authorization: `Bearer ${t}` } : {};
}

export async function listJobs(): Promise<Job[]> {
  return (await fetch("/api/jobs", { headers: headers() })).json();
}
export async function getJob(id: number): Promise<Job> {
  return (await fetch(`/api/jobs/${id}`, { headers: headers() })).json();
}
export async function submit(bundle: string): Promise<{ id: number }> {
  return (await fetch("/api/jobs", { method: "POST", headers: headers(), body: bundle })).json();
}
export async function getMetrics(id: number): Promise<Sample[]> {
  return (await fetch(`/api/jobs/${id}/metrics`, { headers: headers() })).json();
}
export async function getPeaks(): Promise<Peak[]> {
  return (await fetch("/api/metrics/peaks", { headers: headers() })).json();
}
export async function getLog(id: number, kind: string): Promise<string> {
  const r = await fetch(`/api/jobs/${id}/logs/${kind}`, { headers: headers() });
  return r.ok ? r.text() : `(no ${kind} log yet)`;
}
// EventSource can't set headers; the token rides as a query param (Phase 5).
export function eventsUrl(id: number): string {
  const t = token();
  return `/api/jobs/${id}/events${t ? `?token=${encodeURIComponent(t)}` : ""}`;
}

export const mib = (b: number) => (b / 1048576).toFixed(0);
export const gib = (b: number) => (b / 1073741824).toFixed(2);
