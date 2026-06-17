# mackernel-server

A REST + SSE service that queues reproducer **bundles** (the `examples/greeter.md`
format), builds + boots + runs each one through the hardened `run-kernel.py`
pipeline, measures peak RAM/disk, and serves an embedded React UI. The user submits
a bundle, gets a **job number**, and watches it live.

## Build

The React UI is embedded into the binary, so build it first:

```bash
cd ui && npm ci && npm run build && cd ..   # produces ui/dist/ (embedded via rust-embed)
cargo build --release                        # single binary at target/release/mackernel-server
```

(If `ui/dist/` is absent the binary still builds and runs, but serves no UI.)

## Run

```bash
MK_REPO=/path/to/mackernel \      # the repo holding run-kernel.py (default: ..)
MK_SERVER_WORK=./work \           # per-job dirs, logs, and the DuckDB file
MK_TOKEN=some-secret \            # bearer token for /api/* (omit = built-in v7.1 token)
MK_SERVER_BIND=127.0.0.1:8087 \
  ./target/release/mackernel-server
```

Open the bind address in a browser, or use the API:

```bash
curl -H "Authorization: Bearer some-secret" --data-binary @../examples/greeter.md \
     http://127.0.0.1:8087/api/jobs                 # -> {"id": N}
curl -H "Authorization: Bearer some-secret" -N \
     http://127.0.0.1:8087/api/jobs/N/events        # SSE: phases + metrics + done
curl -H "Authorization: Bearer some-secret" \
     http://127.0.0.1:8087/api/jobs/N/logs/{compile,dmesg,exec}
```

Jobs run with `MK_SANDBOX=auto` and a per-job isolated kernel worktree
(`MK_WT_ROOT`). On macOS keep `MK_SERVER_WORK` under the repo (or `$HOME`) so the
sandboxed qemu / podman mounts work.

## Endpoints
- `POST /api/jobs` — body is the bundle text (or `{"source":"<url>"}`); returns `{id}`.
- `GET /api/jobs[/{id}]` — list / detail (status, phase, exit, peak RAM/disk).
- `GET /api/jobs/{id}/events` — SSE stream (phase / metric / done); `?token=` for browsers.
- `GET /api/jobs/{id}/metrics` — RAM/disk sample time-series (from DuckDB).
- `GET /api/jobs/{id}/logs/{compile|dmesg|exec}` — the three logs.
- `GET /api/metrics/peaks` — per-job peak RAM/disk for the overview chart.

## Scheduler
A resource-aware admission loop runs queued jobs while the per-job RAM reservation
fits physical RAM (minus `MK_RAM_RESERVE_GB`) and live free disk fits the disk
estimate (minus `MK_DISK_RESERVE_GB`), up to `MK_MAX_JOBS`. Per-job estimates start
at `MK_EST_RAM_GB` / `MK_EST_DISK_GB` and grow to the largest measured peak.

## Disk retention / cleanup
Each job builds in its own kernel worktree under `work/<id>/wt` (~3 GB for a
pinned-commit bundle). To keep disk bounded:
- **Reclaim on finish:** as soon as a job ends (done or failed) its `work/<id>/wt` is
  deleted; the logs, metrics, and DuckDB job row are kept. Set `MK_KEEP_WORKTREES=1`
  to keep the build tree around (debugging).
- **Retention sweep:** a background task (≈30 s after start, then hourly) deletes the
  whole `work/<id>` dir (logs included) for jobs finished more than
  `MK_JOB_RETENTION_DAYS` (default 30) ago, marks the row `reaped_ms`, and runs
  `git -C $MK_LINUX_SRC worktree prune` to drop stale worktree registrations. The job
  stays in the list (status/peaks) but its log endpoints then 404 (the UI shows
  "logs expired").

## Config (env)
`MK_REPO`, `MK_SERVER_WORK`, `MK_SERVER_BIND`, `MK_TOKEN`, `MK_MAX_JOBS`,
`MK_RAM_RESERVE_GB`, `MK_DISK_RESERVE_GB`, `MK_EST_RAM_GB`, `MK_EST_DISK_GB`,
`MK_KEEP_WORKTREES`, `MK_JOB_RETENTION_DAYS`, `MK_LINUX_SRC` (kernel repo to prune;
default `~/linux`).

## Auth
`/api/*` requires a bearer token (header `Authorization: Bearer …`, or `?token=` for
EventSource). The token defaults to the commit hash that the `v7.1` tag points to and
is overridable with `MK_TOKEN`; the embedded UI is served unauthenticated and, on
first visit, prompts for "the v7.1 commit" and sends it as the bearer token. A 401
clears the stored token and re-prompts. **Google OAuth (OIDC) is planned but not yet
implemented** — see the project plan; it will be config-gated (token-only when OIDC
env is unset).

## Trust model
The service builds and boots arbitrary (untrusted) kernels. It runs them with the
hardened/sandboxed pipeline (`MK_SANDBOX`), gates the API behind a token, and
defaults to binding localhost. Don't expose it to untrusted networks without the
token (and, eventually, OAuth).
