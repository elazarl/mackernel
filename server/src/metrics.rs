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

/// A single `podman stats` reading for one container. `blk_bytes` is a cumulative counter
/// (bytes since container start); the UI plots its per-interval delta. (Network is sampled
/// host-wide instead — see host_net_bytes — so it's continuous across the whole job.)
pub struct ContainerStat {
    pub cpu_pct: f64,
    pub mem_bytes: u64,
    pub blk_bytes: u64,
}

/// Sample one podman container's CPU/mem/disk in a single call. Used for the scaffold
/// stage, whose whole workload is the opencode container (a host `ps` can't see into it,
/// least of all through the podman-machine VM on macOS). `None` if the container is gone
/// or stats fail — the caller treats that as "no sample this tick".
pub async fn container_stats(name: &str) -> Option<ContainerStat> {
    let out = tokio::process::Command::new("podman")
        .args(["stats", "--no-stream", "--format",
               "{{.CPUPerc}}|{{.MemUsage}}|{{.BlockIO}}", name])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let line = String::from_utf8_lossy(&out.stdout);
    let line = line.trim();
    let mut f = line.split('|');
    let cpu = f.next()?.trim().trim_end_matches('%').parse::<f64>().ok()?;
    // MemUsage / BlockIO are "<used> / <total>" and "<read> / <write>" pairs.
    let mem = parse_size(f.next()?.split('/').next()?);
    let (brd, bwr) = parse_pair(f.next()?);
    Some(ContainerStat { cpu_pct: cpu, mem_bytes: mem, blk_bytes: brd + bwr })
}

/// Cumulative host network bytes (rx+tx over real interfaces, excluding loopback).
/// Sampled across the WHOLE job — scaffold and run phases alike — so network activity
/// shows throughout the test cycle, not just while the opencode container is up. One
/// monotonic host-lifetime counter (rootless podman + qemu egress both flow through the
/// host, so this captures them); the UI charts its per-interval delta as a rate. Linux
/// via /proc/net/dev; None elsewhere (the macOS dev host doesn't run the pipeline).
pub async fn host_net_bytes() -> Option<u64> {
    let text = tokio::fs::read_to_string("/proc/net/dev").await.ok()?;
    Some(sum_proc_net_dev(&text))
}

/// Host CPU busy/total jiffies from /proc/stat's aggregate `cpu` line. The caller keeps
/// the previous reading and computes utilization % from the (busy, total) delta between
/// two ticks. Sampled during the run phase so CPU shows across the whole cycle (the build
/// + qemu are host processes). Linux via /proc/stat; None elsewhere.
pub async fn host_cpu_times() -> Option<(u64, u64)> {
    let text = tokio::fs::read_to_string("/proc/stat").await.ok()?;
    Some(parse_proc_stat_cpu(&text)?)
}

/// Parse the aggregate `cpu` line into (busy, total) jiffies. Fields:
/// user nice system idle iowait irq softirq steal ...; busy = total - idle - iowait.
fn parse_proc_stat_cpu(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().next()?;
    let mut f = line.split_whitespace();
    if f.next()? != "cpu" { return None; }
    let vals: Vec<u64> = f.take(8).map(|x| x.parse().unwrap_or(0)).collect();
    if vals.len() < 4 { return None; }
    let total: u64 = vals.iter().sum();
    let idle = vals[3] + vals.get(4).copied().unwrap_or(0); // idle + iowait
    Some((total.saturating_sub(idle), total))
}

fn sum_proc_net_dev(text: &str) -> u64 {
    let mut total = 0u64;
    for line in text.lines() {
        let Some((iface, rest)) = line.split_once(':') else { continue };
        if iface.trim() == "lo" || iface.trim().is_empty() { continue; }
        // /proc/net/dev columns after the iface: receive bytes = 0, transmit bytes = 8.
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() >= 9 {
            total += f[0].parse::<u64>().unwrap_or(0) + f[8].parse::<u64>().unwrap_or(0);
        }
    }
    total
}

fn parse_pair(s: &str) -> (u64, u64) {
    let mut p = s.split('/');
    (p.next().map(parse_size).unwrap_or(0), p.next().map(parse_size).unwrap_or(0))
}

// ponytail: podman prints decimal units (kB/MB/GB = 1000^n), so scale by 1000, not 1024.
// Covers the units podman actually emits; extend if a new suffix shows up.
fn parse_size(s: &str) -> u64 {
    let s = s.trim();
    let digits: String = s.chars().take_while(|c| c.is_ascii_digit() || *c == '.').collect();
    let n: f64 = digits.parse().unwrap_or(0.0);
    let unit = s[digits.len()..].trim().to_ascii_lowercase();
    let mult = match unit.as_str() {
        "b" | "" => 1.0,
        "kb" => 1e3,
        "mb" => 1e6,
        "gb" => 1e9,
        "tb" => 1e12,
        _ => 1.0,
    };
    (n * mult).round() as u64
}

#[cfg(test)]
mod tests {
    use super::*;
    fn parse_line(line: &str) -> ContainerStat {
        let mut f = line.split('|');
        let cpu = f.next().unwrap().trim().trim_end_matches('%').parse::<f64>().unwrap();
        let mem = parse_size(f.next().unwrap().split('/').next().unwrap());
        let (brd, bwr) = parse_pair(f.next().unwrap());
        ContainerStat { cpu_pct: cpu, mem_bytes: mem, blk_bytes: brd + bwr }
    }

    #[test]
    fn parses_podman_stats_line() {
        let s = parse_line("0.50%|12.3MB / 1.5GB|0B / 4.1MB");
        assert_eq!(s.cpu_pct, 0.50);
        assert_eq!(s.mem_bytes, 12_300_000);
        assert_eq!(s.blk_bytes, 4_100_000);
    }

    #[test]
    fn sums_proc_net_dev_excluding_loopback() {
        let text = "\
Inter-|   Receive                    |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo:  100      1    0    0    0     0          0         0      100      1    0    0    0     0       0          0
  eth0: 1000     10    0    0    0     0          0         0      500      5    0    0    0     0       0          0
";
        // eth0 rx 1000 + tx 500 = 1500; lo excluded.
        assert_eq!(sum_proc_net_dev(text), 1500);
    }

    #[test]
    fn parses_proc_stat_cpu_busy_and_total() {
        // user=10 nice=0 system=5 idle=100 iowait=5 irq=0 softirq=0 steal=0 -> sum 120
        let (busy, total) = parse_proc_stat_cpu("cpu  10 0 5 100 5 0 0 0\ncpu0 ...\n").unwrap();
        assert_eq!(total, 120);
        assert_eq!(busy, 120 - 100 - 5); // total - idle - iowait = 15
    }
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
