//! Sampling helpers: process-subtree RSS and job-dir disk usage.
//!
//! RSS via `ps` and disk via `du` -- both cross-platform (macOS + Linux) and
//! cheap to shell out. NOTE: under rootless podman on Linux the build's gcc/make
//! run as host processes (captured here); on macOS the build runs inside the
//! podman-machine VM, so RSS there reflects the python + qemu (-m 2048) tree, not
//! the in-VM build. Disk (the kernel build tree) is accurate on both.
use std::collections::HashMap;
use std::path::Path;

/// Sum of RSS (bytes) of `root_pid` and all its descendants.
pub async fn tree_rss(root_pid: u32) -> u64 {
    let out = match tokio::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,rss="])
        .output()
        .await
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return 0,
    };
    let text = String::from_utf8_lossy(&out);
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    let mut rss_kb: HashMap<u32, u64> = HashMap::new();
    for line in text.lines() {
        let mut f = line.split_whitespace();
        let (Some(pid), Some(ppid), Some(rss)) = (f.next(), f.next(), f.next()) else { continue };
        let (Ok(pid), Ok(ppid), Ok(rss)) = (pid.parse::<u32>(), ppid.parse::<u32>(), rss.parse::<u64>()) else { continue };
        children.entry(ppid).or_default().push(pid);
        rss_kb.insert(pid, rss);
    }
    // BFS from root over the child map.
    let mut total = 0u64;
    let mut stack = vec![root_pid];
    let mut seen = std::collections::HashSet::new();
    while let Some(p) = stack.pop() {
        if !seen.insert(p) {
            continue;
        }
        total += rss_kb.get(&p).copied().unwrap_or(0);
        if let Some(kids) = children.get(&p) {
            stack.extend(kids);
        }
    }
    total * 1024
}

/// Disk usage (bytes) of `dir` via `du -sk`.
pub async fn dir_disk(dir: &Path) -> u64 {
    let out = match tokio::process::Command::new("du")
        .arg("-sk")
        .arg(dir)
        .output()
        .await
    {
        Ok(o) if o.status.success() => o.stdout,
        _ => return 0,
    };
    String::from_utf8_lossy(&out)
        .split_whitespace()
        .next()
        .and_then(|kb| kb.parse::<u64>().ok())
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}
