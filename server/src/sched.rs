//! Resource-aware admission: how many jobs may run given free RAM/disk.
use std::path::Path;

use sysinfo::System;

const GB: u64 = 1024 * 1024 * 1024;

#[derive(Clone)]
pub struct Cfg {
    pub ram_reserve: u64,   // keep this much RAM free for the host
    pub disk_reserve: u64,  // keep this much disk free
    pub max_jobs: usize,    // hard cap regardless of resources
    pub est_ram: u64,       // default per-job RAM estimate (refined by learned peaks)
    pub est_disk: u64,      // default per-job disk estimate
    // Disk cleanup:
    pub keep_worktrees: bool,   // skip reclaiming work/<id>/wt on job finish
    pub retention_days: u64,    // delete a finished job's whole dir after this many days
    pub linux_src: std::path::PathBuf, // kernel repo to `git worktree prune`
}

impl Cfg {
    pub fn from_env() -> Self {
        let g = |k: &str, d: u64| {
            std::env::var(k).ok().and_then(|v| v.parse::<f64>().ok())
                .map(|gb| (gb * GB as f64) as u64).unwrap_or(d)
        };
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        Cfg {
            // Reserve host RAM. The in-process summary model adds ~2.3 GB resident to
            // the server itself (see src/summarize.rs), which isn't part of any job's
            // per-job reservation, so the default headroom is bumped to cover it.
            ram_reserve: g("MK_RAM_RESERVE_GB", 4 * GB),
            disk_reserve: g("MK_DISK_RESERVE_GB", 5 * GB),
            max_jobs: std::env::var("MK_MAX_JOBS").ok().and_then(|v| v.parse().ok()).unwrap_or(4),
            est_ram: g("MK_EST_RAM_GB", 3 * GB),
            est_disk: g("MK_EST_DISK_GB", 3 * GB),
            keep_worktrees: std::env::var("MK_KEEP_WORKTREES").map(|v| v == "1" || v.eq_ignore_ascii_case("true")).unwrap_or(false),
            retention_days: std::env::var("MK_JOB_RETENTION_DAYS").ok().and_then(|v| v.parse().ok()).unwrap_or(30),
            linux_src: std::path::PathBuf::from(
                std::env::var("MK_LINUX_SRC").unwrap_or_else(|_| format!("{home}/linux"))),
        }
    }
}

pub enum SchedMsg {
    New(i64),
    Finished(i64),
}

pub struct Resources {
    pub avail_ram: u64,
    pub total_ram: u64,
    pub avail_disk: u64,
    pub total_disk: u64,
}

pub fn read_resources(sys: &mut System, work: &Path) -> Resources {
    sys.refresh_memory();
    let (avail_disk, total_disk) = df(work);
    Resources {
        avail_ram: sys.available_memory(),
        total_ram: sys.total_memory(),
        avail_disk,
        total_disk,
    }
}

/// Admit a job needing (est_ram, est_disk) given current resources and the sum of
/// running jobs' reservations, under the hard cap.
///
/// RAM is gated by the *reservation* model: keep total committed (running
/// reservations + this job + host reserve) within physical RAM. We deliberately
/// don't gate on live "available" memory — on macOS that figure excludes
/// reclaimable file cache and badly under-reports, which would wedge the queue.
/// Disk is gated by *live* free space (df reports it accurately) plus a reserve.
pub fn can_admit(
    r: &Resources, reserved_ram: u64, reserved_disk: u64,
    est_ram: u64, est_disk: u64, running: usize, cfg: &Cfg,
) -> bool {
    if running >= cfg.max_jobs {
        return false;
    }
    let ram_ok = reserved_ram + est_ram + cfg.ram_reserve <= r.total_ram;
    let disk_ok = r.avail_disk >= est_disk + cfg.disk_reserve
        && reserved_disk + est_disk + cfg.disk_reserve <= r.total_disk;
    ram_ok && disk_ok
}

/// (available, total) bytes on the filesystem holding `path`, via `df -Pk`.
fn df(path: &Path) -> (u64, u64) {
    let out = std::process::Command::new("df").arg("-Pk").arg(path).output();
    if let Ok(o) = out {
        let text = String::from_utf8_lossy(&o.stdout);
        if let Some(line) = text.lines().nth(1) {
            let f: Vec<&str> = line.split_whitespace().collect();
            // Filesystem 1024-blocks Used Available Capacity Mounted
            if f.len() >= 4 {
                let total = f[1].parse::<u64>().unwrap_or(0) * 1024;
                let avail = f[3].parse::<u64>().unwrap_or(0) * 1024;
                return (avail, total);
            }
        }
    }
    (u64::MAX, u64::MAX) // unknown -> don't block on disk
}
