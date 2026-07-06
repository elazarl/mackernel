//! "Scaffold a reproducer": the opencode agent (scaffold-repro.py) writes a reproducer
//! bundle from a patch series. This is the FIRST stage of a normal tracked job — clicking
//! Scaffold creates a job whose `scaffold` phase generates `bundle.md`, after which the
//! usual run-kernel.py pipeline runs it (see run_job in main.rs). There is no separate
//! ephemeral run anymore; progress, logs, and metrics come from the normal job machinery.

use std::process::Stdio;

use axum::{extract::{Path, State}, http::StatusCode, Json};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tracing::info;

use crate::{now_ms, AppState};

/// The user's OpenAI-compatible creds for a scaffold job. Kept in an in-memory map in
/// AppState (never on disk) from job creation until the scaffold stage consumes them; a
/// server restart drops them, so a still-queued scaffold job fails with a clear message.
#[derive(Clone)]
pub struct Creds {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
}

/// The non-secret scaffold inputs, persisted as `<job>/scaffold.json` so run_job knows the
/// job needs scaffolding (and what from) even across a restart.
#[derive(Serialize, Deserialize)]
pub struct Spec {
    pub thread: Option<String>,
    pub patch_file: Option<String>,
    pub commit: Option<String>,
    /// A "refine" job derived from a parent job (its reproducer + run logs are copied in as
    /// prev-repro.md / prev-logs/). Informational/provenance; the scaffold stage detects
    /// refine by prev-repro.md being present (text-refine has no parent). `#[serde(default)]`
    /// so old scaffold.json files still deserialize.
    #[serde(default)]
    pub refine_parent: Option<i64>,
    /// Optional free-text prompt/context the user added, woven into the agent prompt (both
    /// fresh scaffold and refine).
    #[serde(default)]
    pub note: Option<String>,
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
    /// Optional free-text prompt/context woven into the agent's scaffold prompt.
    note: Option<String>,
}

/// POST /api/scaffold — create a tracked job whose first stage scaffolds a bundle. Returns
/// the job id immediately (mirrors `submit`/`run_candidate`); the UI opens it like any job.
pub async fn start(
    State(st): State<AppState>,
    Json(req): Json<ScaffoldReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let thread = req.thread.filter(|s| !s.trim().is_empty());
    let patch = req.patch.filter(|s| !s.trim().is_empty());
    let note = req.note.filter(|s| !s.trim().is_empty());
    // Prompt-only scaffold: no thread/patch is fine as long as the user described what
    // to reproduce in the note (scaffold-repro.py builds a from-scratch prompt).
    if thread.is_none() && patch.is_none() && note.is_none() {
        return Err(StatusCode::BAD_REQUEST);
    }
    // No free tier: scaffolding requires the user's own OpenAI-compatible creds. Reject
    // unless all three are present (the UI gates the buttons on the same condition).
    let nonblank = |o: Option<String>| o.filter(|s| !s.trim().is_empty());
    let (base_url, api_key, model) = match (nonblank(req.base_url), nonblank(req.api_key), nonblank(req.model)) {
        (Some(b), Some(k), Some(m)) => (b, k, m),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    let commit = req.commit.filter(|s| !s.trim().is_empty());
    let id = st.db
        .create_job_full(now_ms(), Some("scaffold"), thread.as_deref(), None)
        .map_err(crate::ise)?;
    let dir = st.work.join(id.to_string());
    let _ = std::fs::remove_dir_all(&dir); // clear any stale dir from a recycled id
    std::fs::create_dir_all(dir.join("logs")).map_err(crate::ise)?;
    // Inline patch goes to a file the scaffold stage reads with --patch-file.
    let patch_file = if let Some(p) = &patch {
        let pf = dir.join("input.patch");
        std::fs::write(&pf, p).map_err(crate::ise)?;
        Some(pf.to_string_lossy().into_owned())
    } else {
        None
    };
    // Persist the non-secret spec; keep the creds in memory only.
    let spec = Spec { thread, patch_file, commit, refine_parent: None, note };
    std::fs::write(dir.join("scaffold.json"), serde_json::to_vec(&spec).map_err(crate::ise)?)
        .map_err(crate::ise)?;
    st.scaffold_creds.lock().unwrap().insert(id, Creds { base_url, api_key, model });

    st.tx.send(crate::SchedMsg::New(id)).map_err(crate::ise)?;
    info!("queued scaffold job {id}");
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefineReq {
    /// The user's OpenAI-compatible endpoint + key + model (no free tier), same as scaffold.
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
    /// Optional free-text context to weave into the agent's fix-it prompt.
    note: Option<String>,
}

/// Recursively copy a directory tree (used to bring a parent job's logs into a refine
/// job's dir, and to seed the demo job's work dir). Handles the compare-job
/// `baseline/`+`patched/` subdirs.
pub(crate) fn copy_dir_all(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    std::fs::create_dir_all(dst)?;
    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let to = dst.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir_all(&entry.path(), &to)?;
        } else {
            std::fs::copy(entry.path(), to)?;
        }
    }
    Ok(())
}

