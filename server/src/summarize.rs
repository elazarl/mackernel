//! CPU-only LLM summarizer for reproducer jobs.
//!
//! Produces four outputs per job against the quantized Phi-3.5-mini GGUF (the
//! "pi3"/Phi-3 model, downloaded once and cached):
//!   - a short **title**,
//!   - a one-sentence **reproducer** summary (at job start, bundle only),
//!   - a one-sentence **result** summary (at job end, + run output),
//!   - a Markdown **detail** ("why it failed", reading the bundle + all logs;
//!     a GitHub-flavored Markdown doc with Summary/Root cause/Evidence sections).
//! Set `MK_SUMMARY_DISABLE=1` to turn the feature off entirely, or
//! `MK_LLAMA_DISABLE=1` to drop just the local model and rely on remote backends.
//!
//! Inference runs out-of-process against one or more OpenAI-compatible servers.
//! `load()` spawns llama.cpp's `llama-server` (the most stable native CPU LLM
//! runtime) as a local subprocess and talks to its `/v1/chat/completions` endpoint
//! over HTTP; the binary comes from `$MK_LLAMA_SERVER`, else `llama-server` on
//! `PATH`, else a prebuilt release is downloaded and cached, and the GGUF is
//! downloaded + cached by llama-server itself. The child is killed when the
//! `Summarizer` drops, and can be CPU-deprioritized via `MK_LLAMA_NICE`.
//!
//! Additional **remote** backends are configured via `MK_OPENAI_SERVERS` (a
//! quote-free `;`/`,`-delimited spec; e.g. OpenRouter — see `parse_servers`). Every
//! summary is generated against *all* backends; one is the `primary` whose output
//! fills the legacy per-field columns and streams live to the UI, the rest are
//! stored per-server in `job_summaries`.

use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::json;
use tracing::warn;

const SYS_TITLE: &str =
    "Reply with exactly two words — a terse title — and nothing else. No punctuation, no preamble.";
const SYS_REPRO: &str = "The job has only just started and has no results yet. Reply with exactly one short sentence describing what the reproducer tests. No preamble.";
const SYS_RESULT: &str = r#"Reply with exactly one short sentence and no preamble describing what actually happened on this run — whether it reproduced and the outcome."#;
const SYS_DETAIL: &str = r#"You are analyzing a Linux kernel bug reproducer run. You are given the reproducer's source code and the run logs.

Read the reproducer code and the logs, then explain:
1. What the reproducer does and how it tries to trigger the bug.
2. What actually happened on this run (did it reproduce, and the outcome).

Support your explanation with evidence: quote the most relevant lines of the reproducer code AND the most relevant dmesg/console lines. Use fenced code blocks for quotes.

Reply ONLY in concise GitHub-flavored Markdown, no preamble."#;

/// Appended to the opencode `detail` prompt. The opencode CLI runs the agent with its
/// file tools in the job's logs dir (see `detail`), so the ~600 KB of logs are read as
/// files rather than embedded in one CLI argument — the old inline prompt tripped
/// Linux's ~128 KB single-argv limit (`MAX_ARG_STRLEN`) and failed every time.
const DETAIL_READ_LOGS_HINT: &str = "\
The run logs are files in your current working directory — open them with your file tools:
- compile.log — the kernel + out-of-tree module + userspace build output
- console.log — the guest's serial console (dmesg)
- run.log — the orchestrator log (it SSHes into the VM and runs the commands; an SSH hang or timeout here signals reproduction, not an infrastructure failure)
A compare job nests these under baseline/ and patched/ subdirectories — read those if the top-level files are absent.
Quote the most relevant reproducer lines AND the most relevant dmesg/console lines as evidence.";

const REPEAT_PENALTY: f32 = 1.1;
const REPEAT_LAST_N: usize = 64;

/// Extra `max_tokens` for remote backends. Many free remote models (e.g. gpt-oss)
/// are *reasoning* models that spend the budget thinking before emitting any
/// `content` (the only part we keep), so a tight cap yields an empty answer. The
/// local non-reasoning model keeps its tight per-field caps.
const REMOTE_REASONING_HEADROOM: usize = 1024;

/// Per-file cap (chars) for logs attached to the end-of-job `detail` prompt. The cap
/// is per backend: the local model has a small context window (`CTX_SIZE`/2 per slot)
/// so its logs stay tight; remote backends have far larger contexts (e.g. 262K–1M
/// tokens) and get near-full logs so late lines (the KASAN `BUG:`, the run outcome)
/// are never truncated away. Remote stays bounded so a pathological multi-MB build log
/// can't overflow even a big context.
const LOCAL_LOG_CAP: usize = 10_000;
const REMOTE_LOG_CAP: usize = 200_000;

/// How long to wait for an `opencode run` one-shot before killing it.
const OPENCODE_TIMEOUT: Duration = Duration::from_secs(300);
/// Extra opencode attempts after the first on failure (timeout / non-zero exit /
/// empty output). The free zen tier and the CLI both fail transiently, so retry a
/// couple of times before giving up and surfacing the failure. Total attempts =
/// `OPENCODE_RETRIES + 1`.
const OPENCODE_RETRIES: usize = 2;
/// Pause between opencode attempts so a transient rate-limit/hiccup can clear.
const OPENCODE_BACKOFF: Duration = Duration::from_secs(2);
/// Cap (chars) for the reproducer source inlined into the opencode `detail` prompt.
/// Only the small reproducer rides in the prompt now; the logs are read as files (see
/// `DETAIL_READ_LOGS_HINT`), so this stays well under the ~128 KB single-argv limit.
const OPENCODE_REPRO_CAP: usize = 60_000;

const GGUF_REPO: &str = "bartowski/Phi-3.5-mini-instruct-GGUF";
const GGUF_FILE: &str = "Phi-3.5-mini-instruct-Q4_K_M.gguf";
/// Human-readable model label, surfaced in logs and `/api/summarizer`.
pub const LABEL: &str = "phi3.5-mini";

/// Total context window (KV cache). llama-server SPLITS this across the `--parallel`
/// slots, so with 2 slots each gets CTX_SIZE/2 = 16384 tokens. Sized so the `detail`
/// prompt — repro spec + the per-file-capped logs for the LOCAL backend (see
/// `LOCAL_LOG_CAP`) ≈ 10k tokens — plus 400 generated fits one slot with margin.
/// Bumped 16384 → 32768 (≈ +3 GB KV RAM on the home box, which has headroom) so the
/// local model can take the larger labeled-log prompts. Remote backends have their
/// own (much larger) context and get fuller logs (see `REMOTE_LOG_CAP`).
const CTX_SIZE: usize = 32768;
/// How long to wait for the server to come up. The first-ever boot blocks here
/// while llama-server downloads the ~2.5 GB GGUF (measured ~15 min on a home
/// link); cached boots are ~30s. Generous so a cold download doesn't get killed.
/// ponytail: bump if first boots on slow links still time out.
const READY_TIMEOUT: Duration = Duration::from_secs(1800);

