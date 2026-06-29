//! "Scaffold a reproducer": run the opencode agent (scaffold-repro.py) in a
//! container to write a reproducer bundle from a patch series, then hand the bundle
//! back to the normal submit/run pipeline.
//!
//! Ephemeral by design: scaffolding is an interactive generate step, not a tracked
//! job, so state lives in memory (a counter + a map), lost on restart. Progress
//! streams over a dedicated `Bus`; the generated bundle and the merged agent log are
//! fetched by id. Runs are serialized (`lock`) — one heavyweight opencode CLI agent
//! at a time, matching the free zen tier's no-concurrency rule (see summarize.rs).

use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};

use axum::{
    extract::{Path, State},
    http::StatusCode,
    response::sse::{Event, KeepAlive, Sse},
    Json,
};
use futures::stream::{Stream, StreamExt};
use serde::Deserialize;
use serde_json::json;
use std::convert::Infallible;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tracing::{error, info};

use crate::bus::Bus;
use crate::{now_ms, AppState};

/// One scaffold run's terminal-or-in-flight state.
#[derive(Clone)]
pub struct Scaffold {
    pub status: String, // "running" | "done" | "failed"
    pub bundle: Option<String>,
    pub error: Option<String>,
}

/// In-memory store for scaffold runs. Its `bus` is separate from the job bus so the
/// scaffold id space never collides with job ids.
pub struct Store {
    next_id: AtomicI64,
    map: Mutex<HashMap<i64, Scaffold>>,
    pub bus: Bus,
    /// Serializes agent runs (one opencode CLI at a time).
    lock: tokio::sync::Mutex<()>,
}

impl Default for Store {
    fn default() -> Self {
        Store {
            next_id: AtomicI64::new(1),
            map: Mutex::new(HashMap::new()),
            bus: Bus::default(),
            lock: tokio::sync::Mutex::new(()),
        }
    }
}

impl Store {
    fn get(&self, id: i64) -> Option<Scaffold> {
        self.map.lock().unwrap().get(&id).cloned()
    }
    fn set(&self, id: i64, s: Scaffold) {
        self.map.lock().unwrap().insert(id, s);
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScaffoldReq {
    /// A lore.kernel.org thread URL whose [PATCH] series the agent reproduces.
    thread: Option<String>,
    /// An inline unified diff (alternative to `thread`).
    patch: Option<String>,
    /// Base commit/tag to explore (optional; defaults to the server's kernel HEAD).
    commit: Option<String>,
    /// The user's OpenAI-compatible endpoint + key + model (no free tier). All three are
    /// required; the agent runs opencode against this provider (see scaffold-repro.py).
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
}

/// POST /api/scaffold — start a scaffold run; returns its id immediately. The agent
/// runs in the background and streams progress over GET /api/scaffold/:id/events.
pub async fn start(
    State(st): State<AppState>,
    Json(req): Json<ScaffoldReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let thread = req.thread.filter(|s| !s.trim().is_empty());
    let patch = req.patch.filter(|s| !s.trim().is_empty());
    if thread.is_none() && patch.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // No free tier: scaffolding requires the user's own OpenAI-compatible creds. Reject
    // unless all three are present (the UI gates the buttons on the same condition).
    let nonblank = |o: Option<String>| o.filter(|s| !s.trim().is_empty());
    let (base_url, api_key, model) = match (nonblank(req.base_url), nonblank(req.api_key), nonblank(req.model)) {
        (Some(b), Some(k), Some(m)) => (b, k, m),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    let store = st.scaffold.clone();
    let id = store.next_id.fetch_add(1, Ordering::Relaxed);
    store.set(id, Scaffold { status: "running".into(), bundle: None, error: None });

    let dir = st.work.join("scaffold").join(id.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).map_err(crate::ise)?;
    // Inline patch goes to a file the script reads with --patch-file.
    let patch_file = if let Some(p) = &patch {
        let pf = dir.join("input.patch");
        std::fs::write(&pf, p).map_err(crate::ise)?;
        Some(pf)
    } else {
        None
    };

    info!("scaffold {id}: queued");
    tokio::spawn(run(st.clone(), id, dir, thread, patch_file, req.commit, base_url, api_key, model));
    Ok(Json(json!({ "id": id })))
}

/// Drive scaffold-repro.py: stream its stdout (MKPROGRESS -> phase events; all lines
/// -> scaffold.log), then record the produced bundle (or the failure).
async fn run(
    st: AppState,
    id: i64,
    dir: std::path::PathBuf,
    thread: Option<String>,
    patch_file: Option<std::path::PathBuf>,
    commit: Option<String>,
    base_url: String,
    api_key: String,
    model: String,
) {
    let store = st.scaffold.clone();
    let _guard = store.lock.lock().await; // one agent at a time

    let out = dir.join("repro.md");
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(st.repo.join("scaffold-repro.py"));
    if let Some(t) = &thread {
        cmd.arg("--thread").arg(t);
    }
    if let Some(pf) = &patch_file {
        cmd.arg("--patch-file").arg(pf);
    }
    if let Some(c) = commit.as_deref().filter(|s| !s.trim().is_empty()) {
        cmd.arg("--commit").arg(c);
    }
    cmd.arg("--out").arg(&out)
        .arg("--log-dir").arg(&dir)
        .arg("--progress")
        .current_dir(&st.repo)
        .env("MK_SANDBOX", "auto")
        .env("MK_WT_ROOT", dir.join("wt"))
        // Creds ride as env, not argv, so the API key never appears in `ps`.
        .env("MK_OPENAI_BASE_URL", &base_url)
        .env("MK_OPENAI_API_KEY", &api_key)
        .env("MK_OPENCODE_MODEL", &model)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());

    let res = drive(&store, id, &dir, cmd).await;
    match res {
        Ok(()) => match tokio::fs::read_to_string(&out).await {
            Ok(b) if !b.trim().is_empty() => {
                store.set(id, Scaffold { status: "done".into(), bundle: Some(b), error: None });
                store.bus.publish(id, json!({ "kind": "done", "status": "done" }).to_string());
                info!("scaffold {id}: done");
            }
            _ => fail(&store, id, "agent produced no bundle"),
        },
        Err(e) => fail(&store, id, &e),
    }
    store.bus.close(id);
}

fn fail(store: &Store, id: i64, msg: &str) {
    error!("scaffold {id}: {msg}");
    store.set(id, Scaffold { status: "failed".into(), bundle: None, error: Some(msg.into()) });
    store.bus.publish(id, json!({ "kind": "done", "status": "failed", "error": msg }).to_string());
}

/// Spawn the child, tee stdout to scaffold.log + emit phase events, drain stderr to
/// the same log. Returns Err(message) if the process fails to spawn or exits nonzero.
async fn drive(
    store: &Store,
    id: i64,
    dir: &std::path::Path,
    mut cmd: tokio::process::Command,
) -> Result<(), String> {
    let mut child = cmd.spawn().map_err(|e| format!("spawn scaffold-repro.py: {e}"))?;
    let log = Arc::new(std::sync::Mutex::new(
        std::fs::File::create(dir.join("scaffold.log")).map_err(|e| e.to_string())?,
    ));

    if let Some(err) = child.stderr.take() {
        let log = log.clone();
        tokio::spawn(async move {
            let mut buf = Vec::new();
            let mut err = err;
            let _ = err.read_to_end(&mut buf).await;
            use std::io::Write;
            let _ = log.lock().unwrap().write_all(&buf);
        });
    }

    if let Some(o) = child.stdout.take() {
        let mut lines = BufReader::new(o).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            {
                use std::io::Write;
                let _ = writeln!(log.lock().unwrap(), "{line}");
            }
            if let Some(rest) = line.strip_prefix("MKPROGRESS ") {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(rest) {
                    if let Some(p) = v.get("phase").and_then(|p| p.as_str()) {
                        store.bus.publish(id, json!({ "kind": "phase", "phase": p, "ts_ms": now_ms() }).to_string());
                    }
                }
            } else {
                // A log line landed; ping so the UI refetches scaffold.log.
                store.bus.publish(id, json!({ "kind": "log" }).to_string());
            }
        }
    }

    let status = child.wait().await.map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("scaffold-repro.py exited with {status}"))
    }
}

