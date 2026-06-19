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
  summary: string | null;
}
export interface Sample { ts_ms: number; rss_bytes: number; disk_bytes: number; }
export interface Peak { id: number; ram_peak: number; disk_peak: number; status: string; }

const TOKEN_KEY = "mk_token";
export function token(): string {
  return localStorage.getItem(TOKEN_KEY) || "";
}
export function hasToken(): boolean {
  return token().length > 0;
}
export function setToken(t: string) {
  localStorage.setItem(TOKEN_KEY, t.trim());
}
export function clearToken() {
  localStorage.removeItem(TOKEN_KEY);
}
function headers(): HeadersInit {
  const t = token();
  return t ? { Authorization: `Bearer ${t}` } : {};
}

// Wrap fetch so a rejected token (401) drops the stored token and tells the app to
// re-prompt for the v7.1 commit, instead of silently retrying with a bad token.
async function authed(input: string, init?: RequestInit): Promise<Response> {
  const r = await fetch(input, { ...init, headers: { ...headers(), ...(init?.headers || {}) } });
  if (r.status === 401) {
    clearToken();
    window.dispatchEvent(new Event("mk-unauthorized"));
    throw new Error("unauthorized");
  }
  return r;
}

export async function listJobs(): Promise<Job[]> {
  return (await authed("/api/jobs")).json();
}
export async function getJob(id: number): Promise<Job> {
  return (await authed(`/api/jobs/${id}`)).json();
}
export async function submit(bundle: string): Promise<{ id: number }> {
  return (await authed("/api/jobs", { method: "POST", body: bundle })).json();
}
export async function getMetrics(id: number): Promise<Sample[]> {
  return (await authed(`/api/jobs/${id}/metrics`)).json();
}
export async function getPeaks(): Promise<Peak[]> {
  return (await authed("/api/metrics/peaks")).json();
}
export async function getLog(id: number, kind: string): Promise<string> {
  const r = await authed(`/api/jobs/${id}/logs/${kind}`);
  return r.ok ? r.text() : `(no ${kind} log yet)`;
}
// Server-side syntax highlighting via arborium (tree-sitter, Rust-only).
// Returns highlighted HTML, or null for unsupported languages / parse errors.
export async function highlight(lang: string, code: string): Promise<string | null> {
  const r = await authed(`/api/highlight/${lang}`, { method: "POST", body: code });
  return r.ok ? r.text() : null;
}
export async function highlightCss(): Promise<string> {
  const r = await authed("/api/highlight.css");
  return r.ok ? r.text() : "";
}

// EventSource can't set headers; the token rides as a query param (Phase 5).
export function eventsUrl(id: number): string {
  const t = token();
  return `/api/jobs/${id}/events${t ? `?token=${encodeURIComponent(t)}` : ""}`;
}
// Process-wide stream that pings whenever the job list changes (push, not poll).
export function globalEventsUrl(): string {
  const t = token();
  return `/api/events${t ? `?token=${encodeURIComponent(t)}` : ""}`;
}

export const mib = (b: number) => (b / 1048576).toFixed(0);
export const gib = (b: number) => (b / 1073741824).toFixed(2);