/// Timing + token count for one generation.
pub struct GenStats {
    pub ms: u64,
    pub tokens: u32,
}

/// How a backend is invoked: an OpenAI-compatible HTTP endpoint, or the `opencode`
/// CLI run one-shot (`opencode run --pure -m <model> "<prompt>"`, stdout = answer).
#[derive(Clone, PartialEq)]
pub enum BackendKind {
    OpenAi,
    Opencode,
}

/// One summary backend: the model name plus how to reach it. For `OpenAi` it's an
/// HTTP base URL + optional bearer key (the local llama-server, or remotes like
/// OpenRouter from `MK_OPENAI_SERVERS`). For `Opencode` it's a `provider/model` run
/// through the local `opencode` CLI (no base_url/key). `primary` = its output fills
/// the legacy per-field columns and streams live to the UI.
#[derive(Clone)]
pub struct Backend {
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub primary: bool,
    pub kind: BackendKind,
}

impl Backend {
    /// The local, context-constrained llama-server (gets the tight log cap). Remote
    /// HTTP backends and opencode have big contexts and get fuller logs.
    fn is_local(&self) -> bool {
        self.kind == BackendKind::OpenAi && self.api_key.is_none()
    }
}

/// Owns the optional local `llama-server` subprocess (killed on drop) plus the set
/// of backends and a shared blocking HTTP client. `Send + Sync` (the `Child` lives
/// behind a `Mutex`), so it slots into the existing `Arc<OnceLock<Arc<Summarizer>>>`.
pub struct Summarizer {
    child: Option<Mutex<Child>>,
    client: reqwest::blocking::Client,
    backends: Vec<Backend>,
    mem_bytes: u64,
    /// Serializes `opencode` CLI runs (the free zen tier rejects concurrent
    /// invocations — a job's result+detail fire at once — and two heavyweight CLI
    /// agents also spike the host) AND records who's waiting/running so the UI can
    /// show it on hover. One run at a time, same as the old bare lock.
    opencode_queue: OpencodeQueue,
}

/// A visible, serialized queue for `opencode` CLI summary runs. `run_lock` enforces
/// one-at-a-time execution (unchanged from the old `Mutex<()>`); `entries` records the
/// waiting + running set in arrival order so `/api/summarizer` can surface it and the
/// 🧠 topbar tooltip can list who's waiting, for what, and how long.
pub struct OpencodeQueue {
    run_lock: Mutex<()>,
    entries: Mutex<Vec<QueueEntry>>,
    seq: AtomicU64,
    /// Set once after load: pings the global SSE bus whenever the queue changes so the
    /// UI refetches. `None` on the debug CLI path (no bus).
    on_change: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

/// One opencode run's place in the queue. `since_ms` is enqueue time; `started_ms` is
/// stamped when it flips to running (`None` while waiting). Queue-wait is derived from
/// these by the API (see `get_summarizer`).
#[derive(Serialize, Clone)]
pub struct QueueEntry {
    pub id: u64,
    pub job_id: i64,
    pub field: &'static str,
    pub backend: String,
    pub running: bool,
    pub since_ms: i64,
    pub started_ms: Option<i64>,
}

impl OpencodeQueue {
    fn new() -> Self {
        Self {
            run_lock: Mutex::new(()),
            entries: Mutex::new(Vec::new()),
            seq: AtomicU64::new(0),
            on_change: Mutex::new(None),
        }
    }

    /// Install the change notifier (a closure that pings the global bus). Called once
    /// after the model loads, before the summarizer is published.
    pub fn set_notifier(&self, f: Arc<dyn Fn() + Send + Sync>) {
        *self.on_change.lock().unwrap() = Some(f);
    }

    /// Current waiting + running entries, in arrival order.
    pub fn snapshot(&self) -> Vec<QueueEntry> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn notify(&self) {
        // Clone the callback out so we don't hold the lock while it runs.
        let cb = self.on_change.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(cb) = cb {
            cb();
        }
    }

    /// Register a waiting entry and return a ticket. The entry lives until the ticket
    /// drops; call `run` on it to block for a turn and execute.
    fn enter(&self, job_id: i64, field: &'static str, backend: &str) -> QueueTicket<'_> {
        let id = self.seq.fetch_add(1, Ordering::Relaxed);
        self.entries.lock().unwrap_or_else(|e| e.into_inner()).push(QueueEntry {
            id, job_id, field, backend: backend.to_string(),
            running: false, since_ms: crate::now_ms(), started_ms: None,
        });
        self.notify();
        QueueTicket { queue: self, id }
    }
}

/// A registered queue entry. Blocks for the serialization lock in `run`, and removes
/// its entry on drop.
struct QueueTicket<'a> {
    queue: &'a OpencodeQueue,
    id: u64,
}

impl QueueTicket<'_> {
    /// Block until it's our turn (holding the serialization lock), mark the entry
    /// running, then execute `f` while still holding the lock.
    fn run<T>(&self, f: impl FnOnce() -> T) -> T {
        let _guard = self.queue.run_lock.lock().unwrap_or_else(|e| e.into_inner());
        {
            let mut es = self.queue.entries.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(e) = es.iter_mut().find(|e| e.id == self.id) {
                e.running = true;
                e.started_ms = Some(crate::now_ms());
            }
        }
        self.queue.notify();
        f()
    }
}

impl Drop for QueueTicket<'_> {
    fn drop(&mut self) {
        self.queue.entries.lock().unwrap_or_else(|e| e.into_inner())
            .retain(|e| e.id != self.id);
        self.queue.notify();
    }
}

impl Drop for Summarizer {
    fn drop(&mut self) {
        if let Some(child) = &self.child {
            if let Ok(mut c) = child.lock() {
                let _ = c.kill();
                let _ = c.wait();
            }
        }
    }
}

impl Summarizer {
    /// True unless explicitly disabled via `MK_SUMMARY_DISABLE`.
    pub fn enabled() -> bool {
        !matches!(
            std::env::var("MK_SUMMARY_DISABLE").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE")
        )
    }

