//! CPU-only LLM summarizer for reproducer jobs.
//!
//! Produces four outputs per job against the quantized Phi-3.5-mini GGUF (the
//! "pi3"/Phi-3 model, downloaded once and cached):
//!   - a short **title**,
//!   - a one-sentence **reproducer** summary (at job start, bundle only),
//!   - a one-sentence **result** summary (at job end, + run output),
//!   - a Markdown **detail** ("why it failed", reading the bundle + all logs;
//!     a GitHub-flavored Markdown doc with Summary/Root cause/Evidence sections).
//! Set `MK_SUMMARY_DISABLE=1` to turn the feature off entirely.
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

use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use serde_json::json;
use tracing::warn;

const SYS_TITLE: &str = "You name Linux kernel bug reproducers. Reply with exactly two words — a terse title — and nothing else. No punctuation, no preamble.";
const SYS_REPRO: &str = "You summarize Linux kernel bug reproducers. The job has only just started and has no results yet. Reply with exactly one short sentence describing what the reproducer tests. No preamble.";
const SYS_RESULT: &str = "You summarize Linux kernel bug reproducer runs. Reply with exactly one short sentence and no preamble describing what actually happened on this run — whether it reproduced and the outcome.";
const SYS_DETAIL: &str = "Read the reproducer text and the result and summarize this job, whether it succeeded or failed. Reply ONLY with concise GitHub-flavored Markdown and no preamble (do not wrap the whole thing in a code fence). Use exactly these three sections: a `## Summary` heading followed by one sentence on what the reproducer tested and how the run ended; a `## Analysis` heading followed by one paragraph on why it ended that way (the root cause if it failed, or what the successful run demonstrated); and a `## Evidence` heading followed by a single fenced code block (open and close it with a line of three backticks) containing the most relevant verbatim log lines, one per line — no bullets and no inline backticks.";

const REPEAT_PENALTY: f32 = 1.1;
const REPEAT_LAST_N: usize = 64;

/// Extra `max_tokens` for remote backends. Many free remote models (e.g. gpt-oss)
/// are *reasoning* models that spend the budget thinking before emitting any
/// `content` (the only part we keep), so a tight cap yields an empty answer. The
/// local non-reasoning model keeps its tight per-field caps.
const REMOTE_REASONING_HEADROOM: usize = 1024;

const GGUF_REPO: &str = "bartowski/Phi-3.5-mini-instruct-GGUF";
const GGUF_FILE: &str = "Phi-3.5-mini-instruct-Q4_K_M.gguf";
/// Human-readable model label, surfaced in logs and `/api/summarizer`.
pub const LABEL: &str = "phi3.5-mini";

/// Total context window. llama-server SPLITS this across the `--parallel` slots, so
/// with 2 slots each gets CTX_SIZE/2. The `detail` prompt (bundle + ~10k chars of
/// curated logs ≈ 3-4k tokens) plus 400 generated must fit one slot, so 16384 → 8192
/// per slot. (At 8192 total, the 4096/slot overflowed and `detail` was rejected.)
const CTX_SIZE: usize = 16384;
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

/// One OpenAI-compatible summary backend: a base URL, the model name to request,
/// an optional bearer key, and whether it's the `primary` (its output fills the
/// legacy per-field columns and streams live to the UI). The local llama-server is
/// just one backend (no key); remote ones come from `MK_OPENAI_SERVERS`.
#[derive(Clone)]
pub struct Backend {
    pub label: String,
    pub base_url: String,
    pub api_key: Option<String>,
    pub model: String,
    pub primary: bool,
}

/// Owns the optional local `llama-server` subprocess (killed on drop) plus the set
/// of backends and a shared blocking HTTP client. `Send + Sync` (the `Child` lives
/// behind a `Mutex`), so it slots into the existing `Arc<OnceLock<Arc<Summarizer>>>`.
pub struct Summarizer {
    child: Option<Mutex<Child>>,
    client: reqwest::blocking::Client,
    backends: Vec<Backend>,
    mem_bytes: u64,
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
        // to remote-only rather than disabling summaries entirely.
        let mut child: Option<Mutex<Child>> = None;
        match spawn_local_server() {
            Ok((c, port)) => {
                child = Some(Mutex::new(c));
                backends.push(Backend {
                    label: LABEL.to_string(),
                    base_url: format!("http://127.0.0.1:{port}"),
                    api_key: None,
                    model: LABEL.to_string(),
                    primary: false,
                });
            }
            Err(e) if !backends.is_empty() => {
                warn!("local llama-server unavailable, using remote backends only: {e:#}");
            }
            Err(e) => return Err(e).context("start local llama-server (no remote backends configured)"),
        }

        // Exactly one primary: honor a configured one, else the first backend.
        if !backends.iter().any(|b| b.primary) {
            backends[0].primary = true;
        }

