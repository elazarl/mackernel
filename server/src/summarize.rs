//! In-process, CPU-only LLM summarizer for reproducer jobs.
//!
//! Produces a short natural-language summary of a reproducer bundle — once when a
//! job starts (bundle only) and again when it finishes (bundle + run output). Runs
//! entirely in-process via `candle` against a quantized GGUF model; the model is
//! downloaded once (cached under `~/.cache/huggingface`) and loaded at boot.
//!
//! Two models are supported and selected with `MK_SUMMARY_MODEL`:
//!   - `phi3.5` (default) — Phi-3.5-mini-instruct, crisper summaries, ~50 s/summary on the
//!     home box (AMD Ryzen 7 5700U, CPU).
//!   - `qwen2.5` — Qwen2.5-1.5B-Instruct, ~3x faster (~15 s) and more verbose.
//! Set `MK_SUMMARY_DISABLE=1` to turn the feature off entirely.
//!
//! candle's quantized CPU prefill does NOT parallelize across cores, so latency scales
//! with prompt length — hence the aggressive curation in `curate_bundle`/`curate_issues`.

use std::sync::Mutex;

use anyhow::{Context, Result};
use candle_core::quantized::gguf_file;
use candle_core::{Device, Tensor};
use candle_transformers::generation::{LogitsProcessor, Sampling};
use candle_transformers::models::quantized_phi3::ModelWeights as Phi3;
use candle_transformers::models::quantized_qwen2::ModelWeights as Qwen2;
use candle_transformers::utils::apply_repeat_penalty;
use tokenizers::Tokenizer;

const SYS_START: &str = "You summarize Linux kernel bug reproducers. The job has only just started and has no results yet. Reply with exactly one short sentence describing what the reproducer tests. No preamble.";
const SYS_END: &str = "You summarize Linux kernel bug reproducers. Reply with exactly two short sentences and no preamble. Sentence 1: what the reproducer tests. Sentence 2: what happened on this run.";

const REPEAT_PENALTY: f32 = 1.1;
const REPEAT_LAST_N: usize = 64;

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum ModelKind {
    Phi35Mini,
    Qwen25_1_5B,
}

impl ModelKind {
    fn from_env() -> Self {
        match std::env::var("MK_SUMMARY_MODEL").unwrap_or_default().to_lowercase().as_str() {
            "qwen" | "qwen2.5" | "qwen2.5-1.5b" | "qwen25" => ModelKind::Qwen25_1_5B,
            _ => ModelKind::Phi35Mini,
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            ModelKind::Phi35Mini => "phi3.5-mini",
            ModelKind::Qwen25_1_5B => "qwen2.5-1.5b",
        }
    }

    /// (gguf repo, gguf filename).
    fn gguf(&self) -> (&'static str, &'static str) {
        match self {
            ModelKind::Phi35Mini => (
                "bartowski/Phi-3.5-mini-instruct-GGUF",
                "Phi-3.5-mini-instruct-Q4_K_M.gguf",
            ),
            ModelKind::Qwen25_1_5B => (
                "bartowski/Qwen2.5-1.5B-Instruct-GGUF",
                "Qwen2.5-1.5B-Instruct-Q4_K_M.gguf",
            ),
        }
    }

    fn tokenizer_repo(&self) -> &'static str {
        match self {
            ModelKind::Phi35Mini => "microsoft/Phi-3.5-mini-instruct",
            ModelKind::Qwen25_1_5B => "Qwen/Qwen2.5-1.5B-Instruct",
        }
    }

    /// Special tokens that end a turn, in priority order.
    fn eos_names(&self) -> &'static [&'static str] {
        match self {
            ModelKind::Phi35Mini => &["<|endoftext|>", "<|end|>"],
            ModelKind::Qwen25_1_5B => &["<|im_end|>", "<|endoftext|>"],
        }
    }
}

enum Model {
    Phi3(Phi3),
    Qwen2(Qwen2),
}

pub struct Summarizer {
    kind: ModelKind,
    model: Mutex<Model>,
    tokenizer: Tokenizer,
    device: Device,
    eos: Vec<u32>,
}

impl Summarizer {
    /// True unless explicitly disabled via `MK_SUMMARY_DISABLE`.
    pub fn enabled() -> bool {
        !matches!(
            std::env::var("MK_SUMMARY_DISABLE").ok().as_deref(),
            Some("1") | Some("true") | Some("TRUE")
        )
    }

