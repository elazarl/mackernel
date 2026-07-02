//! mackernel-server: REST service that queues reproducer bundles, runs them via
//! run-kernel.py, and exposes status + logs. (Phase 1: REST + serial worker.)
mod bus;
mod db;
mod embed;
mod lkml;
mod metrics;
mod scaffold;
mod sched;
mod seed;
mod summarize;
mod thermometer;

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysinfo::System;

use axum::{
    extract::{Path, Query, Request, State},
    http::{header::{AUTHORIZATION, CONTENT_TYPE}, StatusCode},
    middleware::Next,
    response::sse::{Event, KeepAlive, Sse},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use futures::stream::{Stream, StreamExt};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{error, info, warn};

use bus::Bus;
use db::Db;
use sched::{Cfg, SchedMsg};

// Heap profiling (feature `dhat-heap`): route every allocation through dhat so it can
// attribute the heap to allocation sites. Writes dhat-heap.json when the Profiler drops
// (clean exit / SIGINT via the graceful shutdown below).
#[cfg(feature = "dhat-heap")]
#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// Default bearer token: the commit hash that the `v7.1` tag points to. Acts as a
/// shared secret — the UI asks the user for "the v7.1 commit" and sends it as the
/// bearer token, so only someone who knows that SHA can drive the service. Override
/// with the `MK_TOKEN` env var.
const DEFAULT_TOKEN: &str = "8cd9520d35a6c38db6567e97dd93b1f11f185dc6";

#[derive(Clone)]
struct AppState {
    db: Db,
    work: PathBuf,
    repo: PathBuf,
    tx: mpsc::UnboundedSender<SchedMsg>,
    bus: Bus,
    cfg: Cfg,
    auth_token: Option<String>,
    /// In-process LLM summarizer, populated by a background loader once the model is
    /// downloaded/loaded. `None`/empty until ready (or if loading failed/disabled), in
    /// which case summarization is silently skipped.
    summarizer: Arc<std::sync::OnceLock<Arc<summarize::Summarizer>>>,
    /// Summary requests (job id, stage) that arrived before the model finished
    /// loading; flushed by the background loader once it's ready (instead of being
    /// silently dropped). See `spawn_summary`.
    summary_queue: Arc<std::sync::Mutex<Vec<(i64, &'static str)>>>,
    /// OpenAI-compatible creds for queued scaffold jobs, keyed by job id. Kept in memory
    /// (never on disk) from job creation until the scaffold stage consumes them; see
    /// `scaffold.rs`. A restart drops these, so a still-queued scaffold job fails clearly.
    scaffold_creds: Arc<std::sync::Mutex<HashMap<i64, scaffold::Creds>>>,
}

pub(crate) fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn env_path(key: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Held for the whole run; on drop (clean exit / SIGINT) it writes dhat-heap.json.
    #[cfg(feature = "dhat-heap")]
    let _dhat = dhat::Profiler::new_heap();

    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into()))
        .init();

    // Debug subcommand: `mackernel-server summarize <bundle.md> [logs-dir]` loads the
    // model and prints a summary (end-summary if a logs dir is given, else start). Used
    // to verify the summarizer end-to-end without booting the full service.
    let argv: Vec<String> = std::env::args().collect();
    if argv.get(1).map(String::as_str) == Some("summarize") {
        let bundle_path = argv.get(2).cloned().unwrap_or_default();
        let logs_arg = argv.get(3).cloned();
        let md = std::fs::read_to_string(&bundle_path)?;
        // Summarizer talks to llama-server via reqwest::blocking; run it on a plain
        // thread so its internal runtime isn't created/dropped in async context (the
        // service path already does this via spawn_blocking).
        std::thread::spawn(move || -> anyhow::Result<()> {
            let s = summarize::Summarizer::load()?;
            info!("loaded {} summary backend(s); local RSS {} MB",
                s.backends().len(), s.memory_bytes() / 1_048_576);
            let noop = |_: u32| {};
            // One block per backend, continuing past a backend that errors (e.g. a
            // rate-limited remote) so the others still print.
            for b in s.backends() {
                println!("== backend: {} ({}){}", b.label, b.model, if b.primary { " [primary]" } else { "" });
                let one = || -> anyhow::Result<()> {
                    println!("TITLE:  {}", s.title(b, &md, &noop, 0)?.0);
                    println!("REPRO:  {}", s.summarize_repro(b, &md, &noop, 0)?.0);
                    if let Some(logs) = &logs_arg {
                        let logs = std::path::Path::new(logs);
                        let issues = collect_issues(logs, &watched_patterns(&md));
                        println!("RESULT: {}", s.summarize_result(b, &md, &issues, Some(1), "done", &noop, 0)?.0);
                        println!("DETAIL: {}", s.detail(b, &md, logs, &noop, 0)?.0);
                    }
                    Ok(())
                };
                if let Err(e) = one() {
                    eprintln!("  backend {} failed: {e:#}", b.label);
                }
            }
            Ok(())
        })
        .join()
        .map_err(|_| anyhow::anyhow!("summarize thread panicked"))??;
        return Ok(());
    }

    let bind = std::env::var("MK_SERVER_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    let work = env_path("MK_SERVER_WORK", "./work");
    let repo = env_path("MK_REPO", "..").canonicalize()
        .unwrap_or_else(|_| env_path("MK_REPO", ".."));
    std::fs::create_dir_all(&work)?;
    // Absolute: run-kernel.py chdir's to the repo, so relative job paths would
    // otherwise resolve against the wrong directory.
    let work = work.canonicalize()?;
    if !repo.join("run-kernel.py").is_file() {
        anyhow::bail!("MK_REPO={} has no run-kernel.py (set MK_REPO to the mackernel repo)", repo.display());
    }

    let database = Db::open(&work.join("jobs.duckdb"))?;
    database.recover_orphans(now_ms())?; // any job left mid-flight by a prior run

    // Bearer token for /api/*. Defaults to DEFAULT_TOKEN (the commit hash the v7.1
    // tag points to) so the API is authenticated out of the box; MK_TOKEN overrides
    // it. The UI prompts the user for "the v7.1 commit" and sends that as the bearer.
    let auth_token = std::env::var("MK_TOKEN").ok()
        .filter(|t| !t.is_empty())
        .or_else(|| Some(DEFAULT_TOKEN.to_string()));
    if std::env::var_os("MK_TOKEN").is_none() {
        tracing::info!("MK_TOKEN unset -- using built-in v7.1 token for /api/* auth");
    }

    // The summary model loads in the background so a first-boot download (~1-2.5 GB)
    // doesn't block serving. Requests that arrive before it's ready are queued (see
    // `spawn_summary`) and flushed here on load, rather than silently dropped.
    let summarizer = Arc::new(std::sync::OnceLock::<Arc<summarize::Summarizer>>::new());
    let summary_queue = Arc::new(std::sync::Mutex::new(Vec::<(i64, &'static str)>::new()));

    let (tx, rx) = mpsc::unbounded_channel::<SchedMsg>();
    let state = AppState {
        db: database, work: work.clone(), repo: repo.clone(), tx,
        bus: Bus::default(), cfg: Cfg::from_env(), auth_token,
        summarizer: summarizer.clone(), summary_queue,
        scaffold_creds: Arc::new(std::sync::Mutex::new(HashMap::new())),
    };

    // Seed the demo job (#1) from server/seed/ if it's missing, so the guided tour's
    // /job/1 always resolves to a real job even on a fresh database.
    if let Err(e) = seed::seed_demo_job(&state) {
        warn!("demo job seed failed: {e:#}");
    }

    if summarize::Summarizer::enabled() {
        let slot = summarizer.clone();
        let flush_st = state.clone();
        tokio::task::spawn_blocking(move || match summarize::Summarizer::load() {
            Ok(s) => {
                let labels: Vec<&str> = s.backends().iter().map(|b| b.label.as_str()).collect();
                info!("summary backends ready: [{}] (local RSS {} MB)",
                    labels.join(", "), s.memory_bytes() / 1_048_576);
                // Ping the global SSE bus whenever the opencode queue changes so the 🧠
                // topbar tooltip stays live during long runs (jobs stream doesn't fire then).
                let bus = flush_st.bus.clone();
                s.opencode_queue().set_notifier(std::sync::Arc::new(move || {
                    bus.publish_global(json!({ "kind": "summarizer" }).to_string());
                }));
                let _ = slot.set(Arc::new(s));
                // Flush any summaries requested while the model was warming up.
                let queued: Vec<_> = flush_st.summary_queue.lock().unwrap().drain(..).collect();
                if !queued.is_empty() {
                    info!("flushing {} queued summary request(s)", queued.len());
                }
                for (id, stage) in queued {
                    spawn_summary(&flush_st, id, stage);
                }
            }
            Err(e) => warn!("summary model unavailable, summaries disabled: {e:#}"),
        });
    } else {
        info!("MK_SUMMARY_DISABLE set -- job summaries disabled");
    }

    tokio::spawn(scheduler_loop(state.clone(), rx));
    tokio::spawn(cleanup_loop(state.clone()));
    // No LKML polling: the UI browses lists on demand via GET /api/lkml/patches
    // (lore's git mirror, behind Anubis bot-protection) and opens a cover letter as a
    // reproducer, or hands it to the scaffold agent.

    // /api/* requires the bearer token (when configured); the embedded UI is
    // served unauthenticated so it can load and prompt for the token.
    let api = Router::new()
        .route("/api/jobs", post(submit).get(list_jobs))
        .route("/api/jobs/:id", get(get_job))
        .route("/api/jobs/:id/summaries", get(get_job_summaries))
        .route("/api/jobs/:id/events", get(events))
        .route("/api/events", get(global_events))
        .route("/api/jobs/:id/metrics", get(get_metrics))
        .route("/api/jobs/:id/phases", get(get_phases))
        .route("/api/jobs/:id/logs/:kind", get(get_log))
        .route("/api/candidates", get(list_candidates))
        .route("/api/candidates/:msgid/run", post(run_candidate))
        .route("/api/lkml/patches", get(list_lkml_patches))
        .route("/api/jobs/:id/refine", post(scaffold::refine))
        .route("/api/scaffold", post(scaffold::start))
        .route("/api/scaffold/refine-text", post(scaffold::refine_text))
        .route("/api/scaffold/models", post(scaffold::models))
        .route("/api/metrics/peaks", get(get_peaks))
        .route("/api/summarizer", get(get_summarizer))
        .route("/api/highlight.css", get(highlight_css))
        .route("/api/highlight/:lang", post(highlight_code))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth));
    let app = api.fallback(embed::static_handler).with_state(state);

    info!("mackernel-server listening on {bind} (work={}, repo={})", work.display(), repo.display());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    // Graceful shutdown on Ctrl-C so the dhat Profiler drops and flushes dhat-heap.json
    // (no-op for behavior without the feature, just a clean exit).
    axum::serve(listener, app)
        .with_graceful_shutdown(async { let _ = tokio::signal::ctrl_c().await; })
        .await?;
    Ok(())
}

// --- HTTP handlers ----------------------------------------------------------

async fn submit(State(st): State<AppState>, body: String) -> Result<Json<serde_json::Value>, StatusCode> {
    if body.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = st.db.create_job(now_ms(), None).map_err(ise)?;
    let dir = st.work.join(id.to_string());
    // Start clean: after a DB wipe the id sequence resets, so this id's dir may hold
    // a prior occupant's stale logs (baseline/patched, console.log, …). Remove it.
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("logs")).map_err(ise)?;
    std::fs::write(dir.join("bundle.md"), body.as_bytes()).map_err(ise)?;
    st.tx.send(SchedMsg::New(id)).map_err(ise)?;
    info!("queued job {id}");
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    Ok(Json(json!({ "id": id })))
}

async fn list_jobs(State(st): State<AppState>) -> Result<Json<Vec<db::Job>>, StatusCode> {
    Ok(Json(st.db.list_jobs().map_err(ise)?))
}

async fn get_job(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<db::Job>, StatusCode> {
    st.db.get_job(id).map_err(ise)?.map(Json).ok_or(StatusCode::NOT_FOUND)
}

/// All per-server summaries for a job (every backend, every field) — backs the UI's
/// "see all models" expander.
async fn get_job_summaries(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<Vec<db::JobSummary>>, StatusCode> {
    Ok(Json(st.db.get_job_summaries(id).map_err(ise)?))
}

async fn list_candidates(State(st): State<AppState>) -> Result<Json<Vec<db::Candidate>>, StatusCode> {
    Ok(Json(st.db.list_candidates().map_err(ise)?))
}

/// Run an LKML candidate: create a job from its stored bundle (which already carries
/// the injected `thread:` key, so run-kernel.py `git am`s the thread's series), and
/// record the lore link + subject as the job's provenance. Mirrors `submit`.
async fn run_candidate(State(st): State<AppState>, Path(msgid): Path<String>)
    -> Result<Json<serde_json::Value>, StatusCode>
{
    let (bundle, source_url, title) = st.db.get_candidate_bundle(&msgid).map_err(ise)?
        .ok_or(StatusCode::NOT_FOUND)?;
    let id = st.db
        .create_job_full(now_ms(), Some("lkml"), Some(&source_url), title.as_deref())
        .map_err(ise)?;
    let dir = st.work.join(id.to_string());
    let _ = std::fs::remove_dir_all(&dir); // clear any stale dir from a recycled id
    std::fs::create_dir_all(dir.join("logs")).map_err(ise)?;
    std::fs::write(dir.join("bundle.md"), bundle.as_bytes()).map_err(ise)?;
    st.tx.send(SchedMsg::New(id)).map_err(ise)?;
    st.db.set_candidate_job(&msgid, id).map_err(ise)?;
    info!("queued job {id} from lkml candidate {msgid}");
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    st.bus.publish_global(json!({ "kind": "candidates" }).to_string());
    Ok(Json(json!({ "id": id })))
}

/// A page of recent patch cover letters on a public-inbox list, for the on-demand LKML
/// browser. `?list=` is a lore path segment, whitelisted to `[a-z0-9._-]`; `?skip=` pages
/// through the list's git mirror. Returns the helper's JSON (`{patches,more,next,epoch}`)
/// verbatim. A fetch failure (lore unreachable / Anubis) is a 502, not a 500.
async fn list_lkml_patches(
    State(st): State<AppState>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<Response, StatusCode> {
    let list = q.get("list").map(String::as_str).unwrap_or("");
    if list.is_empty()
        || !list.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
    {
        return Err(StatusCode::BAD_REQUEST);
    }
    let skip: u32 = q.get("skip").and_then(|s| s.parse().ok()).unwrap_or(0);
    match lkml::list_patches(&st.repo, list, skip).await {
        Ok(json) => Ok(([(CONTENT_TYPE, "application/json")], json).into_response()),
        Err(e) => {
            warn!("lkml: list_patches({list}, skip={skip}) failed: {e:#}");
            Err(StatusCode::BAD_GATEWAY)
        }
    }
}

async fn get_log(
    State(st): State<AppState>,
    Path((id, kind)): Path<(i64, String)>,
    Query(q): Query<HashMap<String, String>>,
) -> Result<String, StatusCode> {
    let jobdir = st.work.join(id.to_string());
    let logs = jobdir.join("logs");
    if kind == "bundle" {
        // Show the reproducer as soon as there is one. For a refine job the agent hasn't
        // written bundle.md yet (or failed to), so fall back to the reproducer it was
        // handed — prev-repro.md — so the reproducer stays visible throughout the refine.
        if let Ok(s) = tokio::fs::read_to_string(jobdir.join("bundle.md")).await {
            return Ok(s);
        }
        return tokio::fs::read_to_string(jobdir.join("prev-repro.md")).await
            .map_err(|_| StatusCode::NOT_FOUND);
    }
    // Compare jobs nest per-variant logs under logs/<variant>/. Whitelist the names so
    // a `?variant=` value can't escape the logs dir.
    let vdir = match q.get("variant").map(String::as_str) {
        Some("baseline") => logs.join("baseline"),
        Some("patched") => logs.join("patched"),
        _ => logs.clone(),
    };
    if kind == "issues" {
        // The bundle's `search-dmesg:`/`regex-dmesg:` patterns are read fresh from the
        // stored bundle.md; compare variants reuse the same (single) bundle.
        let watched = std::fs::read_to_string(jobdir.join("bundle.md"))
            .map(|md| watched_patterns(&md)).unwrap_or_default();
        return Ok(collect_issues(&vdir, &watched));
    }
    let file = match kind.as_str() {
        "fetch" => "fetch.log",
        "compile" => "compile.log",
        "console" => "console.log",
        "dmesg" => "dmesg.log",
        "exec" => "exec.log",
        "run" => "run.log",
        _ => return Err(StatusCode::BAD_REQUEST),
    };
    // run.log is the orchestrator's own output (one per job, top-level), not per-variant.
    let dir = if kind == "run" { &logs } else { &vdir };
    tokio::fs::read_to_string(dir.join(file)).await.map_err(|_| StatusCode::NOT_FOUND)
}

/// Extract kernel BUG/oops/KASAN reports from a dmesg capture. We anchor on
/// `BUG:` only: a KASAN splat is `BUG: KASAN: …`, a NULL-deref oops is
/// `BUG: kernel NULL pointer …`, etc., so `BUG:` catches them without separately
/// grepping `KASAN` (which would just scatter the report's inner lines). Each
/// report is split into a `head` (the description, shown) and a `trace` (the call
/// stack onward, folded in the UI).
fn dmesg_reports(content: &str) -> Vec<serde_json::Value> {
    let lines: Vec<&str> = content.lines().collect();
    let is_delim = |l: &str| l.len() >= 10 && l.trim().chars().all(|c| c == '=');
    let mut blocks = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        if !lines[i].contains("BUG:") {
            i += 1;
            continue;
        }
        // Capture from the BUG line to the report's end: a KASAN `====` delimiter,
        // an oops `---[ end trace … ]---`, or a hard cap.
        let start = i;
        let mut end = lines.len().min(start + 80);
        let mut j = start + 1;
        while j < end {
            if lines[j].contains("---[ end trace") || is_delim(lines[j]) {
                end = j + 1;
                break;
            }
            j += 1;
        }
        let block = &lines[start..end];
        // Fold from the call stack onward (x86 "Call Trace:", arm64 "Call trace:").
        let split = block.iter().position(|l| l.contains("Call Trace") || l.contains("Call trace"));
        let (head, trace): (&[&str], &[&str]) = match split {
            Some(p) => (&block[..p], &block[p..]),
            None => (block, &[]),
        };
        blocks.push(json!({ "head": head, "trace": trace }));
        i = end;
    }
    blocks
}

/// Scan log files for lines that look like a real problem and return them as a JSON
/// array `[{"file": "...", "blocks": [{"head": [...], "trace": [...]}]}]` so the UI
/// can show each source in its own tab and fold call traces. The dmesg log is
/// parsed into BUG reports (head + foldable call trace); other logs become a single
/// block of matching lines.
fn collect_issues(logs: &std::path::Path, watched: &Watched) -> String {
    // Scan the dir's own logs, plus any per-variant subdirs (compare jobs) so the
    // top-level call (used by the end-of-job summary) isn't blind. A `?variant=` call
    // passes logs/<variant> directly, whose subdirs don't exist — so it stays clean.
    let mut sections = scan_issue_dir(logs, "", watched);
    for variant in ["baseline", "patched"] {
        let sub = logs.join(variant);
        if sub.is_dir() {
            sections.extend(scan_issue_dir(&sub, &format!("{variant}/"), watched));
        }
    }
    json!(sections).to_string()
}

/// Bundle-declared extra patterns (`search-dmesg:` literals, `regex-dmesg:` regexes)
/// to surface from console.log like a BUG. Empty when the bundle declares none.
#[derive(Default)]
struct Watched {
    literals: Vec<String>,
    regexes: Vec<regex::Regex>,
}

impl Watched {
    fn is_empty(&self) -> bool { self.literals.is_empty() && self.regexes.is_empty() }
    fn matches(&self, line: &str) -> bool {
        self.literals.iter().any(|s| line.contains(s.as_str()))
            || self.regexes.iter().any(|re| re.is_match(line))
    }
}

/// Parse `search-dmesg:`/`regex-dmesg:` from a bundle's frontmatter — the first
/// `---`…`---` block outside any code fence (mirrors the reproducer spec). Both keys
/// may repeat; each line is one pattern. Invalid regexes are logged and skipped.
fn watched_patterns(bundle_md: &str) -> Watched {
    let mut w = Watched::default();
    let mut in_block = false;
    let mut opened = false;
    let mut fence: Option<usize> = None; // backtick run length of the open fence
    for line in bundle_md.lines() {
        if let Some(n) = fence {
            if line.trim().len() >= n && line.trim().chars().all(|c| c == '`') { fence = None; }
            continue;
        }
        let ticks = line.trim_start().chars().take_while(|&c| c == '`').count();
        if ticks >= 3 { fence = Some(ticks); continue; }
        if line == "---" {
            if !opened { in_block = true; opened = true; continue; }
            if in_block { break; } // closing the first block
        }
        if in_block {
            if let Some(v) = line.strip_prefix("search-dmesg:") {
                let v = v.trim();
                if !v.is_empty() { w.literals.push(v.to_string()); }
            } else if let Some(v) = line.strip_prefix("regex-dmesg:") {
                let v = v.trim();
                match regex::Regex::new(v) {
                    Ok(re) => w.regexes.push(re),
                    Err(e) => tracing::warn!("ignoring invalid regex-dmesg {v:?}: {e}"),
                }
            }
        }
    }
    w
}

/// Scan one log dir's files for problem markers; `prefix` labels the `file` field
/// (e.g. "baseline/") so variant sources stay distinguishable in the merged result.
fn scan_issue_dir(logs: &std::path::Path, prefix: &str, watched: &Watched) -> Vec<serde_json::Value> {
    // General crash markers. `BUG:` already catches sanitizer splats (KASAN/UBSAN/…
    // reports are printed as "BUG: KASAN: …"), so we don't search for sanitizer names
    // separately — that only produced false hits on build logs that mention the
    // -fsanitize flags. "panic" is dropped from the compile log — the kernel source is
    // full of panic()/BUG() calls that are not build problems.
    const GENERAL: &[&str] = &[
        "BUG:", "Oops", "panic", "general protection", "use-after-free",
        "WARNING:", "FATAL", "fatal", "Call Trace", "segfault", "error:", "Error",
    ];
    let mut sections = Vec::new();
    // Bundle-declared watched patterns come first so they're the default Issues tab.
    if !watched.is_empty() {
        if let Ok(content) = std::fs::read_to_string(logs.join("console.log")) {
            let hits: Vec<&str> = content.lines().filter(|l| watched.matches(l)).collect();
            if !hits.is_empty() {
                sections.push(json!({
                    "file": format!("{prefix}console.log (watched)"),
                    "blocks": [json!({ "head": hits, "trace": [] })],
                }));
            }
        }
    }
    for file in ["console.log", "dmesg.log", "exec.log", "compile.log", "fetch.log", "run.log"] {
        let Ok(content) = std::fs::read_to_string(logs.join(file)) else { continue };
        let blocks = if file == "dmesg.log" {
            dmesg_reports(&content)
        } else {
            let is_compile = file == "compile.log";
            let hits: Vec<&str> = content
                .lines()
                .filter(|l| {
                    GENERAL.iter().any(|m| !(is_compile && *m == "panic") && l.contains(m))
                })
                .collect();
            if hits.is_empty() { Vec::new() } else { vec![json!({ "head": hits, "trace": [] })] }
        };
        if !blocks.is_empty() {
            sections.push(json!({ "file": format!("{prefix}{file}"), "blocks": blocks }));
        }
    }
    sections
}

async fn events(
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Live phase/metric/done messages only. Recorded phases are fetched up front
    // via GET /api/jobs/:id/phases, so this stream doesn't replay history.
    let stream = BroadcastStream::new(st.bus.subscribe(id))
        .filter_map(|r| async move { r.ok() })
        .map(|s| Ok(Event::default().data(s)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Process-wide SSE stream of "the job list changed" pings. The client holds one
/// connection and refetches /api/jobs on each ping — so when nothing changes, no
/// requests flow (vs. polling every few seconds).
async fn global_events(State(st): State<AppState>) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = BroadcastStream::new(st.bus.subscribe_global())
        .filter_map(|r| async move { r.ok() })
        .map(|s| Ok(Event::default().data(s)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}

/// Recorded phase timestamps for a job — the same events the SSE stream replays,
/// served as plain JSON so the chart can mark phases on a terminal job (no SSE).
async fn get_phases(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<Vec<serde_json::Value>>, StatusCode> {
    Ok(Json(
        st.db.get_events(id).map_err(ise)?.into_iter()
            .map(|(ts, phase)| json!({ "phase": phase, "ts_ms": ts }))
            .collect(),
    ))
}

async fn get_metrics(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<Vec<db::Sample>>, StatusCode> {
    Ok(Json(st.db.metrics(id).map_err(ise)?))
}

async fn get_peaks(State(st): State<AppState>) -> Result<Json<Vec<db::Peak>>, StatusCode> {
    Ok(Json(st.db.peaks().map_err(ise)?))
}

/// Summarizer status: label and the measured RAM of the llama-server subprocess.
/// `loaded` is false until the background model load finishes.
async fn get_summarizer(State(st): State<AppState>) -> Json<serde_json::Value> {
    match st.summarizer.get() {
        Some(s) => {
            let servers: Vec<_> = s.backends().iter()
                .map(|b| json!({ "label": b.label, "model": b.model, "primary": b.primary }))
                .collect();
            // Topbar shows the primary's label; mem is the local llama-server child.
            let label = s.backends().iter().find(|b| b.primary)
                .map(|b| b.label.clone())
                .unwrap_or_else(|| summarize::LABEL.to_string());
            // Visible opencode queue: who's waiting/running, and how long they've waited.
            // `waited_ms` is computed here (waiters grow; runners freeze at start).
            let now = now_ms();
            let queue: Vec<_> = s.opencode_queue().snapshot().iter().map(|e| json!({
                "job_id": e.job_id,
                "field": e.field,
                "backend": e.backend,
                "running": e.running,
                "waited_ms": e.started_ms.unwrap_or(now) - e.since_ms,
            })).collect();
            Json(json!({
                "loaded": true,
                "label": label,
                "mem_bytes": s.memory_bytes(),
                "servers": servers,
                "queue": queue,
            }))
        }
        None => Json(json!({ "loaded": false, "label": summarize::LABEL, "mem_bytes": 0, "servers": [] })),
    }
}

// --- arborium syntax highlighting (server-side; tree-sitter is Rust-only) -----

/// Theme stylesheet for the highlighted HTML. arborium emits custom-element spans
/// (`<a-k>`, `<a-f>`, …); this maps them to colours under the `.arb` wrapper.
async fn highlight_css() -> Response {
    // Sub-category tags inherit `color` from their parent element by default, so
    // to_css alone is enough — no separate inheritance ruleset needed.
    let css = arborium::theme::builtin::github_dark().to_css(".arb");
    ([(CONTENT_TYPE, "text/css; charset=utf-8")], css).into_response()
}

/// Highlight a code snippet to HTML for `lang` (e.g. `c`, `bash`). Unsupported
/// languages / parse errors -> 422 so the UI falls back to plain text.
async fn highlight_code(Path(lang): Path<String>, body: String) -> Result<Response, StatusCode> {
    let mut hl = arborium::Highlighter::new();
    match hl.highlight(&lang, &body) {
        Ok(html) => Ok(([(CONTENT_TYPE, "text/html; charset=utf-8")], html).into_response()),
        Err(_) => Err(StatusCode::UNPROCESSABLE_ENTITY),
    }
}

// --- auth: bearer token (Authorization header, or ?token= for EventSource) ---

async fn require_auth(State(st): State<AppState>, req: Request, next: Next) -> Result<Response, StatusCode> {
    let Some(expected) = &st.auth_token else {
        return Ok(next.run(req).await); // dev mode: no token configured
    };
    let presented = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|h| h.to_str().ok())
        .and_then(|s| s.strip_prefix("Bearer ").map(str::to_string))
        .or_else(|| {
            req.uri().query().and_then(|q| {
                q.split('&').find_map(|kv| kv.strip_prefix("token=").map(str::to_string))
            })
        });
    if presented.as_deref().is_some_and(|p| token_ok(p, expected)) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

/// Accept the full token, or any prefix of it that is at least 8 chars long. The
/// token is the v7.1 commit hash, so this lets a short git SHA (e.g. `8cd9520d`)
/// authenticate without pasting all 40 hex chars.
// ponytail: an 8-char hex prefix is ~4B guesses, not a real secret; raise the floor
// or drop prefix matching if this ever guards anything sensitive.
fn token_ok(presented: &str, expected: &str) -> bool {
    presented == expected || (presented.len() >= 8 && expected.starts_with(presented))
}

pub(crate) fn ise<E: std::fmt::Display>(e: E) -> StatusCode {
    error!("internal error: {e}");
    StatusCode::INTERNAL_SERVER_ERROR
}

// --- resource-aware scheduler ----------------------------------------------

/// Per-job estimate = max(config default, largest measured peak so far) — the
/// learned bound keeps us from over-admitting once we've seen how heavy jobs are.
fn estimate(st: &AppState) -> (u64, u64) {
    let (mut ram, mut disk) = (st.cfg.est_ram, st.cfg.est_disk);
    if let Ok(peaks) = st.db.peaks() {
        for p in peaks {
            ram = ram.max(p.ram_peak.max(0) as u64);
            disk = disk.max(p.disk_peak.max(0) as u64);
        }
    }
    (ram, disk)
}

async fn scheduler_loop(st: AppState, mut rx: mpsc::UnboundedReceiver<SchedMsg>) {
    let mut queue: VecDeque<i64> = VecDeque::new();
    let mut running: HashMap<i64, (u64, u64)> = HashMap::new(); // id -> (ram_est, disk_est)
    let mut sys = System::new();
    let mut tick = tokio::time::interval(Duration::from_secs(5));

    loop {
        tokio::select! {
            msg = rx.recv() => match msg {
                Some(SchedMsg::New(id)) => queue.push_back(id),
                Some(SchedMsg::Finished(id)) => { running.remove(&id); }
                None => return,
            },
            _ = tick.tick() => {}
        }

        // Admit as many queued jobs as resources allow.
        loop {
            let Some(&id) = queue.front() else { break };
            let (est_ram, est_disk) = estimate(&st);
            let res = sched::read_resources(&mut sys, &st.work);
            let rr: u64 = running.values().map(|(r, _)| *r).sum();
            let dr: u64 = running.values().map(|(_, d)| *d).sum();
            if !sched::can_admit(&res, rr, dr, est_ram, est_disk, running.len(), &st.cfg) {
                break;
            }
            queue.pop_front();
            running.insert(id, (est_ram, est_disk));
            info!("admit job {id} (running={}, est_ram={est_ram}, est_disk={est_disk})", running.len());
            let st2 = st.clone();
            let tx2 = st.tx.clone();
            tokio::spawn(async move {
                if let Err(e) = run_job(&st2, id).await {
                    error!("job {id} failed: {e}");
                    let _ = st2.db.finish(id, now_ms(), "failed", None, 0, 0);
                    st2.bus.publish(id, serde_json::json!({ "kind": "done", "status": "failed" }).to_string());
                    st2.bus.publish_global(serde_json::json!({ "kind": "jobs" }).to_string());
                    st2.bus.close(id);
                }
                let _ = tx2.send(SchedMsg::Finished(id));
            });
        }
    }
}

/// Spawn a background task that computes a job summary and stores + broadcasts it.
/// `stage` is "start" (bundle only, preliminary) or "end" (bundle + run output). The
/// CPU-bound model call runs on a blocking thread, so it never stalls the async runtime,
/// and it never blocks the job pipeline — failures are logged and leave the summary as-is.
/// No-op until the model has finished loading (`OnceLock` empty). Returns the spawned
/// task handles so the caller can await them (the "end" stage closes the SSE channel
/// only once both summaries finish, so their live token stream reaches the client).
fn spawn_summary(st: &AppState, id: i64, stage: &'static str) -> Vec<tokio::task::JoinHandle<()>> {
    let Some(sumz) = st.summarizer.get().cloned() else {
        // Model still warming up: queue this request; the loader flushes it on ready.
        st.summary_queue.lock().unwrap().push((id, stage));
        info!("job {id}: {stage} summary queued (summarizer warming up)");
        return Vec::new();
    };
    let dir = st.work.join(id.to_string());
    let backends = sumz.backends().to_vec();
    let mut handles = Vec::new();
    // Fan out: every field is generated against every backend. The primary backend's
    // output fills the legacy columns + streams live; the rest land in job_summaries.
    if stage == "start" {
        for b in backends {
            // Bundle only: title + reproducer one-liner, generated concurrently.
            let (s, bb) = (sumz.clone(), b.clone());
            handles.push(spawn_one(st, id, "title", b.clone(), dir.clone(),
                move |bundle, _logs, on_tok| s.title(&bb, &bundle, on_tok, id)));
            let (s, bb) = (sumz.clone(), b.clone());
            handles.push(spawn_one(st, id, "repro", b, dir.clone(),
                move |bundle, _logs, on_tok| s.summarize_repro(&bb, &bundle, on_tok, id)));
        }
    } else {
        // Bundle + run output: result one-liner + two-paragraph detail, concurrently.
        let (exit, outcome) = st.db.get_job(id).ok().flatten()
            .map(|j| (j.exit_code, j.status))
            .unwrap_or((None, "done".to_string()));
        for b in backends {
            let (s, bb, o) = (sumz.clone(), b.clone(), outcome.clone());
            handles.push(spawn_one(st, id, "result", b.clone(), dir.clone(),
                move |bundle, logs, on_tok| {
                    let issues = collect_issues(&logs, &watched_patterns(&bundle));
                    s.summarize_result(&bb, &bundle, &issues, exit, &o, on_tok, id)
                }));
            let (s, bb) = (sumz.clone(), b.clone());
            handles.push(spawn_one(st, id, "detail", b, dir.clone(),
                move |bundle, logs, on_tok| s.detail(&bb, &bundle, &logs, on_tok, id)));
        }
    }
    handles
}

/// Run one summary generation for one backend (its own blocking thread, so a stage's
/// outputs across fields × backends run concurrently). Always records the result in
/// `job_summaries` keyed by backend; for the **primary** backend it also fills the
/// legacy per-field column, records meta, and streams live tokens (the default UI
/// path). Non-primary backends fill the legacy column only if the primary hasn't.
/// `gen` receives the bundle text and the logs dir. Failures are logged and leave the
/// field as-is; never blocks the job pipeline.
fn spawn_one<F>(st: &AppState, id: i64, field: &'static str, backend: summarize::Backend,
                dir: std::path::PathBuf, gen: F) -> tokio::task::JoinHandle<()>
where
    F: FnOnce(String, std::path::PathBuf, &dyn Fn(u32)) -> anyhow::Result<(String, summarize::GenStats)>
        + Send
        + 'static,
{
    let (db, bus) = (st.db.clone(), st.bus.clone());
    tokio::spawn(async move {
        let Ok(bundle) = tokio::fs::read_to_string(dir.join("bundle.md")).await else { return };
        let logs = dir.join("logs");
        let is_primary = backend.primary;
        // Only the primary drives the live progress line the default UI shows.
        if is_primary {
            bus.publish(id, json!({ "kind": "summary_progress", "field": field, "tokens": 0 }).to_string());
        }
        let bus_tok = bus.clone();
        let res = tokio::task::spawn_blocking(move || {
            let on_tok = move |n: u32| {
                if is_primary {
                    bus_tok.publish(id, json!({ "kind": "summary_progress", "field": field, "tokens": n }).to_string());
                }
            };
            gen(bundle, logs, &on_tok)
        })
        .await;
        match res {
            Ok(Ok((text, stats))) if !text.is_empty() => {
                // Always record this backend's output in the per-server table.
                if let Err(e) = db.set_job_summary(id, field, &backend.label, &text,
                        stats.ms, stats.tokens, &backend.model, now_ms()) {
                    warn!("job {id}: storing {field} summary for {} failed: {e:#}", backend.label);
                }
                if backend.primary {
                    let stored = match field {
                        "title" => db.set_short_title(id, &text),
                        "repro" => db.set_repro(id, &text),
                        "result" => db.set_result(id, &text),
                        "detail" => db.set_detail(id, &text),
                        _ => Ok(()),
                    };
                    if let Err(e) = stored {
                        warn!("job {id}: storing {field} summary failed: {e:#}");
                        return;
                    }
                    if let Err(e) = db.set_summary_meta(id, field, stats.ms, stats.tokens, &backend.model) {
                        warn!("job {id}: storing {field} summary meta failed: {e:#}");
                    }
                    bus.publish(id, json!({ "kind": "summary", "field": field, "text": text,
                        "ms": stats.ms, "tokens": stats.tokens, "model": backend.model }).to_string());
                    bus.publish_global(json!({ "kind": "jobs" }).to_string());
                } else {
                    // Never-blank fallback: fill the legacy column if the primary hasn't.
                    let _ = db.set_summary_fallback(id, field, &text);
                }
                info!("job {id}: {field} summary ready via {} ({} tok, {} ms)",
                    backend.label, stats.tokens, stats.ms);
            }
            Ok(Ok(_)) => {
                if backend.primary {
                    record_summary_failure(&db, &bus, id, field, "produced no output");
                }
            }
            Ok(Err(e)) => {
                warn!("job {id}: {field} summary via {} failed: {e:#}", backend.label);
                if backend.primary {
                    record_summary_failure(&db, &bus, id, field, &format!("{e:#}"));
                }
            }
            Err(e) => {
                warn!("job {id}: {field} summary via {} task panicked: {e}", backend.label);
                if backend.primary {
                    record_summary_failure(&db, &bus, id, field, &format!("task panicked: {e}"));
                }
            }
        }
    })
}

/// Persist a primary-backend summary failure and push it live over SSE. The error is
/// stored in `summary_meta` (survives reload) so the UI can show "⚠️ failed: <reason>"
/// instead of a perpetual "⏳ pending"; a later successful generation clears it.
fn record_summary_failure(db: &Db, bus: &Bus, id: i64, field: &str, msg: &str) {
    if let Err(e) = db.set_summary_error(id, field, msg) {
        warn!("job {id}: recording {field} summary failure failed: {e:#}");
    }
    bus.publish(id, json!({ "kind": "summary_error", "field": field, "error": msg }).to_string());
    bus.publish_global(json!({ "kind": "jobs" }).to_string());
}

async fn run_job(st: &AppState, id: i64) -> anyhow::Result<()> {
    let dir = st.work.join(id.to_string());
    let logs = dir.join("logs");
    let bundle = dir.join("bundle.md");
    st.db.set_running(id, now_ms())?;
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    info!("job {id}: starting");

    // Scaffold stage (only for scaffold jobs): generate bundle.md with the opencode agent
    // before the normal pipeline. Detected by scaffold.json present + no bundle yet.
    if dir.join("scaffold.json").is_file() && !bundle.is_file() {
        info!("job {id}: scaffold stage");
        scaffold::run_scaffold_stage(st, id, &dir, &logs).await?;
    }

    // Preliminary summary from the bundle alone (runs concurrently with the build).
    spawn_summary(st, id, "start");

    let mut child = tokio::process::Command::new("python3")
        .arg(st.repo.join("run-kernel.py"))
        .arg(&bundle)
        .arg("--log-dir").arg(&logs)
        .arg("--progress")
        .current_dir(&st.repo)
        .env("MK_SANDBOX", "auto")
        .env("MK_WT_ROOT", dir.join("wt"))
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let pid = child.id().unwrap_or(0);

    // Metrics sampler: ~0.5 Hz process-tree RSS + job-dir disk -> DuckDB + SSE,
    // tracking peaks. Stops when the child exits.
    let ram_peak = Arc::new(AtomicU64::new(0));
    let disk_peak = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let sampler = {
        let (db, busc, jdir) = (st.db.clone(), st.bus.clone(), dir.clone());
        let (rp, dp, sp) = (ram_peak.clone(), disk_peak.clone(), stop.clone());
        // Host CPU thermometer (None off Linux / no sensor); resolved once per job.
        let thermo = thermometer::for_host();
        tokio::spawn(async move {
            while !sp.load(Ordering::Relaxed) {
                let rss = metrics::tree_rss(pid).await;
                let disk = metrics::dir_disk(&jdir).await;
                let temp = thermo.read_mc(); // millidegrees C, or None if unavailable
                rp.fetch_max(rss, Ordering::Relaxed);
                dp.fetch_max(disk, Ordering::Relaxed);
                let ts = now_ms();
                let _ = db.add_metric(id, ts, rss as i64, disk as i64, temp);
                busc.publish(id, json!({ "kind": "metric", "ts_ms": ts, "rss": rss, "disk": disk, "temp_mc": temp }).to_string());
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    };

    // Append (not truncate): the scaffold stage may already have written its agent
    // output here, and the run-kernel.py output should follow it in the same log.
    let run_log = std::sync::Arc::new(std::sync::Mutex::new(
        std::fs::OpenOptions::new().create(true).append(true).open(logs.join("run.log"))?,
    ));

    // Drain stderr into run.log.
    if let Some(err) = child.stderr.take() {
        let rl = run_log.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut err = err;
            let _ = err.read_to_end(&mut buf).await;
            use std::io::Write;
            let _ = rl.lock().unwrap().write_all(&buf);
        });
    }

    // Parse stdout lines: MKPROGRESS json -> phase updates; everything -> run.log.
    let mut exit_from_progress: Option<i64> = None;
    if let Some(out) = child.stdout.take() {
        let mut lines = BufReader::new(out).lines();
        while let Some(line) = lines.next_line().await? {
            {
                use std::io::Write;
                let mut f = run_log.lock().unwrap();
                let _ = writeln!(f, "{line}");
            }
            if let Some(rest) = line.strip_prefix("MKPROGRESS ") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) {
                    if let Some(phase) = v.get("phase").and_then(|p| p.as_str()) {
                        let _ = st.db.set_phase(id, phase);
                        let _ = st.db.add_event(id, now_ms(), phase, "");
                        st.bus.publish(id, json!({ "kind": "phase", "phase": phase, "ts_ms": now_ms() }).to_string());
                        st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
                    }
                    if let Some(e) = v.get("exit").and_then(|e| e.as_i64()) {
                        exit_from_progress = Some(e);
                    }
                }
            }
        }
    }

    let status = child.wait().await?;
    stop.store(true, Ordering::Relaxed);
    let _ = sampler.await;
    let ram = ram_peak.load(Ordering::Relaxed) as i64;
    let disk = disk_peak.load(Ordering::Relaxed) as i64;
    // A final "done" progress line means run-kernel.py reached the guest run and
    // reported the guest's exit code (success path). Without it, run-kernel.py
    // died early (die()) -> the job failed; record its process exit code.
    let (outcome, exit) = match exit_from_progress {
        Some(e) => ("done", Some(e)),
        None => ("failed", status.code().map(|c| c as i64)),
    };

    st.db.finish(id, now_ms(), outcome, exit, ram, disk)?;
    st.bus.publish(id, json!({ "kind": "done", "status": outcome, "exit": exit,
                               "ram_peak": ram, "disk_peak": disk }).to_string());
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    // Final summary including the run output (supersedes the preliminary one). The
    // end-stage summaries run after the job is terminal, so keep the per-job SSE
    // channel open until both finish (so their live token stream reaches the client),
    // then signal completion and close it.
    let summary_tasks = spawn_summary(st, id, "end");
    {
        let bus = st.bus.clone();
        tokio::spawn(async move {
            for h in summary_tasks { let _ = h.await; }
            bus.publish(id, json!({ "kind": "summaries_done" }).to_string());
            bus.publish_global(json!({ "kind": "jobs" }).to_string());
            bus.close(id);
        });
    }
    // Reclaim the per-job kernel worktree (the ~3 GB build tree) now that the job is
    // done; logs + metrics + the DuckDB row stay. Each job owns its MK_WT_ROOT, so
    // this never races another job. Absent for no-metadata jobs (built in LINUX_SRC).
    if !st.cfg.keep_worktrees {
        let wt = dir.join("wt");
        if wt.exists() {
            match tokio::fs::remove_dir_all(&wt).await {
                Ok(()) => {
                    info!("job {id}: reclaimed worktree {}", wt.display());
                    let _ = st.db.add_event(id, now_ms(), "reclaimed", "worktree removed");
                    // Drop the now-dangling worktree registration so `.git/worktrees`
                    // doesn't accumulate prunable entries between jobs.
                    let _ = tokio::process::Command::new("git")
                        .arg("-C").arg(&st.cfg.linux_src).arg("worktree").arg("prune")
                        .output().await;
                }
                Err(e) => error!("job {id}: worktree reclaim failed: {e}"),
            }
        }
    }
    // NB: the per-job SSE channel is closed by the end-summary coordinator above, not
    // here, so the result/detail token stream isn't cut off.
    info!("job {id}: {outcome} (exit={:?}, ram_peak={ram}, disk_peak={disk})", exit);
    Ok(())
}

/// Periodic disk retention: delete the whole work/<id> dir (logs included) for jobs
/// finished more than `retention_days` ago; the DuckDB row (status/peaks) is kept and
/// flagged `reaped_ms`. Runs ~30 s after start, then hourly.
async fn cleanup_loop(st: AppState) {
    tokio::time::sleep(Duration::from_secs(30)).await;
    let mut tick = tokio::time::interval(Duration::from_secs(3600));
    loop {
        tick.tick().await;
        sweep(&st).await;
    }
}

async fn sweep(st: &AppState) {
    let cutoff = now_ms() - (st.cfg.retention_days as i64) * 86_400_000;
    let ids = match st.db.reapable(cutoff) {
        Ok(v) => v,
        Err(e) => { error!("cleanup: reapable query failed: {e}"); return; }
    };
    let mut reaped = 0u64;
    for id in ids {
        let d = st.work.join(id.to_string());
        // Path guard: only ever remove a dir that sits directly under the work root.
        if d.parent() != Some(st.work.as_path()) {
            error!("cleanup: refusing to remove out-of-tree path {}", d.display());
            continue;
        }
        if d.exists() {
            if let Err(e) = tokio::fs::remove_dir_all(&d).await {
                error!("cleanup: rm {} failed: {e}", d.display());
                continue;
            }
        }
        let _ = st.db.mark_reaped(id, now_ms());
        reaped += 1;
    }
    if reaped > 0 {
        // Drop git-worktree registrations left behind by reclaimed/reaped worktrees.
        let _ = tokio::process::Command::new("git")
            .arg("-C").arg(&st.cfg.linux_src).arg("worktree").arg("prune")
            .output().await;
        info!("cleanup: reaped {reaped} job dir(s) finished >{} days ago", st.cfg.retention_days);
        st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watched_patterns_extracts_literals_and_regexes() {
        let md = "---\ncommit: v6.12\nsearch-dmesg: MK_SENTINEL_HIT\nsearch-dmesg: other\nregex-dmesg: MK_SENTINEL_\\w+\nregex-dmesg: ([unclosed\n---\n\n# prose\n";
        let w = watched_patterns(md);
        assert_eq!(w.literals, vec!["MK_SENTINEL_HIT", "other"]);
        assert_eq!(w.regexes.len(), 1, "the unclosed regex must be skipped");
        assert!(w.matches("[ 1.2] MK_SENTINEL_HIT now"));
        assert!(w.matches("MK_SENTINEL_42"), "regex matches");
        assert!(!w.matches("nothing here"));
    }

    #[test]
    fn watched_patterns_ignores_keys_inside_code_fences() {
        // A `search-dmesg:` line inside a fenced block (or after the block) is not meta.
        let md = "---\nsearch-dmesg: real\n---\n\n```init:init.sh\nsearch-dmesg: notmeta\n```\n";
        let w = watched_patterns(md);
        assert_eq!(w.literals, vec!["real"]);
    }

    #[test]
    fn watched_section_is_first_and_only_on_match() {
        let dir = std::env::temp_dir().join(format!("mk-watch-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("console.log"), "boot ok\nMK_SENTINEL_HIT fired\nmore\n").unwrap();
        let w = watched_patterns("---\nsearch-dmesg: MK_SENTINEL_HIT\n---\n");
        let out = collect_issues(&dir, &w);
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        let first = &v[0];
        assert_eq!(first["file"], "console.log (watched)", "watched section is first: {out}");
        assert_eq!(first["blocks"][0]["head"][0], "MK_SENTINEL_HIT fired");

        // No patterns -> no watched section (unchanged behavior).
        let none = collect_issues(&dir, &Watched::default());
        let nv: serde_json::Value = serde_json::from_str(&none).unwrap();
        assert!(nv.as_array().unwrap().iter().all(|s| s["file"] != "console.log (watched)"), "{none}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn token_ok_accepts_full_and_six_char_prefix() {
        let real = "8cd9520d35a6c38db6567e97dd93b1f11f185dc6";
        assert!(token_ok(real, real), "exact match");
        assert!(token_ok("8cd9520d3", real), "9-char prefix");
        assert!(token_ok("8cd9520d", real), "exactly 8 chars");
        assert!(!token_ok("8cd9520", real), "7 chars is too short");
        assert!(!token_ok("deadbeef", real), "non-prefix is rejected");
        assert!(!token_ok("", real), "empty is rejected");
    }
}
