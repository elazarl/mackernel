#!/usr/bin/env bash
# Deploy the latest mackernel-server to a remote host over SSH:
#   push the current branch -> pull on the host -> rebuild the UI + release binary
#   -> restart the systemd --user service -> verify it's up and enforcing auth.
#
# Mirrors the manual deploy to `home` (linux.leibo.org.il). Override any default
# with an env var, e.g.  MK_HOST=myhost ./deploy.sh
#
#   MK_HOST          SSH host alias / target            (default: home)
#   MK_REMOTE_REPO   repo path on the host              (default: ~/mackernel)
#   MK_SERVICE       systemd --user unit to restart     (default: mackernel-server.service)
#   MK_BRANCH        branch to deploy                   (default: current branch)
#   MK_REMOTE_PATH   PATH to use on the host (cargo/npm) (default: ~/.cargo/bin + system)
#   MK_NO_PUSH=1     skip `git push` (deploy what the host can already pull)
set -euo pipefail

HOST="${MK_HOST:-home}"
REMOTE_REPO="${MK_REMOTE_REPO:-mackernel}"   # relative to the remote home dir
SERVICE="${MK_SERVICE:-mackernel-server.service}"
BRANCH="${MK_BRANCH:-$(git rev-parse --abbrev-ref HEAD)}"
REMOTE_PATH="${MK_REMOTE_PATH:-\$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin}"

log() { printf '\n\033[1;36m== %s\033[0m\n' "$*"; }

if [[ "${MK_NO_PUSH:-0}" != "1" ]]; then
  log "push $BRANCH -> origin"
  git push origin "$BRANCH"
else
  log "skipping push (MK_NO_PUSH=1)"
fi

log "deploy on $HOST:~/$REMOTE_REPO (branch $BRANCH)"
ssh "$HOST" bash -seo pipefail <<REMOTE
  export PATH="$REMOTE_PATH:\$PATH"
  cd ~/"$REMOTE_REPO"

  echo "== git pull"
  git fetch origin "$BRANCH"
  git checkout "$BRANCH"
  git pull --ff-only origin "$BRANCH"
  echo "now at: \$(git rev-parse --short HEAD) \$(git log -1 --format=%s)"

  echo "== build UI (embedded into the binary)"
  ( cd server/ui && npm ci && npm run build )

  echo "== cargo build --release"
  ( cd server && cargo build --release )

  echo "== restart $SERVICE"
  systemctl --user restart "$SERVICE"
  sleep 2
  systemctl --user is-active "$SERVICE"

  # Smoke-test: unauthenticated /api/* must be rejected (auth is enforced).
  BIND="\$(systemctl --user show "$SERVICE" -p Environment | tr ' ' '\n' | sed -n 's/.*MK_SERVER_BIND=//p')"
  BIND="\${BIND:-127.0.0.1:8080}"
  CODE="\$(curl -s -o /dev/null -w '%{http_code}' "http://\$BIND/api/jobs" || echo 000)"
  echo "== smoke-test http://\$BIND/api/jobs (no token) -> \$CODE"
  if [[ "\$CODE" != "401" ]]; then
    echo "WARNING: expected 401 from unauthenticated /api/jobs, got \$CODE" >&2
  fi
REMOTE

log "DEPLOY_OK ($HOST)"
