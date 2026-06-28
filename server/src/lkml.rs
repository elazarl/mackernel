//! On-demand LKML browse: list recent patch cover letters on a public-inbox list by
//! reading its Atom feed. No polling — the UI calls `GET /api/lkml/patches?list=…`
//! when the user picks a list, and opens the chosen cover letter as a reproducer
//! (injecting a `thread:` key client-side; see ui/src/bundle.ts:upsertMeta).
//!
//! lore.kernel.org sits behind Anubis bot-protection: only `.atom` feeds are reachable,
//! and only with a User-Agent (a bare request gets HTTP 403). HTML pages, per-message
//! `raw`, `t.mbox.gz`, and search are all blocked — so we read `<list>/new.atom`, whose
//! entries already carry the full message body inline (`<content>`), and never touch
//! those paths.

use serde::Serialize;

/// User-Agent for lore fetches. lore's Anubis bot-protection inverts the usual rule:
/// it *challenges* browser-looking UAs (`Mozilla/…`) and lets bot UAs through, so we
/// send a git-style UA — empirically the one that passes every lore path (.atom feeds
/// and manifest alike). curl's own default UA is rejected.
const LORE_UA: &str = "git/2.43";

/// A patch-series root (cover letter) or standalone patch found on a list, ready to be
/// opened as a reproducer. `body` is the message text (the cover letter == patch 0).
#[derive(Serialize)]
pub struct Patch {
    pub title: String,
    pub url: String,
    pub body: String,
}

/// Recent patch cover letters / standalone patches on `list`, newest first. Reads
/// `<base>/<list>/new.atom` and keeps series roots (`[PATCH 0/N]`, `[PATCH 1/1]`, or a
/// single `[PATCH]` with no n/m), dropping individual `n/m` patches and replies.
pub async fn list_patches(base: &str, list: &str) -> anyhow::Result<Vec<Patch>> {
    let feed_url = format!("{}/{}/new.atom", base.trim_end_matches('/'), list);
    let feed = curl_text(&feed_url).await?;
    let mut out = Vec::new();
    for e in parse_entries(&feed) {
        if !is_series_root(&e.title) {
            continue;
        }
        out.push(Patch { title: e.title, url: e.permalink, body: xhtml_to_text(&e.content) });
    }
    Ok(out)
}