        let mut s = Self { child, client, backends, mem_bytes: 0 };
        // Warm up the local server and measure its RSS for /api/summarizer.
        if let Some(local) = s.backends.iter().find(|b| b.label == LABEL).cloned() {
            let _ = s.generate(&local, SYS_TITLE, "ping", 1, false, &|_| {});
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

    /// Terse two-word title for the job, from the bundle alone (job start).
    pub fn title(&self, b: &Backend, bundle_md: &str, on_tok: &dyn Fn(u32)) -> Result<(String, GenStats)> {
        let user = curate_bundle(bundle_md);
        let (raw, stats) = self.generate(b, SYS_TITLE, &user, 8, false, on_tok)?;
        Ok((two_words(&raw), stats))
    }

    /// One sentence on what the reproducer tests, from the bundle alone (job start).
    pub fn summarize_repro(&self, b: &Backend, bundle_md: &str, on_tok: &dyn Fn(u32)) -> Result<(String, GenStats)> {
        let user = curate_bundle(bundle_md);
        self.generate(b, SYS_REPRO, &user, 64, false, on_tok)
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
    ) -> Result<(String, GenStats)> {
        let exit = exit_code
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into());
        let user = format!(
            "{}\n\nRun result: outcome={outcome}, exit_code={exit}.\n{}",
            curate_bundle(bundle_md),
            curate_issues(issues_json),
        );
        self.generate(b, SYS_RESULT, &user, 96, false, on_tok)
    }

    /// Markdown "why it failed", reading the bundle plus all labeled logs (job end).
    /// Returns GitHub-flavored Markdown (a `## Summary` / `## Root cause` /
    /// `## Evidence` document) as a string; the DB stores it verbatim.
    pub fn detail(&self, b: &Backend, bundle_md: &str, logs_dir: &Path, on_tok: &dyn Fn(u32)) -> Result<(String, GenStats)> {
        let user = format!("{}\n\n{}", curate_bundle(bundle_md), curate_logs(logs_dir));
        self.generate(b, SYS_DETAIL, &user, 400, false, on_tok)
    }

