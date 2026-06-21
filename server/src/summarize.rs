//! In-process, CPU-only LLM summarizer for reproducer jobs.
//!
//! Produces four outputs per job via `candle` against the quantized Phi-3.5-mini GGUF
//! (downloaded once, cached under `~/.cache/huggingface`, loaded at boot):
//!   - a short **title**,
//!   - a one-sentence **reproducer** summary (at job start, bundle only),
//!   - a one-sentence **result** summary (at job end, + run output),
//!   - a two-paragraph **detail** ("why it failed", reading the bundle + all logs).
//! Set `MK_SUMMARY_DISABLE=1` to turn the feature off entirely.
//!
//! Each output has its OWN model instance (`title_model`/`repro_model`/`result_model`/
//! `detail_model`), so the two generations of a stage run concurrently instead of
//! contending on one lock. The instances share weight tensors by Arc (cold-cloned at
//! load, before any KV cache is allocated) — ~1x weights, but each carries its own
//! full-context KV cache, which is why `mem_bytes` is tracked (see `load`).
//!
//! candle's quantized CPU prefill does NOT parallelize across cores, so latency scales
//! with prompt length — hence the aggressive curation in `curate_*`.

use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_phi3::ModelWeights as Model;
use candle_transformers::utils::apply_repeat_penalty;
use tokenizers::Tokenizer;

const SYS_TITLE: &str = "You name Linux kernel bug reproducers. Reply with exactly two words — a terse title — and nothing else. No punctuation, no preamble.";
const SYS_REPRO: &str = "You summarize Linux kernel bug reproducers. The job has only just started and has no results yet. Reply with exactly one short sentence describing what the reproducer tests. No preamble.";
const SYS_RESULT: &str = "You summarize Linux kernel bug reproducer runs. Reply with exactly one short sentence and no preamble describing what actually happened on this run — whether it reproduced and the outcome.";
const SYS_DETAIL: &str = "Read the reproducer text and the result. Explain in two paragraphs what the failure is, why it happened, and quote excerpts from the logs. No preamble.";

const REPEAT_PENALTY: f32 = 1.1;
const REPEAT_LAST_N: usize = 64;

const GGUF_REPO: &str = "bartowski/Phi-3.5-mini-instruct-GGUF";
const GGUF_FILE: &str = "Phi-3.5-mini-instruct-Q4_K_M.gguf";
const TOKENIZER_REPO: &str = "microsoft/Phi-3.5-mini-instruct";
/// Turn-ending special tokens, in priority order.
const EOS_NAMES: &[&str] = &["<|endoftext|>", "<|end|>"];
/// Human-readable model label, surfaced in logs and `/api/summarizer`.
pub const LABEL: &str = "phi3.5-mini";

/// Two model instances sharing weight tensors by Arc (cold-cloned in `load`) but each
/// with its own KV cache, so a stage's two outputs generate concurrently. `model_a`
/// serves title (start) + result (end); `model_b` serves repro (start) + detail (end).
/// Profiling showed the per-model KV cache (~3.9GB), not the shared ~2.8GB weights,
/// dominates RAM — so two caches instead of four roughly halves resident memory.
/// `mem_bytes` is the measured RSS of weights + the two KV caches.
pub struct Summarizer {
    model_a: Mutex<Model>,
    model_b: Mutex<Model>,
    tokenizer: Tokenizer,
    device: Device,
    eos: Vec<u32>,
    mem_bytes: u64,
}

impl Summarizer {
    /// True unless explicitly disabled via `MK_SUMMARY_DISABLE`.
    pub fn enabled() -> bool {
        !matches!(
            std::env::var("MK_SUMMARY_DISABLE").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE")
        )
    }

    /// Download (if needed) and load the model, then cold-clone it into four
    /// instances. Blocking — call from a blocking thread. First call may download
    /// ~2.5 GB; later calls hit the `~/.cache/huggingface` cache and are fast.
    pub fn load() -> Result<Self> {
        let api = hf_hub::api::sync::Api::new().context("init hf-hub api")?;
        let gguf_path = api
            .model(GGUF_REPO.to_string())
            .get(GGUF_FILE)
            .with_context(|| format!("download {GGUF_REPO}/{GGUF_FILE}"))?;
        let tok_path = api
            .model(TOKENIZER_REPO.to_string())
            .get("tokenizer.json")
            .context("download tokenizer.json")?;

        let tokenizer = Tokenizer::from_file(&tok_path).map_err(anyhow::Error::msg)?;
        let device = Device::Cpu;

        let rss0 = self_rss();
        let mut fd = std::fs::File::open(&gguf_path)?;
        let content = gguf_file::Content::read(&mut fd).map_err(|e| e.with_path(&gguf_path))?;
        let model = Model::from_gguf(false, content, &mut fd, &device)?;
        // Cold-clone once: the clone shares weight tensors by Arc (the ~2.8GB weights
        // cost ~1x) and gets its own empty KV cache. NEVER clone a warmed model — its
        // KV buffer is Arc-shared and writes would corrupt across clones (candle_nn::kv_cache).
        let vocab = tokenizer.get_vocab(true);
        let eos: Vec<u32> = EOS_NAMES.iter().filter_map(|n| vocab.get(*n).copied()).collect();

        let mut s = Self {
            model_a: Mutex::new(model.clone()),
            model_b: Mutex::new(model),
            tokenizer,
            device,
            eos,
            mem_bytes: 0,
        };
        // Warm both so their KV caches allocate up front; the RSS delta then captures
        // weights + the two caches. The caches dominate RAM, so two instead of four
        // halves it (see the Summarizer doc comment).
        for m in [&s.model_a, &s.model_b] {
            let _ = s.generate(m, &s.format_prompt("Reply with OK.", "ping"), 1);
        }
        s.mem_bytes = self_rss().saturating_sub(rss0);
        Ok(s)
    }

