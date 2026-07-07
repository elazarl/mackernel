#!/usr/bin/env python3
"""Allowlisting HTTPS CONNECT proxy for the scaffold agent's container egress.

The opencode scaffold container gets normal rootless networking + HTTPS_PROXY pointing
here (mklib.scaffold_args / MK_OPENCODE_PROXY); this proxy only tunnels CONNECT to an
allowlist, so the agent can reach its model provider + the kernel's lore/git and nothing
else. Everything the agent uses is HTTPS, so a CONNECT-only proxy is enough — a plain
GET/POST (no CONNECT) is refused. See docs/opencode-egress.md.

    python3 scaffold-proxy.py --bind 127.0.0.1:8888
    python3 scaffold-proxy.py --selftest      # assert the allowlist logic

Extend the allowlist for a custom provider with MK_PROXY_ALLOW (space/comma-separated
host suffixes), e.g. MK_PROXY_ALLOW="api.mycorp.com".
"""
import os
import select
import socket
import sys
import threading

# Host suffixes the agent may CONNECT to. Kernel sources (lore + git.kernel.org) plus the
# OpenAI-compatible providers the UI offers (keep in sync with server/ui/src/lib/providers.ts).
ALLOW = [
    "lore.kernel.org",
    "git.kernel.org",
    "api.openai.com",
    "api.inference.crusoecloud.com",
    "api.inference.crusoecloud.xyz",
    "openrouter.inference.crusoecloud.com",
    "openrouter.ai",
    "api.groq.com",
    "api.fireworks.ai",
    "api.together.xyz",
    "api.deepinfra.com",
    "api.hyperbolic.xyz",
]
ALLOW += [h for h in os.environ.get("MK_PROXY_ALLOW", "").replace(",", " ").split() if h]


def allowed(host: str) -> bool:
    """True if `host` is one of the allowlisted hosts or a subdomain of one."""
    host = host.strip().strip(".").lower()
    return any(host == a or host.endswith("." + a) for a in ALLOW)


def _pipe(a: socket.socket, b: socket.socket) -> None:
    """Shuttle bytes both ways until either side closes."""
    conns = [a, b]
    try:
        while True:
            r, _, _ = select.select(conns, [], [], 60)
            if not r:
                break
            for s in r:
                data = s.recv(65536)
                if not data:
                    return
                (b if s is a else a).sendall(data)
    except OSError:
        pass
    finally:
        for s in conns:
            s.close()


def handle(client: socket.socket) -> None:
    try:
        client.settimeout(30)
        req = b""
        while b"\r\n" not in req:
            chunk = client.recv(4096)
            if not chunk:
                client.close()
                return
            req += chunk
        line = req.split(b"\r\n", 1)[0].decode("latin1")
        method, _, rest = line.partition(" ")
        target = rest.partition(" ")[0]
        if method.upper() != "CONNECT":
            client.sendall(b"HTTP/1.1 405 Method Not Allowed\r\n\r\n")
            client.close()
            return
        host, _, port = target.partition(":")
        port = int(port or 443)
        if not allowed(host):
            sys.stderr.write(f"scaffold-proxy: DENY {host}:{port}\n")
            sys.stderr.flush()
            client.sendall(b"HTTP/1.1 403 Forbidden\r\n\r\n")
            client.close()
            return
        try:
            upstream = socket.create_connection((host, port), timeout=30)
        except OSError as e:
            sys.stderr.write(f"scaffold-proxy: FAIL {host}:{port} {e}\n")
            client.sendall(b"HTTP/1.1 502 Bad Gateway\r\n\r\n")
            client.close()
            return
        client.settimeout(None)
        client.sendall(b"HTTP/1.1 200 Connection established\r\n\r\n")
        _pipe(client, upstream)
    except OSError:
        client.close()


def serve(host: str, port: int) -> None:
    srv = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind((host, port))
    srv.listen(64)
    sys.stderr.write(f"scaffold-proxy: listening on {host}:{port}; allow={ALLOW}\n")
    sys.stderr.flush()
    while True:
        conn, _ = srv.accept()
        threading.Thread(target=handle, args=(conn,), daemon=True).start()


def _selftest() -> None:
    assert allowed("lore.kernel.org")
    assert allowed("git.kernel.org")
    assert allowed("api.openai.com")
    assert allowed("sub.openrouter.ai")     # subdomain of an allowed host
    assert not allowed("evil.com")
    assert not allowed("notlore.kernel.org.evil.com")
    assert not allowed("kernel.org")        # parent domain is not a match
    print("scaffold-proxy selftest ok")


if __name__ == "__main__":
    if "--selftest" in sys.argv:
        _selftest()
    else:
        bind = "127.0.0.1:8888"
        if "--bind" in sys.argv:
            bind = sys.argv[sys.argv.index("--bind") + 1]
        h, _, p = bind.partition(":")
        serve(h or "127.0.0.1", int(p or 8888))
