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
#   OPENROUTER_API_KEY  if set, adds OpenRouter summary backend(s) (the first is
#                       primary), written to a mode-600 systemd drop-in on the host.
#                       NEVER committed; e.g.  OPENROUTER_API_KEY=sk-or-... ./deploy.sh
#   MK_OR_MODELS     space-separated OpenRouter model ids (non-primary)
#                    (default: "poolside/laguna-xs.2:free nvidia/nemotron-3-ultra-550b-a55b:free")
#   MK_OPENCODE_MODEL  opencode (CLI) free model, the PRIMARY backend + the model
#                    the scaffolder uses (default: opencode/deepseek-v4-flash-free)
#   MK_OPENCODE_PROXY  if set, the scaffold agent container routes egress through this
#                    allowlisting HTTPS proxy (see docs/opencode-egress.md)
#   MK_LLAMA_NICE    nice level for the local llama-server          (default: 19)
#   MK_LLAMA_DISABLE drop the local phi3.5-mini backend, remote-only (default: 1;
#                    home is RAM-constrained — set 0 to run the local model)
set -euo pipefail

HOST="${MK_HOST:-home}"
REMOTE_REPO="${MK_REMOTE_REPO:-mackernel}"
SERVICE="${MK_SERVICE:-mackernel-server.service}"
BRANCH="${MK_BRANCH:-$(git rev-parse --abbrev-ref HEAD)}"
REMOTE_PATH="${MK_REMOTE_PATH:-\$HOME/.cargo/bin:/usr/local/bin:/usr/bin:/bin}"
OR_KEY="${OPENROUTER_API_KEY:-}"
OR_MODELS="${MK_OR_MODELS:-poolside/laguna-xs.2:free nvidia/nemotron-3-ultra-550b-a55b:free}"
OPENCODE_MODEL="${MK_OPENCODE_MODEL:-opencode/deepseek-v4-flash-free}"
LLAMA_NICE="${MK_LLAMA_NICE:-19}"
LLAMA_DISABLE="${MK_LLAMA_DISABLE:-1}"
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
  # --tags so the runner's \`git describe\` (stamped into each job's runner metadata)
  # sees release tags; an explicit-refspec fetch does not auto-follow them otherwise.
  git fetch --tags origin "$BRANCH"
  git checkout "$BRANCH"
  git pull --ff-only origin "$BRANCH"
  echo "now at: \$(git rev-parse --short HEAD) \$(git log -1 --format=%s)"
REMOTE

log "ship prebuilt UI dist -> $HOST"
rsync -az --delete "$HERE/server/ui/dist/" "$HOST:$REMOTE_REPO/server/ui/dist/"

# Summary backends + local-model nice level live in a systemd drop-in (merged with
# the unit's other Environment= lines). MK_OPENAI_SERVERS uses a quote-free,
# space-free spec (key=value, ',' between fields, ';' between servers) because
# systemd's Environment= strips double quotes — JSON would arrive mangled. The
# drop-in carries the OpenRouter key, so it's written mode-600 and the whole file
# travels over ssh *stdin* (never argv, never the repo).
DROPIN="[Service]
Environment=MK_LLAMA_NICE=${LLAMA_NICE}
Environment=MK_LLAMA_DISABLE=${LLAMA_DISABLE}
Environment=MK_OPENCODE_BIN=%h/.opencode/bin/opencode
Environment=MK_OPENCODE_MODEL=${OPENCODE_MODEL}"
# Human-readable state of the local phi3.5 backend for the log lines below.
if [[ "$LLAMA_DISABLE" == "1" ]]; then LOCAL_DESC="local phi3.5 disabled"; else LOCAL_DESC="local phi3.5 (nice ${LLAMA_NICE})"; fi
# Scaffolding (the opencode agent in a container) restricts egress to an allowlist
# (kernel lore/git + the model providers) enforced by scaffold-proxy.py on the host;
# the container's HTTPS_PROXY points at it (see docs/opencode-egress.md). Default on;
# `MK_OPENCODE_PROXY= ./deploy.sh` (empty) disables it for unrestricted egress. The
# `-` (not `:-`) keeps an explicit empty value empty.
PROXY="${MK_OPENCODE_PROXY-http://host.containers.internal:8888}"
if [[ -n "$PROXY" ]]; then
  DROPIN+="