/// GET /api/scaffold/:id — current status (+ error if failed).
pub async fn get(State(st): State<AppState>, Path(id): Path<i64>) -> Result<Json<serde_json::Value>, StatusCode> {
    let s = st.scaffold.get(id).ok_or(StatusCode::NOT_FOUND)?;
    Ok(Json(json!({ "id": id, "status": s.status, "error": s.error })))
}

/// GET /api/scaffold/:id/bundle — the generated bundle (404 until done).
pub async fn bundle(State(st): State<AppState>, Path(id): Path<i64>) -> Result<String, StatusCode> {
    st.scaffold.get(id).and_then(|s| s.bundle).ok_or(StatusCode::NOT_FOUND)
}

/// GET /api/scaffold/:id/log — the merged agent log so far.
pub async fn log(State(st): State<AppState>, Path(id): Path<i64>) -> Result<String, StatusCode> {
    let p = st.work.join("scaffold").join(id.to_string()).join("scaffold.log");
    tokio::fs::read_to_string(p).await.map_err(|_| StatusCode::NOT_FOUND)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ModelsReq {
    base_url: String,
    api_key: String,
}

/// POST /api/scaffold/models — proxy the provider's `GET {baseUrl}/models` with the
/// user's key and return the model ids. The browser can't call the provider directly
/// (CORS), and proxying keeps the key off the page. Runs from the trusted server host,
/// not through the opencode container's egress proxy.
pub async fn models(Json(req): Json<ModelsReq>) -> Result<Json<Vec<String>>, StatusCode> {
    let base = req.base_url.trim().trim_end_matches('/').to_string();
    let key = req.api_key.trim().to_string();
    if base.is_empty() || key.is_empty() {
        return Err(StatusCode::BAD_REQUEST);
    }
    let ids = tokio::task::spawn_blocking(move || -> Result<Vec<String>, ()> {
        let client = reqwest::blocking::Client::builder()
            .timeout(std::time::Duration::from_secs(20))
            .build()
            .map_err(|_| ())?;
        let resp = client
            .get(format!("{base}/models"))
            .bearer_auth(&key)
            .send()
            .map_err(|_| ())?
            .error_for_status()
            .map_err(|_| ())?;
        let v: serde_json::Value = resp.json().map_err(|_| ())?;
        // OpenAI shape: { "data": [ { "id": "..." }, ... ] }.
        Ok(v.get("data")
            .and_then(|d| d.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                    .collect()
            })
            .unwrap_or_default())
    })
    .await
    .map_err(crate::ise)?
    .map_err(|_| StatusCode::BAD_GATEWAY)?;
    Ok(Json(ids))
}

/// GET /api/scaffold/:id/events — SSE phase/log/done stream (mirrors job events).
pub async fn events(
    State(st): State<AppState>,
    Path(id): Path<i64>,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let stream = tokio_stream::wrappers::BroadcastStream::new(st.scaffold.bus.subscribe(id))
        .filter_map(|r| async move { r.ok() })
        .map(|s| Ok(Event::default().data(s)));
    Sse::new(stream).keep_alive(KeepAlive::default())
}
