# Restricting the scaffolding container's network egress

The "scaffold a reproducer" feature runs the `opencode` agent inside a podman
container (`mklib.resolve_image(opencode=True)`) with the kernel source mounted
**read-only**. The agent must reach its model gateway, but nothing else — the
mounted source must not be exfiltrable to an arbitrary host. This note records the
podman options considered and the mechanism we use.

## What opencode connects to

Scaffolding runs against the user's own OpenAI-compatible endpoint (no free tier):
`opencode` talks HTTPS to whichever provider the user picked (its base URL, e.g.
`api.inference.crusoecloud.com`). It is a Node/undici client, so it honors the
`HTTPS_PROXY` / `HTTP_PROXY` environment variables. The allowlist below must therefore
permit every provider host the UI offers (and any "Custom" host an operator allows).

## Options considered (podman)

| Option | Verdict |
|--------|---------|
| `--network=none` (the build-container hardening, `mklib.hardening_args`) | Blocks all network — opencode can't reach its API. Unusable for the agent. |
| Default rootless networking (pasta / slirp4netns) | Full outbound NAT, no filtering. Lets the agent reach any host. Too open. |
| nftables / netavark IP allowlist on a dedicated podman net | A real L3 block, but the gateway is CDN-fronted, so the IP set rotates and is brittle to pin; also wants more privilege than the rootless CentOS home box gives cheaply. |
| **Host-side allowlisting HTTPS proxy** (chosen) | Domain-level allowlist survives CDN IP churn; opencode honors `HTTPS_PROXY`; no in-container privilege needed. |

## Shipped implementation

`scaffold-proxy.py` in the repo root is exactly the ~small allowlisting CONNECT proxy
described below (stdlib only, default-deny, host-suffix allowlist baked in — the kernel
lore/git hosts plus the providers in `server/ui/src/lib/providers.ts`). `deploy.sh`
installs it as a `mk-scaffold-proxy.service` systemd --user unit on the host and points
the container's `MK_OPENCODE_PROXY` at `http://host.containers.internal:8888` by default;
`MK_OPENCODE_PROXY= ./deploy.sh` (empty) disables it for open egress. Extend the allowlist
for a custom provider with `MK_PROXY_ALLOW="host.example.com"`. The `tinyproxy` recipe
below is an equivalent alternative.

## Chosen mechanism: host allowlist proxy

Run a small forward proxy on the host, bound to loopback, that only permits
`CONNECT` to the opencode host(s). The container runs with normal rootless
networking and `HTTPS_PROXY`/`HTTP_PROXY` pointing at the proxy
(`mklib.scaffold_args` injects them from `MK_OPENCODE_PROXY`). All opencode
traffic tunnels through the proxy; its allowlist is the enforcement point.

Reach the host from a rootless container via `host.containers.internal`, e.g.:

    MK_OPENCODE_PROXY=http://host.containers.internal:8888

### A minimal allowlisting proxy

`tinyproxy` with a default-deny filter (one host per line in the filter file):

    # /etc/tinyproxy/tinyproxy.conf
    Port 8888
    Listen 127.0.0.1
    Filter "/etc/tinyproxy/allow.txt"
    FilterDefaultDeny Yes
    FilterExtended On

    # /etc/tinyproxy/allow.txt -- the OpenAI-compatible provider hosts the UI offers
    # (server/ui/src/lib/providers.ts). Add a "Custom" provider's host here too.
    #
    # lore.kernel.org lets the scaffold agent pull extra patch/thread context
    # (git.kernel.org is the canonical source it may also read); keep them allowed.
    (^|\.)lore\.kernel\.org$
    (^|\.)git\.kernel\.org$
    (^|\.)api\.openai\.com$
    (^|\.)api\.inference\.crusoecloud\.com$
    (^|\.)api\.inference\.crusoecloud\.xyz$
    (^|\.)openrouter\.inference\.crusoecloud\.com$
    (^|\.)openrouter\.ai$
    (^|\.)api\.groq\.com$
    (^|\.)api\.fireworks\.ai$
    (^|\.)api\.together\.xyz$
    (^|\.)api\.deepinfra\.com$
    (^|\.)api\.hyperbolic\.xyz$

Start it on the host (or as a sibling systemd unit beside `mackernel-server`) and
set `MK_OPENCODE_PROXY` in the server's environment / systemd drop-in (see
`deploy.sh`). Any `tinyproxy`-equivalent (a ~30-line Go/Python CONNECT proxy that
checks the host against an allowlist) works identically.

## Known limitation (bypass)

The proxy is honored via environment variables, **not enforced at the network
layer** — a process that opens a direct socket ignores `HTTPS_PROXY`. For the
trusted-but-sandboxed model agent this is the pragmatic ceiling. If hard
enforcement is required, add a host nftables rule that drops the container's
egress except to the proxy port (the container already has no default route it
needs other than the proxy). Tracked as a hardening follow-up.