    /// Download (if needed) and load the configured model. Blocking — call from a
    /// blocking thread. First call may download ~1-2.5 GB; later calls hit the
    /// `~/.cache/huggingface` cache and are fast.
    pub fn load() -> Result<Self> {
        let kind = ModelKind::from_env();
        let (repo, file) = kind.gguf();
        let api = hf_hub::api::sync::Api::new().context("init hf-hub api")?;
        let gguf_path = api
            .model(repo.to_string())
            .get(file)
            .with_context(|| format!("download {repo}/{file}"))?;
        let tok_path = api
            .model(kind.tokenizer_repo().to_string())
            .get("tokenizer.json")
            .context("download tokenizer.json")?;

        let tokenizer = Tokenizer::from_file(&tok_path).map_err(anyhow::Error::msg)?;
        let device = Device::Cpu;
        let mut fd = std::fs::File::open(&gguf_path)?;
        let content = gguf_file::Content::read(&mut fd).map_err(|e| e.with_path(&gguf_path))?;
        let model = match kind {
            ModelKind::Phi35Mini => {
                Model::Phi3(Phi3::from_gguf(false, content, &mut fd, &device)?)
            }
            ModelKind::Qwen25_1_5B => Model::Qwen2(Qwen2::from_gguf(content, &mut fd, &device)?),
        };

        let vocab = tokenizer.get_vocab(true);
        let eos: Vec<u32> = kind
            .eos_names()
            .iter()
            .filter_map(|n| vocab.get(*n).copied())
            .collect();

        Ok(Self { kind, model: Mutex::new(model), tokenizer, device, eos })
    }

    pub fn kind(&self) -> ModelKind {
        self.kind
    }

    /// Summary at job start: bundle only, no results yet (one sentence).
    pub fn summarize_start(&self, bundle_md: &str) -> Result<String> {
        let user = curate_bundle(bundle_md);
        self.generate(&self.format_prompt(SYS_START, &user), 64)
    }

    /// Summary at job end: bundle + curated run output (two sentences).
    pub fn summarize_end(
        &self,
        bundle_md: &str,
        issues_json: &str,
        exit_code: Option<i64>,
        outcome: &str,
    ) -> Result<String> {
        let exit = exit_code.map(|e| e.to_string()).unwrap_or_else(|| "unknown".into());
        let user = format!(
            "{}\n\nRun result: outcome={outcome}, exit_code={exit}.\n{}",
            curate_bundle(bundle_md),
            curate_issues(issues_json),
        );
        self.generate(&self.format_prompt(SYS_END, &user), 96)
    }

    fn format_prompt(&self, sys: &str, user: &str) -> String {
        match self.kind {
            ModelKind::Phi35Mini => {
                format!("<|system|>\n{sys}<|end|>\n<|user|>\n{user}<|end|>\n<|assistant|>\n")
            }
            ModelKind::Qwen25_1_5B => format!(
                "<|im_start|>system\n{sys}<|im_end|>\n<|im_start|>user\n{user}<|im_end|>\n<|im_start|>assistant\n"
            ),
        }
    }

    /// Greedy generation (deterministic) with a light repeat penalty. Holds the model
    /// lock for the whole call — summaries are serialized, which is fine at this volume.
    fn generate(&self, prompt: &str, max_new: usize) -> Result<String> {
        let encoding = self.tokenizer.encode(prompt, true).map_err(anyhow::Error::msg)?;
        let prompt_tokens = encoding.get_ids().to_vec();
        if prompt_tokens.is_empty() {
            anyhow::bail!("empty prompt");
        }

        let mut model = self.model.lock().unwrap();
        let mut logits_processor = LogitsProcessor::from_sampling(42, Sampling::ArgMax);

        // Prefill. Phi3 resets its KV cache when index_pos == 0; Qwen2 needs an
        // explicit clear before reusing the loaded model for a fresh generation.
        if let Model::Qwen2(m) = &mut *model {
            m.clear_kv_cache();
        }
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

        let text = self.tokenizer.decode(&generated, true).map_err(anyhow::Error::msg)?;
        Ok(text.trim().to_string())
    }
}

fn forward(model: &mut Model, input: &Tensor, pos: usize) -> candle_core::Result<Tensor> {
    match model {
        Model::Phi3(m) => m.forward(input, pos),
        Model::Qwen2(m) => m.forward(input, pos),
    }
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
    let parsed: serde_json::Value = serde_json::from_str(issues_json).unwrap_or(serde_json::Value::Null);
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
    fn cap_chars_is_utf8_safe() {
        assert_eq!(cap_chars("abc", 10), "abc");
        let capped = cap_chars("ααααα", 3); // multibyte; must not panic
        assert!(capped.chars().count() <= 4);
    }
}