    /// One deterministic (greedy) completion via the OpenAI chat API, streamed so we
    /// can report the live token count (`on_tok`, throttled) and measure wall time.
    /// The server applies the model's chat template, so system/user go as messages
    /// rather than a hand-rolled prompt string.
    fn generate(
        &self,
        b: &Backend,
        sys: &str,
        user: &str,
        max_new: usize,
        json: bool,
        on_tok: &dyn Fn(u32),
    ) -> Result<(String, GenStats)> {
        // Reasoning remote models need headroom (see REMOTE_REASONING_HEADROOM).
        let max_tokens = if b.api_key.is_some() { max_new + REMOTE_REASONING_HEADROOM } else { max_new };
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
            .post(format!("{}/v1/chat/completions", b.base_url))
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
            let Some(data) = line.strip_prefix("data:") else { continue };
            let data = data.trim();
            if data.is_empty() || data == "[DONE]" {
                continue;
            }
            let Ok(chunk): Result<serde_json::Value, _> = serde_json::from_str(data) else { continue };
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
        let stats = GenStats { ms: started.elapsed().as_millis() as u64, tokens };
        Ok((text.trim().to_string(), stats))
    }

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
        "--host".to_string(), "127.0.0.1".to_string(),
        "--port".to_string(), port.to_string(),
        "--ctx-size".to_string(), CTX_SIZE.to_string(),
        "--parallel".to_string(), "2".to_string(),
        "--hf-repo".to_string(), GGUF_REPO.to_string(),
        "--hf-file".to_string(), GGUF_FILE.to_string(),
    ];
    // `MK_LLAMA_NICE` lowers the model's CPU priority (it's a best-effort secondary
    // once a remote backend is primary): run `nice -n N <bin> …` — a shell-out in
    // the same style as the `tar`/`ps` calls. Unset = normal priority.
    let nice = std::env::var("MK_LLAMA_NICE").ok().and_then(|n| n.parse::<i32>().ok());
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
    let mut child = cmd.spawn().with_context(|| format!("spawn {}", bin.display()))?;

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
/// (literal, discouraged), `primary` (true/1/yes). Example:
/// `label=openrouter,base_url=https://openrouter.ai/api/v1,model=x:free,api_key_env=OPENROUTER_API_KEY,primary=true`
/// Returns [] when the var is unset/blank.
fn parse_remote_backends() -> Vec<Backend> {
    let Some(raw) = std::env::var("MK_OPENAI_SERVERS").ok().filter(|s| !s.trim().is_empty()) else {
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
        for kv in entry.split(',') {
            let Some((k, v)) = kv.split_once('=') else { continue };
            let v = v.trim().to_string();
            match k.trim() {
                "label" => label = Some(v),
                "base_url" => base_url = Some(v.trim_end_matches('/').to_string()),
                "model" => model = Some(v),
                "api_key" => api_key = Some(v),
                "api_key_env" => api_key_env = Some(v),
                "primary" => primary = matches!(v.as_str(), "1" | "true" | "yes"),
                other => warn!("MK_OPENAI_SERVERS: ignoring unknown field '{other}'"),
            }
        }
        let (Some(label), Some(base_url), Some(model)) = (label, base_url, model) else {
            warn!("MK_OPENAI_SERVERS entry missing label/base_url/model; skipping: {entry}");
            continue;
        };
        let api_key = match api_key_env {
            Some(env_name) => match lookup(&env_name) {
                Some(k) => Some(k),
                None => { warn!("summary backend '{label}': {env_name} unset/empty; skipping"); continue; }
            },
            None => api_key,
        };
        out.push(Backend { label, base_url, api_key, model, primary });
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
    let target = if cfg!(windows) { "llama-server.exe" } else { "llama-server" };
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
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
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
        if let Some(pages) = s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()) {
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

/// Read each known log file, label it with context, and cap per-file + overall so the
/// detail prompt stays bounded (model prefill cost scales with prompt length).
/// ponytail: per-file ~2.5k chars, total ~10k; raise if detail JSON quality suffers.
fn curate_logs(logs_dir: &Path) -> String {
    const LOGS: &[(&str, &str)] = &[
        ("compile.log", "This is the kernel compilation through podman:"),
        ("dmesg.log", "This is the dmesg from the VM serial:"),
        ("console.log", "This is the raw VM serial console:"),
        ("exec.log", "This is the in-VM reproducer execution log:"),
        ("run.log", "This is the orchestrator (run-kernel.py) log:"),
        ("fetch.log", "This is the kernel source fetch log:"),
    ];
    let mut out = String::new();
    for (file, label) in LOGS {
        let Ok(content) = std::fs::read_to_string(logs_dir.join(file)) else { continue };
        let content = content.trim();
        if content.is_empty() {
            continue;
        }
        out.push_str(label);
        out.push('\n');
        out.push_str(&cap_chars(content, 2500));
        out.push_str("\n\n");
    }
    cap_chars(out.trim(), 10000)
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

/// `collect_issues` returns a JSON array `[{"file","lines":[...]}]`. Flatten to a short
/// plaintext block (capped) for the prompt, or a clear "no issues" note when empty.
fn curate_issues(issues_json: &str) -> String {
    let parsed: serde_json::Value =
        serde_json::from_str(issues_json).unwrap_or(serde_json::Value::Null);
    let mut lines: Vec<&str> = Vec::new();
    if let Some(arr) = parsed.as_array() {
        for section in arr {
            if let Some(ls) = section.get("lines").and_then(|l| l.as_array()) {
                for l in ls {
                    if let Some(s) = l.as_str() {
                        lines.push(s);
                    }
                }
            }
        }
    }
    if lines.is_empty() {
        return "No errors or sanitizer reports were found in the logs.".to_string();
    }
    let mut s = String::from("Issues found in logs:\n");
    for l in lines.iter().take(40) {
        s.push_str(l.trim());
        s.push('\n');
    }
    cap_chars(s.trim(), 1200)
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
    fn parse_servers_resolves_key_env_and_skips_missing() {
        let raw = "label=openrouter,base_url=https://openrouter.ai/api/v1/,model=g:free,api_key_env=OR_KEY,primary=true;\
                   label=nokey,base_url=https://x/v1,model=m,api_key_env=MISSING;\
                   label=local,base_url=http://127.0.0.1:1/,model=m";
        let bs = parse_servers(raw, |k| (k == "OR_KEY").then(|| "sk-x".to_string()));
        assert_eq!(bs.len(), 2, "the missing-key backend is dropped");
        assert_eq!(bs[0].label, "openrouter");
        assert_eq!(bs[0].base_url, "https://openrouter.ai/api/v1"); // trailing slash trimmed
        assert_eq!(bs[0].model, "g:free");
        assert_eq!(bs[0].api_key.as_deref(), Some("sk-x"));
        assert!(bs[0].primary);
        assert_eq!(bs[1].label, "local");
        assert!(bs[1].api_key.is_none() && !bs[1].primary);
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
    fn curate_issues_flattens_lines() {
        let j = r#"[{"file":"dmesg.log","lines":["BUG: KASAN: slab-use-after-free","x"]}]"#;
        let c = curate_issues(j);
        assert!(c.contains("Issues found in logs"));
        assert!(c.contains("KASAN"));
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
    fn curate_logs_labels_and_caps() {
        let dir = std::env::temp_dir().join(format!("mk_curate_logs_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("compile.log"), "gcc: fatal error xyz").unwrap();
        std::fs::write(dir.join("dmesg.log"), "d".repeat(9000)).unwrap();
        std::fs::write(dir.join("console.log"), "   ").unwrap(); // blank → skipped
        let c = curate_logs(&dir);
        assert!(c.contains("kernel compilation through podman"), "{c}");
        assert!(c.contains("dmesg from the VM serial"));
        assert!(c.contains("fatal error xyz"));
        assert!(!c.contains("raw VM serial console"), "blank log should be skipped");
        assert!(c.chars().count() <= 10_001, "overall cap");
        std::fs::remove_dir_all(&dir).ok();
    }
}