    /// Assemble the backend set: parse remote backends from `MK_OPENAI_SERVERS`,
    /// then spawn the local `llama-server` (degrading to remote-only if it can't
    /// start but remotes exist). Blocking — call from a blocking thread. First call
    /// may download the binary and the ~2.5 GB model; later calls hit the caches.
    pub fn load() -> Result<Self> {
        let client = reqwest::blocking::Client::builder()
            .timeout(Duration::from_secs(600))
            .build()
            .context("build http client")?;

        // Remote OpenAI-compatible backends (e.g. OpenRouter), if configured.
        let mut backends: Vec<Backend> = parse_remote_backends();

        // Local llama-server backend. If it can't start but remotes exist, degrade
        // to remote-only rather than disabling summaries entirely. `MK_LLAMA_DISABLE`
        // skips the local model outright (e.g. RAM-constrained hosts that rely on the
        // remote/opencode backends) — distinct from `MK_SUMMARY_DISABLE`, which kills
        // summaries entirely.
        let mut child: Option<Mutex<Child>> = None;
        let local_result = if local_disabled() {
            Err(anyhow::anyhow!("local llama-server disabled via MK_LLAMA_DISABLE"))
        } else {
            spawn_local_server()
        };
        match local_result {
            Ok((c, port)) => {
                child = Some(Mutex::new(c));
                backends.push(Backend {
                    label: LABEL.to_string(),
                    // base_url is the full OpenAI API base incl. version (we append
                    // only `/chat/completions`); llama-server serves `/v1/...`.
                    base_url: format!("http://127.0.0.1:{port}/v1"),
                    api_key: None,
                    model: LABEL.to_string(),
                    primary: false,
                    kind: BackendKind::OpenAi,
                });
            }
            Err(e) if !backends.is_empty() => {
                warn!("local llama-server unavailable, using remote backends only: {e:#}");
            }
            Err(e) => {
                return Err(e).context("start local llama-server (no remote backends configured)")
            }
        }

        // Exactly one primary: honor a configured one, else the first backend.
        if !backends.iter().any(|b| b.primary) {
            backends[0].primary = true;
        }

        let mut s = Self {
            child,
            client,
            backends,
            mem_bytes: 0,
            opencode_queue: OpencodeQueue::new(),
        };
        // Warm up the local server and measure its RSS for /api/summarizer.
        if let Some(local) = s.backends.iter().find(|b| b.label == LABEL).cloned() {
            let _ = s.generate(&local, SYS_TITLE, "ping", 1, false, &|_| {}, 0, "warmup");
            if let Some(child) = &s.child {
                let pid = child.lock().unwrap().id();
                s.mem_bytes = rss_of(pid);
            }
        }
        Ok(s)
    }

    /// The configured backends (one is `primary`).
    pub fn backends(&self) -> &[Backend] {
        &self.backends
    }

    /// Measured RAM of the local llama-server child (weights + KV cache), in bytes.
    /// 0 when running remote-only.
    pub fn memory_bytes(&self) -> u64 {
        self.mem_bytes
    }

    /// The visible opencode run queue (backs `/api/summarizer`'s `queue` field).
    pub fn opencode_queue(&self) -> &OpencodeQueue {
        &self.opencode_queue
    }

    /// Terse two-word title for the job, from the bundle alone (job start).
    pub fn title(
        &self,
        b: &Backend,
        bundle_md: &str,
        on_tok: &dyn Fn(u32),
        job_id: i64,
    ) -> Result<(String, GenStats)> {
        let user = curate_bundle(bundle_md);
        let (raw, stats) = self.generate(b, SYS_TITLE, &user, 8, false, on_tok, job_id, "title")?;
        Ok((two_words(&raw), stats))
    }

    /// One sentence on what the reproducer tests, from the bundle alone (job start).
    pub fn summarize_repro(
        &self,
        b: &Backend,
        bundle_md: &str,
        on_tok: &dyn Fn(u32),
        job_id: i64,
    ) -> Result<(String, GenStats)> {
        let user = curate_bundle(bundle_md);
        self.generate(b, SYS_REPRO, &user, 64, false, on_tok, job_id, "repro")
    }

