# Restricting the scaffolding container's network egress

The "scaffold a reproducer" feature runs the `opencode` agent inside a podman
container (`mklib.resolve_image(opencode=True)`) with the kernel source mounted
**read-only**. The agent must reach its model gateway, but nothing else — the
mounted source must not be exfiltrable to an arbitrary host. This note records the
podman options considered and the mechanism we use.

## What opencode connects to

`opencode` (the `opencode/...` "zen" free models) talks HTTPS to the opencode
gateway (`opencode.ai` / its API host). It is a Node/undici client, so it honors
the `HTTPS_PROXY` / `HTTP_PROXY` environment variables.

## Options considered (podman)

| Option | Verdict |
|--------|---------|
| `--network=none` (the build-container hardening, `mklib.hardening_args`) | Blocks all network — opencode can't reach its API. Unusable for the agent. |
| Default rootless networking (pasta / slirp4netns) | Full outbound NAT, no filtering. Lets the agent reach any host. Too open. |
| nftables / netavark IP allowlist on a dedicated podman net | A real L3 block, but the gateway is CDN-fronted, so the IP set rotates and is brittle to pin; also wants more privilege than the rootless CentOS home box gives cheaply. |
| **Host-side allowlisting HTTPS proxy** (chosen) | Domain-level allowlist survives CDN IP churn; opencode honors `HTTPS_PROXY`; no in-container privilege needed. |

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

    # /etc/tinyproxy/allow.txt
    (^|\.)opencode\.ai$

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
