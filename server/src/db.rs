//! DuckDB-backed job index, event log, and metrics time-series.
use std::sync::{Arc, Mutex};

use anyhow::Result;
use duckdb::Connection;
use serde::Serialize;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

#[derive(Serialize, Clone)]
pub struct Job {
    pub id: i64,
    pub created_ms: i64,
    pub started_ms: Option<i64>,
    pub finished_ms: Option<i64>,
    pub status: String,
    pub phase: Option<String>,
    pub exit_code: Option<i64>,
    pub ram_peak: i64,
    pub disk_peak: i64,
    pub reaped_ms: Option<i64>,
    /// Natural-language summary (see src/summarize.rs): a preliminary one-liner when the
    /// job starts, replaced by a two-sentence summary (incl. output) when it finishes.
    pub summary: Option<String>,
}

#[derive(Serialize, Clone)]
pub struct Sample {
    pub ts_ms: i64,
    pub rss_bytes: i64,
    pub disk_bytes: i64,
}

#[derive(Serialize, Clone)]
pub struct Peak {
    pub id: i64,
    pub ram_peak: i64,
    pub disk_peak: i64,
    pub status: String,
}

impl Db {
    pub fn open(path: &std::path::Path) -> Result<Self> {
        let conn = Connection::open(path)?;
        conn.execute_batch(
            r#"
            CREATE SEQUENCE IF NOT EXISTS job_seq START 1;
            CREATE TABLE IF NOT EXISTS jobs (
                id BIGINT PRIMARY KEY,
                created_ms BIGINT NOT NULL,
                started_ms BIGINT,
                finished_ms BIGINT,
                status VARCHAR NOT NULL,
                phase VARCHAR,
                exit_code BIGINT,
                ram_peak BIGINT NOT NULL DEFAULT 0,
                disk_peak BIGINT NOT NULL DEFAULT 0,
                submitter VARCHAR
            );
            ALTER TABLE jobs ADD COLUMN IF NOT EXISTS reaped_ms BIGINT;
            ALTER TABLE jobs ADD COLUMN IF NOT EXISTS summary VARCHAR;
            CREATE TABLE IF NOT EXISTS events (
                job_id BIGINT NOT NULL, ts_ms BIGINT NOT NULL,
                phase VARCHAR NOT NULL, message VARCHAR
            );
            CREATE TABLE IF NOT EXISTS metrics (
                job_id BIGINT NOT NULL, ts_ms BIGINT NOT NULL,
                rss_bytes BIGINT NOT NULL, disk_bytes BIGINT NOT NULL
            );
            "#,
        )?;
        Ok(Self { conn: Arc::new(Mutex::new(conn)) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().expect("db mutex poisoned")
    }

    pub fn create_job(&self, now_ms: i64, submitter: Option<&str>) -> Result<i64> {
        let c = self.lock();
        let id: i64 = c.query_row("SELECT nextval('job_seq')", [], |r| r.get(0))?;
        c.execute(
            "INSERT INTO jobs (id, created_ms, status, submitter) VALUES (?, ?, 'queued', ?)",
            duckdb::params![id, now_ms, submitter],
        )?;
        Ok(id)
    }

    pub fn set_running(&self, id: i64, now_ms: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET status='running', started_ms=? WHERE id=?",
            duckdb::params![now_ms, id],
        )?;
        Ok(())
    }

    pub fn set_phase(&self, id: i64, phase: &str) -> Result<()> {
        self.lock()
            .execute("UPDATE jobs SET phase=? WHERE id=?", duckdb::params![phase, id])?;
        Ok(())
    }

    pub fn finish(&self, id: i64, now_ms: i64, status: &str, exit: Option<i64>,
                  ram_peak: i64, disk_peak: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET status=?, finished_ms=?, exit_code=?, ram_peak=?, disk_peak=? WHERE id=?",
            duckdb::params![status, now_ms, exit, ram_peak, disk_peak, id],
        )?;
        Ok(())
    }

    /// Store/replace a job's natural-language summary (called at job start, then again
    /// at job finish — the later call supersedes the preliminary one).
    pub fn set_summary(&self, id: i64, summary: &str) -> Result<()> {
        self.lock()
            .execute("UPDATE jobs SET summary=? WHERE id=?", duckdb::params![summary, id])?;
        Ok(())
    }

