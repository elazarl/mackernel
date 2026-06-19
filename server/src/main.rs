//! mackernel-server: REST service that queues reproducer bundles, runs them via
//! run-kernel.py, and exposes status + logs. (Phase 1: REST + serial worker.)
mod bus;
mod db;
mod embed;
mod metrics;
mod sched;

use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use sysinfo::System;

use axum::{
    extract::{Path, Request, State},
    http::{header::AUTHORIZATION, StatusCode},
    middleware::Next,
    response::sse::{Event, KeepAlive, Sse},
    response::Response,
    routing::{get, post},
    Json, Router,
};
use futures::stream::{Stream, StreamExt};
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;
use tokio_stream::wrappers::BroadcastStream;
use tracing::{error, info};

use bus::Bus;
use db::Db;
use sched::{Cfg, SchedMsg};

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
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as i64
}

fn env_path(key: &str, default: &str) -> PathBuf {
    PathBuf::from(std::env::var(key).unwrap_or_else(|_| default.to_string()))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| "info".into()))
        .init();

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

    let (tx, rx) = mpsc::unbounded_channel::<SchedMsg>();
    let state = AppState {
        db: database, work: work.clone(), repo: repo.clone(), tx,
        bus: Bus::default(), cfg: Cfg::from_env(), auth_token,
    };

    tokio::spawn(scheduler_loop(state.clone(), rx));
    tokio::spawn(cleanup_loop(state.clone()));

    // /api/* requires the bearer token (when configured); the embedded UI is
    // served unauthenticated so it can load and prompt for the token.
    let api = Router::new()
        .route("/api/jobs", post(submit).get(list_jobs))
        .route("/api/jobs/:id", get(get_job))
        .route("/api/jobs/:id/events", get(events))
        .route("/api/jobs/:id/metrics", get(get_metrics))
        .route("/api/jobs/:id/logs/:kind", get(get_log))
        .route("/api/metrics/peaks", get(get_peaks))
        .route_layer(axum::middleware::from_fn_with_state(state.clone(), require_auth));
    let app = api.fallback(embed::static_handler).with_state(state);

    info!("mackernel-server listening on {bind} (work={}, repo={})", work.display(), repo.display());
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// --- HTTP handlers ----------------------------------------------------------

async fn submit(State(st): State<AppState>, body: String) -> Result<Json<serde_json::Value>, StatusCode> {
    if body.trim().is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let id = st.db.create_job(now_ms(), None).map_err(ise)?;
    let dir = st.work.join(id.to_string());
    std::fs::create_dir_all(dir.join("logs")).map_err(ise)?;
    std::fs::write(dir.join("bundle.md"), body.as_bytes()).map_err(ise)?;
    st.tx.send(SchedMsg::New(id)).map_err(ise)?;
    info!("queued job {id}");
    Ok(Json(json!({ "id": id })))
}

async fn list_jobs(State(st): State<AppState>) -> Result<Json<Vec<db::Job>>, StatusCode> {
    Ok(Json(st.db.list_jobs().map_err(ise)?))
}

