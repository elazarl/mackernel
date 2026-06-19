//! LKML monitor: poll public-inbox Atom feeds, detect cover letters whose
//! frontmatter matches the reproducer spec, and record them as runnable candidates.
//!
//! Disabled unless `MK_LKML_LISTS` is set (see sched::Cfg). For each watched list we
//! fetch `<base>/<list>/new.atom`, and for every message we haven't evaluated before
//! (tracked in the `lkml_seen` table) we fetch its raw text and check for our
//! metadata block. A match becomes a `candidates` row — listed on the site with a
//! Run button; nothing builds until a human clicks it (see main.rs:run_candidate).
//!
//! The thread's patch series is applied at run time by run-kernel.py: we inject a
//! `thread:` key (the cover-letter permalink) into the stored bundle so it `git am`s
//! the `[PATCH n/m]` mails of the thread on top of the bundle's `commit`.

use std::time::Duration;

use tracing::{info, warn};

use crate::AppState;

/// Cap on entries processed per (list, poll) so a backfilled feed can't wedge a poll.
const MAX_PER_POLL: usize = 200;
/// Metadata keys that make a `---` block count as our frontmatter (mirror of
/// run-kernel.py's META_KEYS).
const RECOGNIZED: &[&str] = &["url", "commit", "patch", "arch", "thread"];

/// Background loop: every `cfg.lkml_poll_secs`, poll each watched list. Only spawned
/// when at least one list is configured.
pub async fn monitor_loop(st: AppState) {
    let lists = st.cfg.lkml_lists.clone();
    let period = st.cfg.lkml_poll_secs.max(30);
    info!("lkml monitor: watching {:?} every {}s (base {})", lists, period, st.cfg.lkml_base);
    let mut tick = tokio::time::interval(Duration::from_secs(period));
    loop {
        tick.tick().await;
        for list in &lists {
            if let Err(e) = poll_list(&st, list).await {
                warn!("lkml monitor: list {list} poll failed: {e}");
            }
        }
    }
}

async fn poll_list(st: &AppState, list: &str) -> anyhow::Result<()> {
    let feed_url = format!("{}/{}/new.atom", st.cfg.lkml_base.trim_end_matches('/'), list);
    let feed = curl_text(&feed_url).await?;
    let mut added = 0u32;
    for (permalink, title) in parse_atom(&feed).into_iter().take(MAX_PER_POLL) {
        let msgid = msgid_from(&permalink);
        if msgid.is_empty() || st.db.lkml_seen(&msgid)? {
            continue;
        }
        // Mark seen before fetching: a fetch failure shouldn't make us retry the same
        // message every poll forever.
        st.db.lkml_mark_seen(&msgid, list, crate::now_ms())?;
        let raw_url = format!("{}raw", ensure_trailing_slash(&permalink));
        let raw = match curl_text(&raw_url).await {
            Ok(t) => t,
            Err(e) => {
                warn!("lkml: fetch {raw_url} failed: {e}");
                continue;
            }
        };
        if !has_frontmatter(&raw) {
            continue;
        }
        // Inject the thread permalink so run-kernel.py applies the whole series.
        let bundle = upsert_thread(&raw, &permalink);
        st.db.add_candidate(&msgid, list, &title, &permalink, &bundle, crate::now_ms())?;
        info!("lkml: new candidate from {list}: {title} <{permalink}>");
        added += 1;
    }
    if added > 0 {
        st.bus.publish_global(serde_json::json!({ "kind": "candidates" }).to_string());
    }
    Ok(())
}

