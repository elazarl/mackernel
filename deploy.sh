#!/usr/bin/env bash
# Deploy the latest mackernel-server to a remote host over SSH.
#
# The UI is built LOCALLY and its dist/ is shipped to the host (rsync); only the
# Rust binary is built on the host. This avoids running npm on the host — `home`'s
# npm crashes mid-install ("Exit handler never called!") on a larger dep set yet
# exits 0, which silently shipped a stale UI. Building where npm is known-good and
# embedding the prebuilt dist sidesteps that entirely.
#
# Flow: build UI locally -> push branch -> host git pull -> rsync dist -> host
#       cargo build --release -> restart the systemd --user service -> smoke-test.
#
# Override any default with an env var, e.g.  MK_HOST=myhost ./deploy.sh
#   MK_HOST          SSH host alias / target            (default: home)
#   MK_REMOTE_REPO   repo path on the host (rel. to ~)  (default: mackernel)
#   MK_SERVICE       systemd --user unit to restart     (default: mackernel-server.service)
#   MK_BRANCH        branch to deploy                   (default: current branch)
#   MK_REMOTE_PATH   PATH to use on the host (cargo)     (default: ~/.cargo/bin + system)
#   MK_NO_PUSH=1     skip `git push` (deploy what the host can already pull)
set -euo pipefail

HOST="${MK_HOST:-home}"
REMOTE_REPO="${MK_REMOTE_REPO:-mackernel}"
SERVICE="${MK_SERVICE:-mackernel-server.service}"
BRANCH="${MK_BRANCH:-$(git rev-parse --abbrev-ref HEAD)}"
REMOTE_PATH="${MK_REMOTE_PATH:-\$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin}"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

log() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }

log "build UI locally (embedded into the binary)"
(
  cd "$HERE/server/ui"
  npm ci || { echo "npm ci failed; retrying clean install" >&2; rm -rf node_modules; npm install; }
  # npm can exit 0 yet leave deps missing — assert vite exists before trusting it.
  test -x node_modules/.bin/vite || { echo "vite missing after install" >&2; exit 1; }
  npm run build
)
test -f "$HERE/server/ui/dist/index.html" || { echo "no dist/index.html after build" >&2; exit 1; }

if [[ "${MK_NO_PUSH:-0}" != "1" ]]; then
  log "push $BRANCH -> origin"
  git -C "$HERE" push origin "$BRANCH"
else
  log "skipping push (MK_NO_PUSH=1)"
fi

log "pull source on $HOST:~/$REMOTE_REPO (branch $BRANCH)"
ssh "$HOST" bash -seo pipefail <<REMOTE
  cd ~/"$REMOTE_REPO"
  git fetch origin "$BRANCH"
  git checkout "$BRANCH"
  git pull --ff-only origin "$BRANCH"
  echo "now at: \$(git rev-parse --short HEAD) \$(git log -1 --format=%s)"
REMOTE

log "ship prebuilt UI dist -> $HOST"
rsync -az --delete "$HERE/server/ui/dist/" "$HOST:$REMOTE_REPO/server/ui/dist/"

log "build binary + restart on $HOST"
ssh "$HOST" bash -seo pipefail <<REMOTE
  export PATH="$REMOTE_PATH:\$PATH"
  cd ~/"$REMOTE_REPO/server"
  echo "dist asset: \$(grep -o 'assets/[^\"]*' ui/dist/index.html)"
  cargo build --release 2>&1 | tail -3
  systemctl --user restart "$SERVICE"
  sleep 2
  systemctl --user is-active "$SERVICE"

  # Smoke-test: unauthenticated /api/* must be rejected (auth is enforced).
  BIND="\$(systemctl --user show "$SERVICE" -p Environment | tr ' ' '\n' | sed -n 's/.*MK_SERVER_BIND=//p')"
  BIND="\${BIND:-127.0.0.1:8080}"
  CODE="\$(curl -s -o /dev/null -w '%{http_code}' "http://\$BIND/api/jobs" || echo 000)"
  echo "smoke-test http://\$BIND/api/jobs (no token) -> \$CODE"
  [[ "\$CODE" == "401" ]] || { echo "WARNING: expected 401 from unauthenticated /api/jobs, got \$CODE" >&2; exit 1; }
REMOTE

log "DEPLOY_OK ($HOST)"