    /// One sentence on what happened on this run: bundle + curated issues + outcome
    /// (job end).
    pub fn summarize_result(
        &self,
        b: &Backend,
        bundle_md: &str,
        issues_json: &str,
        exit_code: Option<i64>,
        outcome: &str,
        on_tok: &dyn Fn(u32),
        job_id: i64,
    ) -> Result<(String, GenStats)> {
        let exit = exit_code
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into());
        let user = format!(
            "{}\n\nRun result: outcome={outcome}, exit_code={exit}.\n{}",
            curate_bundle(bundle_md),
            curate_issues(issues_json),
        );
        self.generate(b, SYS_RESULT, &user, 96, false, on_tok, job_id, "result")
    }

    /// Markdown analysis of the run (job end). The user message is the reproducer
    /// spec plus the key logs, each prefixed with a plain-language explanation of
    /// what it is (see `curate_end_context`). Returns GitHub-flavored Markdown; the
    /// DB stores it verbatim.
    pub fn detail(
        &self,
        b: &Backend,
        bundle_md: &str,
        logs_dir: &Path,
        on_tok: &dyn Fn(u32),
        job_id: i64,
    ) -> Result<(String, GenStats)> {
        // opencode is invoked as a CLI, so a full-log prompt would exceed the ~128 KB
        // single-argv limit. Instead give it a small prompt — the reproducer source
        // inline plus an instruction to READ the logs as files — and run it in the logs
        // dir so its file tools can open them (see DETAIL_READ_LOGS_HINT).
        if b.kind == BackendKind::Opencode {
            let user = format!(
                "This is the reproducer you are using — its full source. Quote the relevant code in your analysis:\n{}\n\n{}",
                cap_chars(bundle_md.trim(), OPENCODE_REPRO_CAP),
                DETAIL_READ_LOGS_HINT,
            );
            return generate_opencode(
                &self.opencode_queue, job_id, "detail", b, SYS_DETAIL, &user,
                logs_dir.to_path_buf(), false, on_tok,
            );
        }
        // HTTP backends take the logs inline (they have huge contexts): only the local
        // llama-server is context-constrained and gets the tight cap.
        let cap = if b.is_local() { LOCAL_LOG_CAP } else { REMOTE_LOG_CAP };
        let user = curate_end_context(bundle_md, logs_dir, cap);
        self.generate(b, SYS_DETAIL, &user, 400, false, on_tok, job_id, "detail")
    }

    /// One completion against a backend. Dispatches to the OpenAI HTTP path or the
    /// `opencode` CLI depending on the backend kind. `job_id`/`field` label the opencode
    /// queue entry (ignored for HTTP backends).
    #[allow(clippy::too_many_arguments)]
    fn generate(
        &self,
        b: &Backend,
        sys: &str,
        user: &str,
        max_new: usize,
        json: bool,
        on_tok: &dyn Fn(u32),
        job_id: i64,
        field: &'static str,
    ) -> Result<(String, GenStats)> {
        if b.kind == BackendKind::Opencode {
            // The short fields (title/repro/result) answer inline, so run --pure in a
            // neutral dir. The `detail` field bypasses this and calls generate_opencode
            // directly with a working dir + tools (see `detail`).
            return generate_opencode(&self.opencode_queue, job_id, field, b, sys, user,
                                     std::env::temp_dir(), true, on_tok);
        }
        self.generate_openai(b, sys, user, max_new, json, on_tok)
    }

    /// One deterministic (greedy) completion via the OpenAI chat API, streamed so we
    /// can report the live token count (`on_tok`, throttled) and measure wall time.
    /// The server applies the model's chat template, so system/user go as messages
    /// rather than a hand-rolled prompt string.
    fn generate_openai(
        &self,
        b: &Backend,
        sys: &str,
        user: &str,
        max_new: usize,
        json: bool,
        on_tok: &dyn Fn(u32),
    ) -> Result<(String, GenStats)> {
        // Reasoning remote models need headroom (see REMOTE_REASONING_HEADROOM).
        let max_tokens = if b.api_key.is_some() {
            max_new + REMOTE_REASONING_HEADROOM
        } else {
            max_new
        };
        let mut body = json!({
            "model": b.model,
            "messages": [
                {"role": "system", "content": sys},
                {"role": "user", "content": user},
            ],
            "temperature": 0.0,          // deterministic — matches old Sampling::ArgMax
            "max_tokens": max_tokens,
            "repeat_penalty": REPEAT_PENALTY,
            "repeat_last_n": REPEAT_LAST_N,
            "stream": true,
            "stream_options": {"include_usage": true},
        });
        if json {
            // llama-server constrains output to valid JSON via a grammar.
            body["response_format"] = serde_json::json!({"type": "json_object"});
        }
        let started = Instant::now();
        let mut req = self
            .client
            .post(format!("{}/chat/completions", b.base_url))
            .json(&body);
        if let Some(key) = &b.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .context("POST /v1/chat/completions")?
            .error_for_status()
            .context("summary backend returned an error status")?;

        // Parse the OpenAI SSE stream: `data: {json}` lines, ending with `data: [DONE]`.
        // Each chunk carries an incremental `choices[0].delta.content`; the final usage
        // chunk (include_usage) carries the authoritative `usage.completion_tokens`.
        let mut text = String::new();
        let mut tokens: u32 = 0;
        let mut usage_tokens: Option<u32> = None;
        for line in BufReader::new(resp).lines() {
            let line = line.context("read completion stream")?;
            let Some(data) = line.strip_prefix("data:") else {
                continue;
            };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(chunk): Result<serde_json::Value, _> = serde_json::from_str(data) else {
                continue;
            };
            if let Some(c) = chunk["choices"][0]["delta"]["content"].as_str() {
                if !c.is_empty() {
                    text.push_str(c);
                    tokens += 1;
                    if tokens % 8 == 0 {
                        on_tok(tokens);
                    }
                }
            }
            if let Some(n) = chunk["usage"]["completion_tokens"].as_u64() {
                usage_tokens = Some(n as u32);
            }
        }
        let tokens = usage_tokens.unwrap_or(tokens);
        on_tok(tokens); // final count
        let stats = GenStats {
            ms: started.elapsed().as_millis() as u64,
            tokens,
        };
        Ok((text.trim().to_string(), stats))
    }
}

/// One completion via the `opencode` CLI run one-shot: `opencode run [--pure] -m
/// <model> "<sys>\n\n<user>"`. Either way stdout is exactly the answer (the
/// "> build · model" header, ANSI, and any tool traces go to stderr). Binary from
/// `$MK_OPENCODE_BIN`, else `opencode` on PATH. `pure` = `--pure` (no tools, answer
/// inline) for the short fields in a neutral `cwd`; `!pure` enables the agent's file
/// tools (with `--dangerously-skip-permissions`) so `detail` can read the logs from
/// `cwd` instead of receiving them in the prompt. Killed after a per-attempt timeout.
/// opencode emits no token usage, so `tokens` is a whitespace-word estimate.
/// `queue` serializes runs — the free zen tier rejects concurrent invocations — and
/// records this run's `job_id`/`field` as a waiting entry, flipped to running once it
/// holds the serialization lock (see `OpencodeQueue`).
#[allow(clippy::too_many_arguments)]
fn generate_opencode(
    queue: &OpencodeQueue,
    job_id: i64,
    field: &'static str,
    b: &Backend,
    sys: &str,
    user: &str,
    cwd: std::path::PathBuf,
    pure: bool,
    on_tok: &dyn Fn(u32),
) -> Result<(String, GenStats)> {
    // Registered as waiting; `run` blocks for a turn, marks it running, and removes the
    // entry on drop. Only one opencode CLI executes at a time. Each attempt spawns a
    // fresh `opencode run` (see `with_retries`), so a transient failure sends another
    // instance without releasing the serialization slot.
    let ticket = queue.enter(job_id, field, &b.label);
    ticket.run(|| {
        let bin = std::env::var("MK_OPENCODE_BIN").unwrap_or_else(|_| "opencode".to_string());
        let prompt = format!("{sys}\n\n{user}");
        let label = format!("job {job_id}: opencode {field}");
        with_retries(OPENCODE_RETRIES + 1, OPENCODE_BACKOFF, &label, || {
            opencode_attempt(&bin, &b.model, &prompt, &cwd, pure, on_tok)
        })
    })
}