    pub fn add_event(&self, id: i64, ts_ms: i64, phase: &str, message: &str) -> Result<()> {
        self.lock().execute(
            "INSERT INTO events (job_id, ts_ms, phase, message) VALUES (?, ?, ?, ?)",
            duckdb::params![id, ts_ms, phase, message],
        )?;
        Ok(())
    }

    pub fn get_events(&self, id: i64) -> Result<Vec<(i64, String)>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT ts_ms, phase FROM events WHERE job_id=? ORDER BY ts_ms",
        )?;
        let mut rows = stmt.query(duckdb::params![id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push((r.get(0)?, r.get(1)?));
        }
        Ok(out)
    }

    pub fn add_metric(&self, id: i64, ts_ms: i64, rss: i64, disk: i64) -> Result<()> {
        self.lock().execute(
            "INSERT INTO metrics (job_id, ts_ms, rss_bytes, disk_bytes) VALUES (?, ?, ?, ?)",
            duckdb::params![id, ts_ms, rss, disk],
        )?;
        Ok(())
    }

    pub fn get_job(&self, id: i64) -> Result<Option<Job>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT id, created_ms, started_ms, finished_ms, status, phase, exit_code, ram_peak, disk_peak, reaped_ms, summary
             FROM jobs WHERE id=?",
        )?;
        let mut rows = stmt.query(duckdb::params![id])?;
        if let Some(r) = rows.next()? {
            Ok(Some(row_to_job(r)?))
        } else {
            Ok(None)
        }
    }

    pub fn list_jobs(&self) -> Result<Vec<Job>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT id, created_ms, started_ms, finished_ms, status, phase, exit_code, ram_peak, disk_peak, reaped_ms, summary
             FROM jobs ORDER BY id DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(row_to_job(r)?);
        }
        Ok(out)
    }

    pub fn metrics(&self, id: i64) -> Result<Vec<Sample>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT ts_ms, rss_bytes, disk_bytes FROM metrics WHERE job_id=? ORDER BY ts_ms",
        )?;
        let mut rows = stmt.query(duckdb::params![id])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(Sample { ts_ms: r.get(0)?, rss_bytes: r.get(1)?, disk_bytes: r.get(2)? });
        }
        Ok(out)
    }

    pub fn peaks(&self) -> Result<Vec<Peak>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT id, ram_peak, disk_peak, status FROM jobs ORDER BY id DESC",
        )?;
        let mut rows = stmt.query([])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(Peak { id: r.get(0)?, ram_peak: r.get(1)?, disk_peak: r.get(2)?, status: r.get(3)? });
        }
        Ok(out)
    }

    /// Mark any jobs left mid-flight by a previous run as failed (startup recovery).
    pub fn recover_orphans(&self, now_ms: i64) -> Result<()> {
        self.lock().execute(
            "UPDATE jobs SET status='failed', finished_ms=? WHERE status IN ('queued','running')",
            duckdb::params![now_ms],
        )?;
        Ok(())
    }

    /// Finished jobs older than `before_ms` whose on-disk dir hasn't been reaped yet.
    pub fn reapable(&self, before_ms: i64) -> Result<Vec<i64>> {
        let c = self.lock();
        let mut stmt = c.prepare(
            "SELECT id FROM jobs WHERE finished_ms IS NOT NULL AND finished_ms < ? AND reaped_ms IS NULL ORDER BY id",
        )?;
        let mut rows = stmt.query(duckdb::params![before_ms])?;
        let mut out = Vec::new();
        while let Some(r) = rows.next()? {
            out.push(r.get(0)?);
        }
        Ok(out)
    }

    /// Record that a job's on-disk dir (logs included) was removed by the sweep.
    pub fn mark_reaped(&self, id: i64, ts_ms: i64) -> Result<()> {
        self.lock()
            .execute("UPDATE jobs SET reaped_ms=? WHERE id=?", duckdb::params![ts_ms, id])?;
        Ok(())
    }
}

fn row_to_job(r: &duckdb::Row<'_>) -> Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        created_ms: r.get(1)?,
        started_ms: r.get(2)?,
        finished_ms: r.get(3)?,
        status: r.get(4)?,
        phase: r.get(5)?,
        exit_code: r.get(6)?,
        ram_peak: r.get(7)?,
        disk_peak: r.get(8)?,
        reaped_ms: r.get(9)?,
        summary: r.get(10)?,
    })
}