/// Fetch a URL as text via curl with a User-Agent (Anubis returns 403 without one).
/// 30s cap; non-2xx -> Err. We shell out rather than pull in an HTTP-client dep, like
/// the rest of the server.
async fn curl_text(url: &str) -> anyhow::Result<String> {
    let out = tokio::process::Command::new("curl")
        .args(["-LfsS", "-A", LORE_UA, "--max-time", "30", url])
        .output()
        .await?;
    if !out.status.success() {
        anyhow::bail!("curl {url} exited {}: {}", out.status, String::from_utf8_lossy(&out.stderr));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

// --- Atom feed parsing ------------------------------------------------------
// public-inbox emits a regular Atom feed: one <entry> per message, each with a
// <link href="<permalink>"/>, a <title>, and a <content type="xhtml"> body. We scan
// for those rather than depend on a full XML parser.

struct Entry {
    permalink: String,
    title: String,
    /// Raw (still entity-escaped) XHTML of the message body.
    content: String,
}

fn parse_entries(feed: &str) -> Vec<Entry> {
    let mut out = Vec::new();
    for chunk in feed.split("<entry").skip(1) {
        let entry = chunk.split("</entry>").next().unwrap_or(chunk);
        let Some(permalink) = find_attr(entry, "<link", "href") else { continue };
        let title = find_tag_text(entry, "title").unwrap_or_default();
        // Body is left entity-escaped here; xhtml_to_text strips tags first, then
        // unescapes — so an entity-escaped `&lt;` in the body isn't mistaken for a tag.
        let content = tag_inner(entry, "content").unwrap_or_default().to_string();
        out.push(Entry { permalink, title, content });
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

/// Raw inner text of the first `<tag ...>...</tag>` in `hay` (no unescape).
fn tag_inner<'a>(hay: &'a str, tag: &str) -> Option<&'a str> {
    let start = hay.find(&format!("<{tag}"))?;
    let after = &hay[start..];
    let after = &after[after.find('>')? + 1..];
    let end = after.find(&format!("</{tag}>"))?;
    Some(&after[..end])
}

/// Unescaped text content of the first `<tag ...>...</tag>` in `hay`.
fn find_tag_text(hay: &str, tag: &str) -> Option<String> {
    tag_inner(hay, tag).map(|s| xml_unescape(s.trim()))
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"")
        .replace("&#39;", "'").replace("&apos;", "'").replace("&amp;", "&")
}

// --- cover-letter detection + body extraction --------------------------------

/// Is this subject a patch-series root (cover letter / standalone patch) rather than a
/// follow-on `n/m` patch or a reply? Keeps `[PATCH 0/N]`, `[PATCH 1/1]`, and any
/// `[PATCH …]` with no `n/m`. (ponytail: keys on `[PATCH`; misses `[RFC PATCH …]`
/// variants — broaden the marker if that matters.)
fn is_series_root(title: &str) -> bool {
    let t = title.trim_start();
    if t.starts_with("Re:") || t.starts_with("RE:") {
        return false;
    }
    let Some(p) = t.find("[PATCH") else { return false };
    let close = t[p..].find(']').map(|i| p + i).unwrap_or(t.len());
    let tag = &t[p..close];
    match patch_index(tag) {
        Some((n, m)) => n == 0 || (n == 1 && m == 1),
        None => true,
    }
}

/// The `(n, m)` from an `n/m` token inside a `[PATCH …]` tag, if any.
fn patch_index(tag: &str) -> Option<(u32, u32)> {
    for part in tag.split([' ', '[', ']']) {
        if let Some((a, b)) = part.split_once('/') {
            if let (Ok(n), Ok(m)) = (a.parse::<u32>(), b.parse::<u32>()) {
                return Some((n, m));
            }
        }
    }
    None
}

/// Convert a public-inbox `<content type="xhtml">` body (a `<div><pre>…</pre></div>`
/// with URLs wrapped as `<a href=…>url</a>`) to plain text: drop every tag (anchor
/// text is the URL itself, so this preserves links), then unescape entities last.
fn xhtml_to_text(xhtml: &str) -> String {
    let mut out = String::with_capacity(xhtml.len());
    let mut in_tag = false;
    for c in xhtml.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    xml_unescape(out.trim())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_atom_entries_with_body() {
        let feed = r#"<feed>
          <entry><title>[PATCH 0/2] fix</title>
            <link href="https://lore.kernel.org/lkml/msg1@x/"/>
            <content type="xhtml"><div><pre>Cover letter body
with &lt;angle&gt; brackets and a <a href="http://x/">http://x/</a> link.</pre></div></content></entry>
          <entry><title>Re: hi &amp; bye</title>
            <link href="https://lore.kernel.org/lkml/msg2@x/"/>
            <content type="xhtml"><div><pre>reply</pre></div></content></entry>
        </feed>"#;
        let entries = parse_entries(feed);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].permalink, "https://lore.kernel.org/lkml/msg1@x/");
        assert_eq!(entries[0].title, "[PATCH 0/2] fix");
        let body = xhtml_to_text(&entries[0].content);
        assert_eq!(body, "Cover letter body\nwith <angle> brackets and a http://x/ link.");
    }

    #[test]
    fn keeps_series_roots_drops_followups_and_replies() {
        assert!(is_series_root("[PATCH 0/9] treewide: cleanup"));
        assert!(is_series_root("[PATCH v2] mm: single patch"));
        assert!(is_series_root("[PATCH net 1/1] tcp: bound timers"));
        assert!(!is_series_root("[PATCH 2/9] mm: one of many"));
        assert!(!is_series_root("Re: [PATCH 0/2] fix"));
        assert!(!is_series_root("just a normal email"));
    }
}