/// One `opencode run` invocation: spawn, poll until it exits or the timeout fires,
/// then read stdout as the answer. A non-zero exit, a timeout, or empty output is a
/// failure (`Err`) — `with_retries` decides whether to try another instance.
fn opencode_attempt(
    bin: &str,
    model: &str,
    prompt: &str,
    cwd: &Path,
    pure: bool,
    on_tok: &dyn Fn(u32),
) -> Result<(String, GenStats)> {
    let started = Instant::now();
    let mut cmd = Command::new(bin);
    cmd.arg("run");
    if pure {
        // No tools: the agent answers directly from the (self-contained) prompt.
        cmd.arg("--pure");
    } else {
        // Tools enabled so the agent can read the logs from `cwd`; auto-approve so the
        // non-interactive run doesn't block on a permission prompt.
        cmd.arg("--dangerously-skip-permissions");
    }
    let mut child = cmd
        .args(["-m", model, prompt])
        .current_dir(cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .with_context(|| format!("spawn {bin} run"))?;

    let deadline = Instant::now() + OPENCODE_TIMEOUT;
    loop {
        match child.try_wait()? {
            Some(status) if status.success() => break,
            Some(status) => bail!("opencode run exited with {status}"),
            None => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    bail!("opencode run timed out after {OPENCODE_TIMEOUT:?}");
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        }
    }

    // Summaries are small, so reading stdout after the child exits won't deadlock.
    let mut text = String::new();
    if let Some(mut out) = child.stdout.take() {
        out.read_to_string(&mut text).context("read opencode stdout")?;
    }
    let text = text.trim().to_string();
    if text.is_empty() {
        bail!("opencode produced no output");
    }
    let tokens = text.split_whitespace().count() as u32; // estimate; CLI has no usage
    on_tok(tokens);
    Ok((text, GenStats { ms: started.elapsed().as_millis() as u64, tokens }))
}

/// Run `attempt` up to `total` times, returning the first `Ok`. On each failure it
/// logs `label` + the attempt number and (unless it was the last) sleeps `backoff`
/// before retrying. Returns the last `Err` if every attempt fails.
fn with_retries<T>(
    total: usize,
    backoff: Duration,
    label: &str,
    mut attempt: impl FnMut() -> Result<T>,
) -> Result<T> {
    let mut last: Option<anyhow::Error> = None;
    for n in 1..=total {
        match attempt() {
            Ok(v) => return Ok(v),
            Err(e) => {
                warn!("{label} attempt {n}/{total} failed: {e:#}");
                last = Some(e);
                if n < total {
                    std::thread::sleep(backoff);
                }
            }
        }
    }
    Err(last.unwrap_or_else(|| anyhow::anyhow!("{label}: no attempts run")))
}

/// Spawn the local `llama-server` (optionally CPU-deprioritized via `MK_LLAMA_NICE`)
/// and block until it answers `/health`. Returns the child and the port it listens
/// on. Blocking — the first call may download the binary and the ~2.5 GB GGUF.
fn spawn_local_server() -> Result<(Child, u16)> {
    let bin = resolve_binary().context("locate or download llama-server")?;
    let port: u16 = std::env::var("MK_LLAMA_PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(18080);
    let args = [
        "--host".to_string(),
        "127.0.0.1".to_string(),
        "--port".to_string(),
        port.to_string(),
        "--ctx-size".to_string(),
        CTX_SIZE.to_string(),
        "--parallel".to_string(),
        "2".to_string(),
        "--hf-repo".to_string(),
        GGUF_REPO.to_string(),
        "--hf-file".to_string(),
        GGUF_FILE.to_string(),
    ];
    // `MK_LLAMA_NICE` lowers the model's CPU priority (it's a best-effort secondary
    // once a remote backend is primary): run `nice -n N <bin> …` — a shell-out in
    // the same style as the `tar`/`ps` calls. Unset = normal priority.
    let nice = std::env::var("MK_LLAMA_NICE")
        .ok()
        .and_then(|n| n.parse::<i32>().ok());
    let mut cmd = match nice {
        Some(n) => {
            let mut c = Command::new("nice");
            c.arg("-n").arg(n.to_string()).arg(&bin).args(&args);
            c
        }
        None => {
            let mut c = Command::new(&bin);
            c.args(&args);
            c
        }
    };
    cmd.stdout(Stdio::null()).stderr(Stdio::null());
    // Run from the binary's own directory so bundled shared libraries resolve.
    if let Some(dir) = bin.parent() {
        cmd.current_dir(dir);
    }
    let mut child = cmd
        .spawn()
        .with_context(|| format!("spawn {}", bin.display()))?;

    // Poll /health until ready, the child exits, or we time out.
    let health = format!("http://127.0.0.1:{port}/health");
    let deadline = Instant::now() + READY_TIMEOUT;
    let poll = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    loop {
        if let Some(status) = child.try_wait()? {
            // Common cause: a downloaded prebuilt that won't run on this host
            // (e.g. the Ubuntu build needs a newer libstdc++ than RHEL ships).
            bail!(
                "llama-server exited early ({status}); if the prebuilt is \
                 incompatible with this host, set MK_LLAMA_SERVER to a working binary"
            );
        }
        if let Ok(r) = poll.get(&health).send() {
            if r.status().is_success() {
                return Ok((child, port));
            }
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            bail!("timed out after {READY_TIMEOUT:?} waiting for {health}");
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

/// Parse `MK_OPENAI_SERVERS`. Format is a quote-free, space-free spec (so systemd
/// `Environment=` and shell quoting can't mangle it — JSON's double quotes get
/// stripped by systemd): `;`-separated server entries, each a `,`-separated list of
/// `key=value` fields. Keys: `label`, `base_url`, `model` (required), `api_key_env`
/// (env var holding the bearer key — keeps the secret out of this value), `api_key`
/// (literal, discouraged), `primary` (true/1/yes), `kind` (`opencode` to run via the
/// opencode CLI; default HTTP). `base_url` is the full OpenAI API base *including* the
/// version path (we append only `/chat/completions`) and is required for HTTP backends
/// but not for `kind=opencode`. Examples:
/// `label=openrouter,base_url=https://openrouter.ai/api/v1,model=x:free,api_key_env=OPENROUTER_API_KEY,primary=true`
/// `label=opencode,model=opencode/deepseek-v4-flash-free,kind=opencode,primary=true`
/// Returns [] when the var is unset/blank.
/// True when the local llama-server backend is disabled via `MK_LLAMA_DISABLE`.
fn local_disabled() -> bool {
    matches!(
        std::env::var("MK_LLAMA_DISABLE").ok().as_deref(),
        Some("1") | Some("true") | Some("TRUE")
    )
}

fn parse_remote_backends() -> Vec<Backend> {
    let Some(raw) = std::env::var("MK_OPENAI_SERVERS")
        .ok()
        .filter(|s| !s.trim().is_empty())
    else {
        return Vec::new();
    };
    parse_servers(&raw, |k| std::env::var(k).ok().filter(|s| !s.is_empty()))
}

/// Pure parser for the `MK_OPENAI_SERVERS` spec (env lookup injected for
/// testability). A backend whose `api_key_env` names an unset/empty var is skipped
/// with a warning, as is one missing label/base_url/model.
fn parse_servers(raw: &str, lookup: impl Fn(&str) -> Option<String>) -> Vec<Backend> {
    let mut out = Vec::new();
    for entry in raw.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (mut label, mut base_url, mut model) = (None, None, None);
        let (mut api_key, mut api_key_env, mut primary) = (None, None, false);
        let mut kind = BackendKind::OpenAi;
        for kv in entry.split(',') {
            let Some((k, v)) = kv.split_once('=') else {
                continue;
            };
            let v = v.trim().to_string();
            match k.trim() {
                "label" => label = Some(v),
                "base_url" => base_url = Some(v.trim_end_matches('/').to_string()),
                "model" => model = Some(v),
                "api_key" => api_key = Some(v),
                "api_key_env" => api_key_env = Some(v),
                "primary" => primary = matches!(v.as_str(), "1" | "true" | "yes"),
                "kind" => kind = if v == "opencode" { BackendKind::Opencode } else { BackendKind::OpenAi },
                other => warn!("MK_OPENAI_SERVERS: ignoring unknown field '{other}'"),
            }
        }
        // label + model always required; base_url only for HTTP (OpenAI) backends —
        // opencode is invoked via its CLI and needs no URL.
        let (Some(label), Some(model)) = (label, model) else {
            warn!("MK_OPENAI_SERVERS entry missing label/model; skipping: {entry}");
            continue;
        };
        let base_url = base_url.unwrap_or_default();
        if kind == BackendKind::OpenAi && base_url.is_empty() {
            warn!("summary backend '{label}': base_url required for an HTTP backend; skipping");
            continue;
        }
        let api_key = match api_key_env {
            Some(env_name) => match lookup(&env_name) {
                Some(k) => Some(k),
                None => {
                    warn!("summary backend '{label}': {env_name} unset/empty; skipping");
                    continue;
                }
            },
            None => api_key,
        };
        out.push(Backend {
            label,
            base_url,
            api_key,
            model,
            primary,
            kind,
        });
    }
    out
}

/// Locate the `llama-server` binary: explicit override, then `PATH`, then download.
fn resolve_binary() -> Result<PathBuf> {
    if let Some(p) = std::env::var_os("MK_LLAMA_SERVER") {
        let p = PathBuf::from(p);
        if p.is_file() {
            return Ok(p);
        }
        bail!("MK_LLAMA_SERVER={} is not a file", p.display());
    }
    if let Some(p) = which("llama-server") {
        return Ok(p);
    }
    download_llama_server()
}

/// First `llama-server` found on `PATH` (no external `which` dependency).
fn which(bin: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(bin))
        .find(|p| p.is_file())
}

/// Download the latest prebuilt llama.cpp release for this host and unpack it into
/// the cache. Reuses an already-unpacked binary if present.
fn download_llama_server() -> Result<PathBuf> {
    let dir = cache_root();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    if let Some(existing) = find_server_bin(&dir) {
        return Ok(existing);
    }

    let suffix = asset_suffix()?;
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(600))
        .user_agent("mackernel-server")
        .build()?;
    let rel: serde_json::Value = client
        .get("https://api.github.com/repos/ggml-org/llama.cpp/releases/latest")
        .send()?
        .error_for_status()?
        .json()
        .context("parse latest-release json")?;
    let url = rel["assets"]
        .as_array()
        .and_then(|assets| {
            assets.iter().find_map(|a| {
                let name = a["name"].as_str()?;
                name.ends_with(suffix)
                    .then(|| a["browser_download_url"].as_str())
                    .flatten()
            })
        })
        .with_context(|| format!("no llama.cpp release asset ending in {suffix}"))?;

    let bytes = client.get(url).send()?.error_for_status()?.bytes()?;
    let archive = dir.join("llama-release.tar.gz");
    std::fs::write(&archive, &bytes).with_context(|| format!("write {}", archive.display()))?;
    extract_targz(&archive, &dir)?;
    let _ = std::fs::remove_file(&archive);
    find_server_bin(&dir).context("llama-server not found in downloaded archive")
}

/// Exact release-asset name suffix for this OS/arch — the plain CPU build (not the
/// rocm/sycl/vulkan/openvino variants). Selected from `std::env::consts` (compile-time
/// constants), so a Linux build picks the Linux asset even when built on a Mac.
/// Linux/macOS ship `.tar.gz`; Windows ships `.zip` and isn't supported here
/// (set `MK_LLAMA_SERVER` on Windows).
fn asset_suffix() -> Result<&'static str> {
    let (os, arch) = (std::env::consts::OS, std::env::consts::ARCH);
    let s = match (os, arch) {
        ("linux", "x86_64") => "bin-ubuntu-x64.tar.gz",
        ("linux", "aarch64") => "bin-ubuntu-arm64.tar.gz",
        ("macos", "aarch64") => "bin-macos-arm64.tar.gz",
        ("macos", "x86_64") => "bin-macos-x64.tar.gz",
        _ => bail!("no prebuilt llama-server for {os}/{arch}; set MK_LLAMA_SERVER to a binary"),
    };
    Ok(s)
}

/// Unpack a `.tar.gz` via system `tar` (always present on Linux/macOS — same
/// shell-out style as the `ps` RSS fallback).
fn extract_targz(archive: &Path, dest: &Path) -> Result<()> {
    let status = Command::new("tar")
        .arg("-xzf")
        .arg(archive)
        .arg("-C")
        .arg(dest)
        .status()
        .context("run tar")?;
    if !status.success() {
        bail!("tar failed extracting {}", archive.display());
    }
    Ok(())
}

/// Cache dir for the downloaded binary: `$XDG_CACHE_HOME` or `~/.cache`, else tmp.
fn cache_root() -> PathBuf {
    if let Some(d) = std::env::var_os("XDG_CACHE_HOME") {
        return PathBuf::from(d).join("mackernel/llama.cpp");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return PathBuf::from(home).join(".cache/mackernel/llama.cpp");
    }
    std::env::temp_dir().join("mackernel/llama.cpp")
}

/// Find `llama-server` anywhere under `dir` and ensure it's executable.
fn find_server_bin(dir: &Path) -> Option<PathBuf> {
    let target = if cfg!(windows) {
        "llama-server.exe"
    } else {
        "llama-server"
    };
    let found = walk(dir)
        .into_iter()
        .find(|p| p.file_name().and_then(|n| n.to_str()) == Some(target))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(&found) {
            let mut perm = meta.permissions();
            perm.set_mode(perm.mode() | 0o755);
            let _ = std::fs::set_permissions(&found, perm);
        }
    }
    Some(found)
}