Environment=MK_OPENCODE_PROXY=${PROXY}"
fi
# opencode (free zen model via the opencode CLI) is the PRIMARY backend; the
# OpenRouter HTTP models (if a key is set) are added as non-primary. Label is the
# model basename minus ':free' (full id shows in the UI tooltip). %h = user home.
SERVERS_SPEC="label=opencode,model=${OPENCODE_MODEL},kind=opencode,primary=true"
if [[ -n "$OR_KEY" ]]; then
  read -ra _models <<< "$OR_MODELS"
  for m in "${_models[@]}"; do
    label="${m##*/}"; label="${label%:free}"
    entry="label=${label},base_url=https://openrouter.ai/api/v1,model=${m},api_key_env=OPENROUTER_API_KEY,primary=false"
    SERVERS_SPEC="${SERVERS_SPEC};$entry"
  done
  DROPIN+="
Environment=OPENROUTER_API_KEY=${OR_KEY}"
  log "configure backends on $HOST: opencode primary (${OPENCODE_MODEL}) + ${LOCAL_DESC} + OpenRouter [${OR_MODELS}]"
else
  log "configure backends on $HOST: opencode primary (${OPENCODE_MODEL}) + ${LOCAL_DESC}"
fi
DROPIN+="
Environment=MK_OPENAI_SERVERS=${SERVERS_SPEC}"
# One single-quoted remote command run by the login shell (no `bash -c` wrapper —
# that mis-parsed the multi-line script); `cat` reads the drop-in from ssh stdin.
printf '%s\n' "$DROPIN" | ssh "$HOST" '
  d="$HOME/.config/systemd/user/mackernel-server.service.d"
  mkdir -p "$d" && umask 077 && cat > "$d/extra.conf" && chmod 600 "$d/extra.conf"
  systemctl --user daemon-reload && echo "wrote $d/extra.conf"
'

# Install/refresh the scaffold egress allowlist proxy (scaffold-proxy.py, pulled with
# the repo above) as a systemd --user unit, so the container's HTTPS_PROXY has something
# to talk to. Skipped when the proxy is disabled (empty MK_OPENCODE_PROXY).
if [[ -n "$PROXY" ]]; then
  log "install scaffold egress proxy on $HOST"
  UNIT="[Unit]
Description=mackernel scaffold egress allowlist proxy
[Service]
# Bind all interfaces, not loopback: rootless podman (pasta) routes the container's
# host.containers.internal to a host-gateway addr (169.254.1.2), NOT host loopback, so a
# 127.0.0.1-bound proxy is refused and the agent gets zero egress. LAN-only exposure of a
# CONNECT proxy that only tunnels to the allowlist (port not WAN-forwarded) is acceptable.
ExecStart=/usr/bin/python3 %h/${REMOTE_REPO}/scaffold-proxy.py --bind 0.0.0.0:8888
Restart=on-failure
[Install]
WantedBy=default.target"
  printf '%s\n' "$UNIT" | ssh "$HOST" '
    d="$HOME/.config/systemd/user"
    mkdir -p "$d" && cat > "$d/mk-scaffold-proxy.service"
    systemctl --user daemon-reload
    systemctl --user enable mk-scaffold-proxy.service
    systemctl --user restart mk-scaffold-proxy.service
    systemctl --user is-active mk-scaffold-proxy.service && echo "scaffold-proxy active"
  '
fi

log "build binary + restart on $HOST"
ssh "$HOST" bash -seo pipefail <<REMOTE
  export PATH="$REMOTE_PATH:\$PATH"
  cd ~/"$REMOTE_REPO/server"
  echo "dist asset: \$(grep -o 'assets/[^\"]*' ui/dist/index.html)"
  cargo build --release 2>&1 | tail -3
  systemctl --user restart "$SERVICE"
  systemctl --user is-active "$SERVICE"

  # Smoke-test: unauthenticated /api/* must be rejected (auth is enforced). The server
  # needs a few seconds to bind (DuckDB open + migrations + seed), so poll until it
  # answers (curl exit != 0 -> 000) instead of a fixed sleep that races the startup.
  BIND="\$(systemctl --user show "$SERVICE" -p Environment | tr ' ' '\n' | sed -n 's/.*MK_SERVER_BIND=//p')"
  BIND="\${BIND:-127.0.0.1:8080}"
  CODE=000
  for i in \$(seq 1 20); do
    CODE="\$(curl -s -o /dev/null -w '%{http_code}' "http://\$BIND/api/jobs" || echo 000)"
    [[ "\$CODE" == "000" ]] || break   # server answered (any HTTP status) -> stop waiting
    sleep 1
  done
  echo "smoke-test http://\$BIND/api/jobs (no token) -> \$CODE"
  [[ "\$CODE" == "401" ]] || { echo "WARNING: expected 401 from unauthenticated /api/jobs, got \$CODE" >&2; exit 1; }
REMOTE

log "DEPLOY_OK ($HOST)"
