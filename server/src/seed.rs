//! Seed the demo job (#1) from a committed fixture so the guided tour's `/job/1` always
//! resolves to a real, populated job — even on a fresh database. The fixture is captured
//! from a real run and lives in `server/seed/`: `job1.json` (the DB rows) plus `job1/`
//! (the reproducer bundle and log tree). No-op once job #1 already exists.

use serde::Deserialize;

use crate::AppState;

#[derive(Deserialize)]
struct Metric { ts_ms: i64, rss_bytes: i64, disk_bytes: i64 }

#[derive(Deserialize)]
struct Event { ts_ms: i64, phase: String }

#[derive(Deserialize)]
struct Summary {
    field: String,
    server: String,
    text: String,
    ms: Option<i64>,
    tokens: Option<i64>,
    model: Option<String>,
}

#[derive(Deserialize)]
struct Fixture {
    job: crate::db::Job,
    #[serde(default)]
    metrics: Vec<Metric>,
    #[serde(default)]
    events: Vec<Event>,
    #[serde(default)]
    summaries: Vec<Summary>,
}

/// Insert job #1 from `server/seed/` if it's missing. Called once at startup.
pub fn seed_demo_job(st: &AppState) -> anyhow::Result<()> {
    if st.db.get_job(1)?.is_some() {
        return Ok(()); // real (or already-seeded) job #1 present — leave it alone
    }
    let dir = st.repo.join("server/seed");
    let fixture_path = dir.join("job1.json");
    if !fixture_path.is_file() {
        return Ok(()); // fixture not shipped — nothing to seed
    }
    let f: Fixture = serde_json::from_slice(&std::fs::read(&fixture_path)?)?;

    let id = st.db.seed_job(&f.job)?;
    for m in &f.metrics {
        st.db.add_metric(id, m.ts_ms, m.rss_bytes, m.disk_bytes, None)?;
    }
    for e in &f.events {
        st.db.add_event(id, e.ts_ms, &e.phase, "")?;
    }
    let now = f.job.finished_ms.or(f.job.started_ms).unwrap_or(f.job.created_ms);
    for s in &f.summaries {
        st.db.set_job_summary(id, &s.field, &s.server, &s.text,
            s.ms.unwrap_or(0) as u64, s.tokens.unwrap_or(0) as u32,
            s.model.as_deref().unwrap_or(""), now)?;
    }

    // The reproducer bundle + logs go into the job's work dir (files, mirroring a real run).
    let src = dir.join("job1");
    if src.is_dir() {
        let dst = st.work.join(id.to_string());
        let _ = std::fs::remove_dir_all(&dst); // clear any stale dir from a recycled id
        crate::scaffold::copy_dir_all(&src, &dst)?;
    }
    tracing::info!("seeded demo job #{id} from {}", fixture_path.display());
    Ok(())
}