/// POST /api/jobs/:id/refine — create a new scaffold job that hands the parent job's
/// reproducer + all of its run logs back to the opencode agent ("this failed, fix it").
/// Mirrors `start`, but seeds the bundle/logs from the parent instead of a patch series.
pub async fn refine(
    State(st): State<AppState>,
    Path(parent_id): Path<i64>,
    Json(req): Json<RefineReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    // Same cred gate as `start` (no free tier).
    let nonblank = |o: Option<String>| o.filter(|s| !s.trim().is_empty());
    let (base_url, api_key, model) = match (nonblank(req.base_url), nonblank(req.api_key), nonblank(req.model)) {
        (Some(b), Some(k), Some(m)) => (b, k, m),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    // The parent must still have its reproducer and logs on disk (not reaped).
    let parent = st.db.get_job(parent_id).map_err(crate::ise)?.ok_or(StatusCode::NOT_FOUND)?;
    let pdir = st.work.join(parent_id.to_string());
    let pbundle = pdir.join("bundle.md");
    let plogs = pdir.join("logs");
    if !pbundle.is_file() || !plogs.is_dir() {
        return Err(StatusCode::BAD_REQUEST); // nothing to refine (no bundle / logs reaped)
    }

    // New child job; carry the parent's provenance (lore link + title) forward.
    let id = st.db
        .create_job_full(now_ms(), Some("scaffold"), parent.source_url.as_deref(), parent.title.as_deref())
        .map_err(crate::ise)?;
    let dir = st.work.join(id.to_string());
    let _ = std::fs::remove_dir_all(&dir); // clear any stale dir from a recycled id
    std::fs::create_dir_all(dir.join("logs")).map_err(crate::ise)?;

    // Seed the agent's inputs: the prior reproducer and the full log tree.
    std::fs::copy(&pbundle, dir.join("prev-repro.md")).map_err(crate::ise)?;
    copy_dir_all(&plogs, &dir.join("prev-logs")).map_err(crate::ise)?;

    // Persist the spec (refine). Carry the parent's thread (source_url) so the agent can
    // re-anchor the worktree/patch context; the embedded patch in prev-repro.md is primary.
    let spec = Spec {
        thread: parent.source_url.clone(),
        patch_file: None,
        commit: None,
        refine_parent: Some(parent_id),
        note: req.note.filter(|s| !s.trim().is_empty()),
    };
    std::fs::write(dir.join("scaffold.json"), serde_json::to_vec(&spec).map_err(crate::ise)?)
        .map_err(crate::ise)?;
    st.scaffold_creds.lock().unwrap().insert(id, Creds { base_url, api_key, model });

    st.tx.send(crate::SchedMsg::New(id)).map_err(crate::ise)?;
    info!("queued refine job {id} (from job {parent_id})");
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    Ok(Json(json!({ "id": id })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RefineTextReq {
    /// The reproducer bundle text to improve (from the editor).
    bundle: Option<String>,
    /// Optional free-text prompt guiding the improvement.
    note: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    model: Option<String>,
}

/// POST /api/scaffold/refine-text — create a scaffold job that hands the agent an existing
/// reproducer bundle (the editor's current text) plus an optional prompt and asks it to
/// improve it. Like `refine` but with no parent job and no run logs.
pub async fn refine_text(
    State(st): State<AppState>,
    Json(req): Json<RefineTextReq>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let bundle = req.bundle.filter(|s| !s.trim().is_empty()).ok_or(StatusCode::BAD_REQUEST)?;
    let nonblank = |o: Option<String>| o.filter(|s| !s.trim().is_empty());
    let (base_url, api_key, model) = match (nonblank(req.base_url), nonblank(req.api_key), nonblank(req.model)) {
        (Some(b), Some(k), Some(m)) => (b, k, m),
        _ => return Err(StatusCode::BAD_REQUEST),
    };

    let id = st.db
        .create_job_full(now_ms(), Some("scaffold"), None, None)
        .map_err(crate::ise)?;
    let dir = st.work.join(id.to_string());
    let _ = std::fs::remove_dir_all(&dir); // clear any stale dir from a recycled id
    std::fs::create_dir_all(dir.join("logs")).map_err(crate::ise)?;

    // The bundle to improve is the agent's prev-repro.md; no prev-logs (it hasn't run).
    std::fs::write(dir.join("prev-repro.md"), &bundle).map_err(crate::ise)?;
    let spec = Spec {
        thread: None,
        patch_file: None,
        commit: None,
        refine_parent: None,
        note: nonblank(req.note),
    };
    std::fs::write(dir.join("scaffold.json"), serde_json::to_vec(&spec).map_err(crate::ise)?)
        .map_err(crate::ise)?;
    st.scaffold_creds.lock().unwrap().insert(id, Creds { base_url, api_key, model });

    st.tx.send(crate::SchedMsg::New(id)).map_err(crate::ise)?;
    info!("queued refine-text job {id}");
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());
    Ok(Json(json!({ "id": id })))
}

/// The scaffold stage: run scaffold-repro.py to generate `<dir>/bundle.md`. Called by
/// run_job before the run-kernel.py pipeline when `<dir>/scaffold.json` is present and no
/// bundle exists yet. Output (stdout+stderr) is appended to the job's run.log.
pub async fn run_scaffold_stage(
    st: &AppState,
    id: i64,
    dir: &std::path::Path,
    logs: &std::path::Path,
) -> anyhow::Result<()> {
    st.db.set_phase(id, "scaffold")?;
    st.db.add_event(id, now_ms(), "scaffold", "")?;
    st.bus.publish(id, json!({ "kind": "phase", "phase": "scaffold", "ts_ms": now_ms() }).to_string());
    st.bus.publish_global(json!({ "kind": "jobs" }).to_string());

    let spec: Spec = serde_json::from_slice(&std::fs::read(dir.join("scaffold.json"))?)?;
    let creds = st.scaffold_creds.lock().unwrap().remove(&id)
        .ok_or_else(|| anyhow::anyhow!("scaffold credentials lost (server restart?) — resubmit"))?;

    let bundle = dir.join("bundle.md");
    let mut cmd = tokio::process::Command::new("python3");
    cmd.arg(st.repo.join("scaffold-repro.py"));
    if let Some(t) = &spec.thread {
        cmd.arg("--thread").arg(t);
    }
    if let Some(pf) = &spec.patch_file {
        cmd.arg("--patch-file").arg(pf);
    }
    if let Some(c) = &spec.commit {
        cmd.arg("--commit").arg(c);
    }
    // Refine: a prior reproducer was seeded as prev-repro.md (by `refine` from a parent job,
    // or by `refine_text` from the editor). Point the agent at it; include prev-logs/ only
    // when present (text-refine has none) so the agent fixes/improves rather than writes new.
    let prev_repro = dir.join("prev-repro.md");
    if prev_repro.is_file() {
        cmd.arg("--refine").arg("--prev-repro").arg(&prev_repro);
        let prev_logs = dir.join("prev-logs");
        if prev_logs.is_dir() {
            cmd.arg("--prev-logs").arg(&prev_logs);
        }
    }
    // Optional user prompt/context (fresh scaffold and refine alike).
    if let Some(note) = &spec.note {
        cmd.arg("--note").arg(note);
    }
    // No --progress: run_job owns the single "scaffold" phase. The agent's output is
    // streamed line-by-line: appended to logs/scaffold.log (its own file — run.log stays
    // the run-kernel orchestrator log) and published on the job's SSE bus as
    // `scaffold_log` events so the UI shows it live. stdout and stderr are both piped
    // (scaffold-repro.py merges opencode's stderr into stdout, but its own die() prints
    // to stderr).
    let logpath = logs.join("scaffold.log");
    let logf = std::fs::OpenOptions::new().create(true).append(true).open(&logpath)?;
    let logf2 = logf.try_clone()?;
    // Tee one child stream: each line lands in scaffold.log and on the SSE bus.
    fn tee<R: tokio::io::AsyncRead + Unpin + Send + 'static>(
        r: R, mut file: std::fs::File, bus: crate::bus::Bus, id: i64,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            use std::io::Write;
            use tokio::io::AsyncBufReadExt;
            let mut lines = tokio::io::BufReader::new(r).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let _ = writeln!(file, "{line}");
                bus.publish(id, json!({ "kind": "scaffold_log", "line": line }).to_string());
            }
        })
    }
    let mut child = cmd
        .arg("--out").arg(&bundle)
        .arg("--log-dir").arg(logs)
        .current_dir(&st.repo)
        .env("MK_SANDBOX", "auto")
        .env("MK_WT_ROOT", dir.join("wt"))
        // Creds ride as env, not argv, so the API key never appears in `ps`.
        .env("MK_OPENAI_BASE_URL", &creds.base_url)
        .env("MK_OPENAI_API_KEY", &creds.api_key)
        .env("MK_OPENCODE_MODEL", &creds.model)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let t_out = tee(child.stdout.take().expect("stdout piped"), logf, st.bus.clone(), id);
    let t_err = tee(child.stderr.take().expect("stderr piped"), logf2, st.bus.clone(), id);
    let status = child.wait().await?;
    let _ = t_out.await;
    let _ = t_err.await;
    // Persist the log to the jobs row win or lose — it's the only scaffold diagnostic
    // left once the job dir is reaped.
    if let Ok(text) = std::fs::read_to_string(&logpath) {
        let _ = st.db.set_scaffold_log(id, &text);
    }
    if !status.success() {
        anyhow::bail!("scaffold stage: scaffold-repro.py exited with {status}");
    }
    if !bundle.is_file() || tokio::fs::read_to_string(&bundle).await.map(|b| b.trim().is_empty()).unwrap_or(true) {
        anyhow::bail!("scaffold stage: agent produced no bundle");
    }
    // Done scaffolding — drop the marker so a rerun won't re-scaffold over the bundle.
    let _ = std::fs::remove_file(dir.join("scaffold.json"));
    Ok(())
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