async fn get_job(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<db::Job>, StatusCode> {
    st.db.get_job(id).map_err(ise)?.map(Json).ok_or(StatusCode::NOT_FOUND)
}

async fn get_log(State(st): State<AppState>, Path((id, kind)): Path<(i64, String)>) -> Result<String, StatusCode> {
    let jobdir = st.work.join(id.to_string());
    let logs = jobdir.join("logs");
    if kind == "bundle" {
        return tokio::fs::read_to_string(jobdir.join("bundle.md")).await
            .map_err(|_| StatusCode::NOT_FOUND);
    }
    if kind == "issues" {
        return Ok(collect_issues(&logs));
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
    tokio::fs::read_to_string(logs.join(file)).await.map_err(|_| StatusCode::NOT_FOUND)
}

/// Grep every log file for lines that look like a real problem — crashes, fatal
/// errors, and sanitizer splats (KASAN/UBSAN/KCSAN/KFENCE) — and return them
/// grouped by source file. This replaces the old KASAN-only special case with a
/// generic "anything interesting went wrong" view.
fn collect_issues(logs: &std::path::Path) -> String {
    // General markers apply to every log. Sanitizer markers only apply to the
    // runtime logs: the build/fetch logs mention KASAN/sanitizer as compile
    // flags (e.g. -fsanitize=kernel-address), which are not problems.
    const GENERAL: &[&str] = &[
        "BUG:", "Oops", "panic", "general protection", "use-after-free",
        "WARNING:", "FATAL", "fatal", "Call Trace", "segfault", "error:", "Error",
    ];
    const SANITIZER: &[&str] = &["KASAN", "UBSAN", "KCSAN", "KFENCE", "KMSAN", "sanitizer"];
    let mut out = String::new();
    for file in ["console.log", "dmesg.log", "exec.log", "compile.log", "fetch.log", "run.log"] {
        let Ok(content) = std::fs::read_to_string(logs.join(file)) else { continue };
        let runtime = matches!(file, "console.log" | "dmesg.log" | "exec.log");
        let hits: Vec<&str> = content
            .lines()
            .filter(|l| GENERAL.iter().any(|m| l.contains(m))
                || (runtime && SANITIZER.iter().any(|m| l.contains(m))))
            .collect();
        if !hits.is_empty() {
            out.push_str(&format!("===== {file} ({} line(s)) =====\n", hits.len()));
            for l in hits {
                out.push_str(l);
                out.push('\n');
            }
            out.push('\n');
        }
    }
    if out.is_empty() {
        "no error / fatal / panic / sanitizer markers found in any log".to_string()
    } else {
        out
    }
}

async fn events(
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    // Subscribe first so live events during replay aren't lost, then replay the
    // recorded phase events, then stream live phase/metric/done messages.
    let live = BroadcastStream::new(st.bus.subscribe(id))
        .filter_map(|r| async move { r.ok() })
        .map(|s| Ok(Event::default().data(s)));
    let past: Vec<Result<Event, Infallible>> = st
        .db
        .get_events(id)
        .unwrap_or_default()
        .into_iter()
        .map(|(ts, phase)| {
            Ok(Event::default()
                .data(json!({ "kind": "phase", "phase": phase, "ts_ms": ts }).to_string()))
        })
        .collect();
    let stream = futures::stream::iter(past).chain(live);
    Sse::new(stream).keep_alive(KeepAlive::default())
}

async fn get_metrics(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<Vec<db::Sample>>, StatusCode> {
    Ok(Json(st.db.metrics(id).map_err(ise)?))
}

async fn get_peaks(State(st): State<AppState>) -> Result<Json<Vec<db::Peak>>, StatusCode> {
    Ok(Json(st.db.peaks().map_err(ise)?))
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
    if presented.as_deref() == Some(expected.as_str()) {
        Ok(next.run(req).await)
    } else {
        Err(StatusCode::UNAUTHORIZED)
    }
}

fn ise<E: std::fmt::Display>(e: E) -> StatusCode {
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
                    st2.bus.close(id);
                }
                let _ = tx2.send(SchedMsg::Finished(id));
            });
        }
    }
}

async fn run_job(st: &AppState, id: i64) -> anyhow::Result<()> {
    let dir = st.work.join(id.to_string());
    let logs = dir.join("logs");
    let bundle = dir.join("bundle.md");
    st.db.set_running(id, now_ms())?;
    info!("job {id}: starting");

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
        tokio::spawn(async move {
            while !sp.load(Ordering::Relaxed) {
                let rss = metrics::tree_rss(pid).await;
                let disk = metrics::dir_disk(&jdir).await;
                rp.fetch_max(rss, Ordering::Relaxed);
                dp.fetch_max(disk, Ordering::Relaxed);
                let ts = now_ms();
                let _ = db.add_metric(id, ts, rss as i64, disk as i64);
                busc.publish(id, json!({ "kind": "metric", "ts_ms": ts, "rss": rss, "disk": disk }).to_string());
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        })
    };

    let run_log = std::sync::Arc::new(std::sync::Mutex::new(
        std::fs::File::create(logs.join("run.log"))?,
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
    st.bus.close(id);
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
    }
}