/// Recursively list every file under `dir` (best-effort, skips unreadable dirs).
fn walk(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else {
            continue;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else {
                out.push(p);
            }
        }
    }
    out
}

/// Resident set size (bytes) of process `pid`. Linux reads `/proc/<pid>/statm`;
/// elsewhere it shells out to `ps`. Best-effort: returns 0 on failure.
/// ponytail: assumes 4 KiB pages on Linux — fine for the x86_64 home box.
fn rss_of(pid: u32) -> u64 {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string(format!("/proc/{pid}/statm")) {
        if let Some(pages) = s
            .split_whitespace()
            .nth(1)
            .and_then(|p| p.parse::<u64>().ok())
        {
            return pages * 4096;
        }
    }
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &pid.to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Read `name` from the logs dir, or its `baseline/` subdir for compare jobs (whose
/// per-variant logs nest there), trimmed and capped. None if absent/empty.
fn read_log_capped(logs_dir: &Path, name: &str, cap: usize) -> Option<String> {
    for cand in [logs_dir.join(name), logs_dir.join("baseline").join(name)] {
        if let Ok(c) = std::fs::read_to_string(&cand) {
            let c = c.trim();
            if !c.is_empty() {
                return Some(cap_chars(c, cap));
            }
        }
    }
    None
}

/// End-of-job analysis context: the reproducer spec plus the key logs, each prefixed
/// with a plain-language explanation of what it is, so the model knows what each
/// attachment is. Capped per file so the prompt stays bounded (prefill cost scales
/// with length).
fn curate_end_context(bundle_md: &str, logs_dir: &Path, log_cap: usize) -> String {
    let mut out = String::new();
    // Raw bundle (NOT code-stripped) so the model can quote the reproducer's source.
    out.push_str("This is the reproducer you are using — its full source (code, config, run script) as submitted to the runner. Quote the relevant code in your analysis:\n");
    out.push_str(&cap_chars(bundle_md.trim(), log_cap));
    out.push_str("\n\n");

    const ATTACH: &[(&str, &str)] = &[
        ("compile.log",
         "This compiles the kernel, the out-of-tree module, and the userspace from the reproducer:"),
        ("console.log", "This is the dmesg from the serial port:"),
        ("run.log",
         "This is the orchestrator log: it connects to the VM over SSH and runs the user's \
          commands (the userspace program, or the kernel module if one exists). A reproduced \
          `BUG:` can hang or panic the kernel, so an SSH hang or timeout here is expected and \
          signals reproduction — not an infrastructure failure:"),
    ];
    // The decisive lines land late (the KASAN `BUG:` is ~7k bytes into console.log
    // after the boot/cloud-init spam; run.log's SSH-connect + outcome are at its end),
    // so the cap must be generous enough to reach them — hence the large REMOTE cap.
    for (file, label) in ATTACH {
        if let Some(c) = read_log_capped(logs_dir, file, log_cap) {
            out.push_str(label);
            out.push('\n');
            out.push_str(&c);
            out.push_str("\n\n");
        }
    }
    out.trim().to_string()
}

/// Reproducer bundles are mostly C source inside code fences; for a short summary we
/// want the prose + frontmatter only. Strip fenced code blocks, squeeze blank lines,
/// and cap length (model prefill cost scales with prompt length).
fn curate_bundle(md: &str) -> String {
    let mut out = String::new();
    let mut in_fence = false;
    for line in md.lines() {
        let t = line.trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }

    let mut squeezed = String::new();
    let mut prev_blank = false;
    for line in out.lines() {
        let blank = line.trim().is_empty();
        if blank && prev_blank {
            continue;
        }
        squeezed.push_str(line);
        squeezed.push('\n');
        prev_blank = blank;
    }
    cap_chars(squeezed.trim(), 1600)
}

/// `collect_issues` returns a JSON array `[{"file", "blocks":[{"head":[...], "trace":[...]}]}]`
/// (scanning the top-level logs plus the per-variant baseline/ and patched/ subdirs).
/// Flatten the `head` lines, each tagged with its file, into a short plaintext block
/// (capped) for the prompt, or a clear "no issues" note when empty. NOTE: this must
/// match collect_issues' shape — it previously looked for a `"lines"` key that shape
/// never has, so it always reported "no errors" and the result summary was wrong.
fn curate_issues(issues_json: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(issues_json).unwrap_or(serde_json::Value::Null);
    let mut lines: Vec<String> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for section in arr {
            let file = section.get("file").and_then(|f| f.as_str()).unwrap_or("");
            let Some(blocks) = section.get("blocks").and_then(|b| b.as_array()) else {
                continue;
            };
            for block in blocks {
                if let Some(head) = block.get("head").and_then(|h| h.as_array()) {
                    for l in head {
                        if let Some(s) = l.as_str() {
                            lines.push(format!("{file}: {}", s.trim()));
                        }
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        return "No errors or sanitizer reports were found in the logs.".to_string();
    }
    let mut s = String::from("Issues found in logs (file: line):\n");
    for l in lines.iter().take(40) {
        s.push_str(l);
        s.push('\n');
    }
    cap_chars(s.trim(), 1500)
}

/// Keep the first two whitespace-separated tokens, stripped of surrounding punctuation —
/// small models often add a stray period, quotes, or a third word.
fn two_words(s: &str) -> String {
    s.split_whitespace()
        .map(|w| w.trim_matches(|c: char| !c.is_alphanumeric()))
        .filter(|w| !w.is_empty())
        .take(2)
        .collect::<Vec<_>>()
        .join(" ")
}

fn cap_chars(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        s.to_string()
    } else {
        let truncated: String = s.chars().take(n).collect();
        format!("{truncated}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn with_retries_stops_on_success_and_bounds_attempts() {
        use std::cell::Cell;
        // Fails twice, succeeds on the third: exactly 3 calls, returns Ok.
        let calls = Cell::new(0);
        let r = with_retries(3, Duration::from_millis(0), "t", || {
            calls.set(calls.get() + 1);
            if calls.get() < 3 { anyhow::bail!("boom") } else { Ok(42) }
        });
        assert_eq!(r.unwrap(), 42);
        assert_eq!(calls.get(), 3);
        // Always fails: runs exactly `total` times, returns the last Err.
        let calls = Cell::new(0);
        let r: Result<i32> = with_retries(3, Duration::from_millis(0), "t", || {
            calls.set(calls.get() + 1);
            anyhow::bail!("nope")
        });
        assert!(r.is_err());
        assert_eq!(calls.get(), 3);
    }

    #[test]
    fn queue_tracks_waiting_running_and_dequeues_on_drop() {
        let q = OpencodeQueue::new();
        let t1 = q.enter(1, "detail", "oc");
        let t2 = q.enter(2, "title", "oc");
        // Both registered, both waiting, arrival order preserved.
        let s = q.snapshot();
        assert_eq!(s.len(), 2);
        assert_eq!((s[0].job_id, s[1].job_id), (1, 2));
        assert!(s.iter().all(|e| !e.running && e.started_ms.is_none()));
        // Running the first flips only its entry and stamps started_ms.
        t1.run(|| {
            let s = q.snapshot();
            let e1 = s.iter().find(|e| e.job_id == 1).unwrap();
            let e2 = s.iter().find(|e| e.job_id == 2).unwrap();
            assert!(e1.running && e1.started_ms.is_some());
            assert!(!e2.running && e2.started_ms.is_none());
        });
        // Dropping a ticket removes exactly its entry.
        drop(t1);
        assert_eq!(q.snapshot().iter().map(|e| e.job_id).collect::<Vec<_>>(), vec![2]);
        drop(t2);
        assert!(q.snapshot().is_empty());
    }

    #[test]
    fn queue_serializes_runs_one_at_a_time() {
        use std::sync::atomic::AtomicUsize;
        use std::thread;
        let q = OpencodeQueue::new();
        let live = AtomicUsize::new(0); // concurrent runs right now
        let peak = AtomicUsize::new(0); // max ever seen
        let (q, live, peak) = (&q, &live, &peak); // shared by reference across threads
        thread::scope(|sc| {
            for job in 0..4i64 {
                sc.spawn(move || {
                    let t = q.enter(job, "detail", "oc");
                    t.run(|| {
                        let c = live.fetch_add(1, Ordering::SeqCst) + 1;
                        peak.fetch_max(c, Ordering::SeqCst);
                        thread::sleep(Duration::from_millis(20));
                        live.fetch_sub(1, Ordering::SeqCst);
                    });
                });
            }
        });
        assert_eq!(peak.load(Ordering::SeqCst), 1, "only one opencode run at a time");
        assert!(q.snapshot().is_empty(), "all entries cleared after runs finish");
    }

    #[test]
    fn parse_servers_resolves_key_env_and_skips_missing() {
        let raw = "label=openrouter,base_url=https://openrouter.ai/api/v1/,model=g:free,api_key_env=OR_KEY,primary=true;\
                   label=nokey,base_url=https://x/v1,model=m,api_key_env=MISSING;\
                   label=httpnobase,model=m;\
                   label=oc,model=opencode/deepseek-v4-flash-free,kind=opencode;\
                   label=local,base_url=http://127.0.0.1:1/,model=m";
        let bs = parse_servers(raw, |k| (k == "OR_KEY").then(|| "sk-x".to_string()));
        assert_eq!(bs.len(), 3, "missing-key and base-less-HTTP backends are dropped");
        assert_eq!(bs[0].label, "openrouter");
        assert_eq!(bs[0].base_url, "https://openrouter.ai/api/v1"); // trailing slash trimmed
        assert_eq!(bs[0].model, "g:free");
        assert_eq!(bs[0].api_key.as_deref(), Some("sk-x"));
        assert!(bs[0].primary && bs[0].kind == BackendKind::OpenAi);
        assert_eq!(bs[1].label, "oc");
        assert!(bs[1].kind == BackendKind::Opencode && bs[1].base_url.is_empty());
        assert_eq!(bs[2].label, "local");
        assert!(bs[2].api_key.is_none() && !bs[2].primary);
    }

    #[test]
    fn parse_servers_tolerates_garbage() {
        assert!(parse_servers("", |_| None).is_empty());
        assert!(parse_servers("nonsense-without-fields", |_| None).is_empty());
    }

    #[test]
    fn curate_strips_code_fences_keeps_prose() {
        let md = "---\ncommit: v6.19\n---\n# Title\nProse line.\n```module:x.c\nint main(){ return 1; }\n```\nMore prose.\n";
        let c = curate_bundle(md);
        assert!(c.contains("Title"), "{c}");
        assert!(c.contains("commit: v6.19"));
        assert!(c.contains("More prose"));
        assert!(!c.contains("int main"), "code should be stripped: {c}");
    }

    #[test]
    fn curate_issues_handles_empty_and_garbage() {
        assert!(curate_issues("[]").contains("No errors"));
        assert!(curate_issues("not json").contains("No errors"));
    }

    #[test]
    fn curate_issues_flattens_blocks() {
        // Must match collect_issues' real shape: [{file, blocks:[{head, trace}]}].
        let j = r#"[{"file":"baseline/console.log","blocks":[{"head":["BUG: KASAN: slab-use-after-free in reader_fn"],"trace":[]}]}]"#;
        let c = curate_issues(j);
        assert!(c.contains("Issues found in logs"), "{c}");
        assert!(c.contains("KASAN"));
        assert!(c.contains("baseline/console.log"), "line is tagged with its file: {c}");
        // The legacy {file, lines:[...]} shape must NOT silently report 'no errors'
        // via the wrong key — with the real shape it surfaces the issue.
    }

    #[test]
    fn two_words_trims_and_caps() {
        assert_eq!(two_words("Memory Leak"), "Memory Leak");
        assert_eq!(two_words("  \"Use After\" Free  "), "Use After");
        assert_eq!(two_words("net/sched: qdisc"), "net/sched qdisc");
        assert_eq!(two_words("OneWord"), "OneWord");
        assert_eq!(two_words("..."), "");
    }

    #[test]
    fn cap_chars_is_utf8_safe() {
        assert_eq!(cap_chars("abc", 10), "abc");
        let capped = cap_chars("ααααα", 3); // multibyte; must not panic
        assert!(capped.chars().count() <= 4);
    }

    #[test]
    fn end_context_labels_attachments_and_falls_back_to_baseline() {
        let dir = std::env::temp_dir().join(format!("mk_end_ctx_{}", std::process::id()));
        std::fs::create_dir_all(dir.join("baseline")).unwrap();
        std::fs::write(dir.join("compile.log"), "gcc: fatal error xyz").unwrap();
        // console.log only exists per-variant (compare job) -> baseline fallback.
        std::fs::write(dir.join("baseline").join("console.log"), "BUG: KASAN: slab-use-after-free").unwrap();
        std::fs::write(dir.join("run.log"), "ssh: running repro").unwrap();
        let c = curate_end_context("# bug\nProse only.\n", &dir, 8000);
        assert!(c.contains("reproducer you are using"), "{c}");
        assert!(c.contains("compiles the kernel") && c.contains("fatal error xyz"));
        assert!(c.contains("dmesg from the serial port") && c.contains("KASAN"), "baseline fallback: {c}");
        assert!(c.contains("orchestrator log") && c.contains("SSH") && c.contains("running repro"));
        std::fs::remove_dir_all(&dir).ok();
    }
}
