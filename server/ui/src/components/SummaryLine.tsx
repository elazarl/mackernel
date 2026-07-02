import type { Job } from "../api";
import { summaryTip } from "../lib/format";

// One summary line: the text + hover tooltip once it's generated, a live token count
// while it streams, a "failed" note if the last attempt errored, or a "pending"
// placeholder when it's due but not yet started.
export function SummaryLine(
  { job, field, icon, text, progress, due, summarizerReady, error }:
  { job: Job | null; field: string; icon: string; text: string | null;
    progress: Record<string, number>; due: boolean; summarizerReady: boolean; error?: string },
) {
  if (text) return <p className="summary" title={summaryTip(job, field)}>{icon} {text}</p>;
  const tok = progress[field];
  if (tok !== undefined)
    return <p className="summary"><span className="text-muted">{icon} generating… {tok} tok</span></p>;
  if (error)
    return <p className="summary" title={error}><span style={{ color: "#f85149" }}>{icon} ⚠️ failed: {error}</span></p>;
  if (due)
    return <p className="summary"><span className="text-muted">{icon} {summarizerReady ? "⏳ pending" : "⏳ summarizer warming up…"}</span></p>;
  return null;
}