/// Fetch a URL as text via curl (the server already shells out to git/python/curl;
/// this avoids pulling in an HTTP-client dependency). 30s cap; non-2xx -> Err.
async fn curl_text(url: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("curl")
        .args(["-LfsS", "--max-time", "30", url])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("curl {url} exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The message id is the last path segment of a lore permalink
/// (`https://lore.kernel.org/<list>/<msgid>/`) — globally unique, so it dedups
/// across lists too.
fn msgid_from(permalink: &str) -> String {
    permalink.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string()
}

fn ensure_trailing_slash(url: &str) -> String {
    if url.ends_with('/') { url.to_string() } else { format!("{url}/") }
}

// --- Atom feed parsing ------------------------------------------------------
// public-inbox emits a regular Atom feed: one <entry> per message, each with a
// <link href="<permalink>"/> and a <title>. We scan for those rather than depend on
// a full XML parser.

/// Extract `(permalink, title)` per `<entry>` in the feed.
fn parse_atom(feed: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    for chunk in feed.split("<entry").skip(1) {
        let entry = chunk.split("</entry>").next().unwrap_or(chunk);
        if let Some(href) = find_attr(entry, "<link", "href") {
            let title = find_tag_text(entry, "title").unwrap_or_default();
            out.push((href, title));
        }
    }
    out
}

/// Value of `attr="..."` inside the first `tag` element of `hay`.
fn find_attr(hay: &str, tag: &str, attr: &str) -> Option<String> {
    let tstart = hay.find(tag)?;
    let tag_slice = &hay[tstart..];
    let tag_slice = &tag_slice[..tag_slice.find('>')?];
    let key = format!("{attr}=\"");
    let astart = tag_slice.find(&key)? + key.len();
    let rest = &tag_slice[astart..];
    Some(xml_unescape(&rest[..rest.find('"')?]))
}

/// Text content of the first `<tag ...>...</tag>` in `hay`.
fn find_tag_text(hay: &str, tag: &str) -> Option<String> {
    let start = hay.find(&format!("<{tag}"))?;
    let after = &hay[start..];
    let after = &after[after.find('>')? + 1..];
    let end = after.find(&format!("</{tag}>"))?;
    Some(xml_unescape(after[..end].trim()))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
        .replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

// --- frontmatter detection (mirror of run-kernel.py:parse_bundle) ------------

/// True if `text` contains a `---`-delimited metadata block (column 0, outside a
/// fenced code block) whose lines are all `key: value` (or blank) and include at
/// least one recognized key. Mirrors the spec's parser so detection matches what
/// run-kernel.py will actually parse.
fn has_frontmatter(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    let fenced = fence_mask(&lines);
    let dashes: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].trim_end() == "---" && !fenced[i])
        .collect();
    for w in dashes.windows(2) {
        if let Some(keys) = parse_kv_keys(&lines[w[0] + 1..w[1]]) {
            if keys.iter().any(|k| RECOGNIZED.contains(&k.as_str())) {
                return true;
            }
        }
    }
    false
}

/// Per-line "inside a ``` fence" mask, matching parse_bundle: a line opening with
/// ``` (3+ backticks) starts a fence, closed by the next bare ```-only line.
fn fence_mask<S: AsRef<str>>(lines: &[S]) -> Vec<bool> {
    let mut mask = vec![false; lines.len()];
    let mut i = 0;
    while i < lines.len() {
        if lines[i].as_ref().starts_with("```") {
            let start = i;
            i += 1;
            while i < lines.len() && !is_fence_close(lines[i].as_ref()) {
                i += 1;
            }
            let end = i.min(lines.len() - 1);
            for m in mask.iter_mut().take(end + 1).skip(start) {
                *m = true;
            }
            i += 1;
        } else {
            i += 1;
        }
    }
    mask
}

fn is_fence_close(l: &str) -> bool {
    let t = l.trim_end();
    t.starts_with("```") && t.trim_start_matches('`').is_empty()
}

/// Parse `key: value` lines; return the keys, or None if any non-blank line isn't a
/// valid `key: value` (so a thematic-break `---` block of prose is rejected).
fn parse_kv_keys(block: &[&str]) -> Option<Vec<String>> {
    let mut keys = Vec::new();
    for ln in block {
        let s = ln.trim();
        if s.is_empty() {
            continue;
        }
        let (k, _v) = s.split_once(':')?;
        let k = k.trim();
        let mut chars = k.chars();
        let first_ok = chars.next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
        let rest_ok = k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
        if !first_ok || !rest_ok {
            return None;
        }
        keys.push(k.to_string());
    }
    Some(keys)
}

// --- thread-key injection (mirror of bundle.ts:upsertMeta) -------------------

/// Set `thread: <url>` in the bundle's frontmatter: update it in place if present,
/// insert into the existing block otherwise, or prepend a new block if there is none
/// (qualifying bundles always have a block, so the prepend branch is a safety net).
fn upsert_thread(text: &str, url: &str) -> String {
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let kv = format!("thread: {url}");
    if let Some((open, close)) = frontmatter_range(&lines) {
        for i in open + 1..close {
            if is_key(&lines[i], "thread") {
                lines[i] = kv;
                return lines.join("\n");
            }
        }
        lines.insert(close, kv);
        return lines.join("\n");
    }
    format!("---\n{kv}\n---\n\n{text}")
}

/// `(open, close)` line indices of the first column-0 `---…---` block outside any
/// fence, or None.
fn frontmatter_range(lines: &[String]) -> Option<(usize, usize)> {
    let fenced = fence_mask(lines);
    let dashes: Vec<usize> = (0..lines.len())
        .filter(|&i| lines[i].trim_end() == "---" && !fenced[i])
        .collect();
    if dashes.len() >= 2 {
        Some((dashes[0], dashes[1]))
    } else {
        None
    }
}

/// True if `line` is a `key:` entry for `key` (e.g. `thread: ...`).
fn is_key(line: &str, key: &str) -> bool {
    line.trim().split_once(':').map(|(k, _)| k.trim() == key).unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_spec_frontmatter() {
        let cover = "Subject line\n\n---\ncommit: v6.12\n---\n\n```init:init.sh\n#!/bin/sh\n```\n";
        assert!(has_frontmatter(cover));
        let with_thread = "---\nthread: x\narch: x86_64\n---\n";
        assert!(has_frontmatter(with_thread));
    }

    #[test]
    fn ignores_non_frontmatter_dashes() {
        // A git cover letter's scissors/diffstat `---` separators are not a kv block.
        let plain = "Some prose\n\n---\n drivers/x.c | 2 +-\n 1 file changed\n---\n";
        assert!(!has_frontmatter(plain));
        // `---` inside a fenced code block (a doc example) must be ignored.
        let fenced = "```\n---\ncommit: v6.12\n---\n```\n";
        assert!(!has_frontmatter(fenced));
        // No metadata block at all.
        assert!(!has_frontmatter("just a normal email body\nwith no metadata\n"));
    }

    #[test]
    fn upserts_thread_into_existing_block() {
        let b = "---\ncommit: v6.12\n---\n\nbody\n";
        let out = upsert_thread(b, "https://lore.kernel.org/all/abc/");
        assert!(out.contains("thread: https://lore.kernel.org/all/abc/"));
        assert!(out.contains("commit: v6.12"));
        // Re-upsert replaces rather than duplicates.
        let again = upsert_thread(&out, "https://lore.kernel.org/all/xyz/");
        assert_eq!(again.matches("thread:").count(), 1);
        assert!(again.contains("xyz"));
    }

    #[test]
    fn parses_atom_entries() {
        let feed = r#"<feed>
          <entry><title>[PATCH 0/2] fix</title>
            <link href="https://lore.kernel.org/lkml/msg1@x/"/></entry>
          <entry><title>Re: hi &amp; bye</title>
            <link href="https://lore.kernel.org/lkml/msg2@x/"/></entry>
        </feed>"#;
        let entries = parse_atom(feed);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].0, "https://lore.kernel.org/lkml/msg1@x/");
        assert_eq!(entries[0].1, "[PATCH 0/2] fix");
        assert_eq!(entries[1].1, "Re: hi & bye");
        assert_eq!(msgid_from(&entries[1].0), "msg2@x");
    }
}
