//! On-demand LKML browse: list recent patch cover letters on a public-inbox list,
//! paginated. No polling — the UI calls `GET /api/lkml/patches?list=…&skip=…` when the
//! user picks a list / clicks "load more", and opens the chosen cover letter as a
//! reproducer (injecting a `thread:` key client-side; see ui/src/bundle.ts:upsertMeta).
//!
//! lore.kernel.org sits behind Anubis bot-protection: `new.atom` is hard-capped at ~25
//! with no pagination, and search/HTML/raw/mbox are blocked. The reachable bulk source
//! is each list's public-inbox **git mirror** (bot UAs pass). The actual fetch + RFC822
//! parsing lives in `lkml-browse.py` (Python's `email` module handles MIME/encodings
//! robustly); this module just shells to it and caches each list's latest git epoch so
//! "load more" doesn't re-probe.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Mutex, OnceLock};

/// list name -> latest git epoch, learned from the first browse and reused so paging
/// doesn't re-probe lore each time. Epochs roll over rarely (≈once an archive fills).
fn epoch_cache() -> &'static Mutex<HashMap<String, u32>> {
    static C: OnceLock<Mutex<HashMap<String, u32>>> = OnceLock::new();
    C.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Run `lkml-browse.py <list> <skip> <page> <epoch>` and return its JSON
/// (`{patches,more,next,epoch}`) verbatim for the API to pass through. The cached epoch
/// is passed when known (`-1` tells the script to probe); the script echoes the epoch it
/// used, which we then cache.
pub async fn list_patches(repo: &Path, list: &str, skip: u32) -> anyhow::Result<String> {
    let cached = epoch_cache().lock().unwrap().get(list).copied();
    let epoch_arg = cached.map_or_else(|| "-1".to_string(), |e| e.to_string());
    let out = tokio::process::Command::new("python3")
        .arg(repo.join("lkml-browse.py"))
        .arg(list)
        .arg(skip.to_string())
        .arg("50")
        .arg(epoch_arg)
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("lkml-browse.py failed: {}", String::from_utf8_lossy(&out.stderr));
    }
    let json = String::from_utf8_lossy(&out.stdout).into_owned();
    if cached.is_none() {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&json) {
            if let Some(e) = v.get("epoch").and_then(serde_json::Value::as_u64) {
                epoch_cache().lock().unwrap().insert(list.to_string(), e as u32);
            }
        }
    }
    Ok(json)
}