    /// Measured RAM of the four model instances (weights + KV caches), in bytes.
    pub fn memory_bytes(&self) -> u64 {
        self.mem_bytes
    }

    /// Terse two-word title for the job, from the bundle alone (job start).
    pub fn title(&self, bundle_md: &str) -> Result<String> {
        let user = curate_bundle(bundle_md);
        let raw = self.generate(&self.model_a, &self.format_prompt(SYS_TITLE, &user), 8)?;
        Ok(two_words(&raw))
    }

    /// One sentence on what the reproducer tests, from the bundle alone (job start).
    pub fn summarize_repro(&self, bundle_md: &str) -> Result<String> {
        let user = curate_bundle(bundle_md);
        self.generate(&self.model_b, &self.format_prompt(SYS_REPRO, &user), 64)
    }

    /// One sentence on what happened on this run: bundle + curated issues + outcome
    /// (job end).
    pub fn summarize_result(
        &self,
        bundle_md: &str,
        issues_json: &str,
        exit_code: Option<i64>,
        outcome: &str,
    ) -> Result<String> {
        let exit = exit_code
            .map(|e| e.to_string())
            .unwrap_or_else(|| "unknown".into());
        let user = format!(
            "{}\n\nRun result: outcome={outcome}, exit_code={exit}.\n{}",
            curate_bundle(bundle_md),
            curate_issues(issues_json),
        );
        self.generate(&self.model_a, &self.format_prompt(SYS_RESULT, &user), 96)
    }

    /// Two-paragraph "why it failed", reading the bundle plus all labeled logs (job end).
    pub fn detail(&self, bundle_md: &str, logs_dir: &Path) -> Result<String> {
        let user = format!("{}\n\n{}", curate_bundle(bundle_md), curate_logs(logs_dir));
        self.generate(&self.model_b, &self.format_prompt(SYS_DETAIL, &user), 400)
    }

    fn format_prompt(&self, sys: &str, user: &str) -> String {
        format!("<|system|>\n{sys}<|end|>\n<|user|>\n{user}<|end|>\n<|assistant|>\n")
    }

    /// Greedy generation (deterministic) with a light repeat penalty. Holds the given
    /// model's lock for the whole call; distinct models generate concurrently.
    fn generate(&self, model: &Mutex<Model>, prompt: &str, max_new: usize) -> Result<String> {
        let encoding = self
            .tokenizer
            .encode(prompt, true)
            .map_err(anyhow::Error::msg)?;
        let prompt_tokens = encoding.get_ids().to_vec();
        if prompt_tokens.is_empty() {
            anyhow::bail!("empty prompt");
        }

        let mut model = model.lock().unwrap();
        let mut logits_processor = LogitsProcessor::from_sampling(42, Sampling::ArgMax);

        // Prefill. Phi3 resets its KV cache when index_pos == 0, so each call starts fresh.
        let input = Tensor::new(prompt_tokens.as_slice(), &self.device)?.unsqueeze(0)?;
        let logits = forward(&mut model, &input, 0)?.squeeze(0)?;
        let mut next = logits_processor.sample(&logits)?;

        let n_prompt = prompt_tokens.len();
        let mut generated: Vec<u32> = Vec::with_capacity(max_new);
        for i in 0..max_new {
            if self.eos.contains(&next) {
                break;
            }
            generated.push(next);
            let input = Tensor::new(&[next], &self.device)?.unsqueeze(0)?;
            let logits = forward(&mut model, &input, n_prompt + i)?.squeeze(0)?;
            let logits = if generated.len() > 1 {
                let start = generated.len().saturating_sub(REPEAT_LAST_N);
                apply_repeat_penalty(&logits, REPEAT_PENALTY, &generated[start..])?
            } else {
                logits
            };
            next = logits_processor.sample(&logits)?;
        }
        drop(model);

        let text = self
            .tokenizer
            .decode(&generated, true)
            .map_err(anyhow::Error::msg)?;
        Ok(text.trim().to_string())
    }
}

fn forward(model: &mut Model, input: &Tensor, pos: usize) -> candle_core::Result<Tensor> {
    model.forward(input, pos)
}

/// Resident set size (bytes) of the current process. Linux reads `/proc/self/statm`;
/// elsewhere it shells out to `ps`. Best-effort: returns 0 on failure.
/// ponytail: assumes 4 KiB pages on Linux — fine for the x86_64 home box.
fn self_rss() -> u64 {
    #[cfg(target_os = "linux")]
    if let Ok(s) = std::fs::read_to_string("/proc/self/statm") {
        if let Some(pages) = s.split_whitespace().nth(1).and_then(|p| p.parse::<u64>().ok()) {
            return pages * 4096;
        }
    }
    std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .and_then(|s| s.trim().parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Read each known log file, label it with context, and cap per-file + overall so the
/// detail prompt stays bounded (candle CPU prefill cost scales with prompt length).
/// ponytail: per-file ~2.5k chars, total ~10k; raise if two-paragraph quality suffers.
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
/// and cap length (candle CPU prefill cost scales with prompt length).
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
