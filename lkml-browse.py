#!/usr/bin/env python3
"""List recent patch cover letters on a lore.kernel.org list via its public-inbox git
mirror -- the only reachable bulk source past lore's Anubis bot-protection (new.atom is
hard-capped at ~25 with no pagination, and search/HTML are blocked).

Each commit in the mirror is one message (tree blob `m` = raw RFC822). We page through
the latest epoch newest-first, keep patch-series roots (cover letters / standalone
patches), and print JSON: {"patches":[{title,url,body}], "more":bool, "next":int, "epoch":int}.

Usage: lkml-browse.py <list> [skip] [page] [epoch]
  epoch < 0 (default) probes for the latest epoch; pass a cached value to skip probing.
"""
from __future__ import annotations

import email
import email.policy
import json
import shutil
import subprocess
import sys
import tempfile

BASE = "https://lore.kernel.org"
UA = "git/2.43"  # Anubis allows bot UAs, challenges browser (Mozilla/...) ones
PAGE = 50        # messages scanned per page (cover letters are a subset)


def _git(*args: str, **kw) -> subprocess.CompletedProcess:
    return subprocess.run(["git", "-c", f"http.userAgent={UA}", *args],
                          capture_output=True, **kw)


def epoch_exists(lst: str, n: int) -> bool:
    return _git("ls-remote", f"{BASE}/{lst}/git/{n}.git", timeout=25).returncode == 0


def latest_epoch(lst: str) -> int:
    """public-inbox splits an archive into epochs 0.git, 1.git, …; the highest holds the
    newest mail. Probe upward until one is missing."""
    n = 0
    while n < 20 and epoch_exists(lst, n):
        n += 1
    return max(0, n - 1)


def is_cover(subject: str) -> bool:
    """A patch-series root: `[PATCH 0/N]`, `[PATCH 1/1]`, or a single `[PATCH …]` with no
    n/m. Drops follow-on `n/m` patches and replies."""
    s = subject.strip()
    if s.startswith("Re:") or s.startswith("RE:"):
        return False
    i = s.find("[PATCH")
    if i < 0:
        return False
    close = s.find("]", i)
    tag = s[i: close if close > 0 else len(s)]
    for part in tag.replace("[", " ").replace("]", " ").split():
        a, slash, b = part.partition("/")
        if slash and a.isdigit() and b.isdigit():
            n, m = int(a), int(b)
            return n == 0 or (n == 1 and m == 1)
    return True  # no n/m -> standalone patch


def body_text(msg) -> str:
    """Decoded plain-text body (first text/plain part of a multipart)."""
    if msg.is_multipart():
        for p in msg.walk():
            if p.get_content_type() == "text/plain":
                return p.get_content()
        return ""
    return msg.get_content()


def browse(lst: str, skip: int, page: int, epoch: int) -> dict:
    if epoch < 0:
        epoch = latest_epoch(lst)
    url = f"{BASE}/{lst}/git/{epoch}.git"
    depth = skip + page
    tmp = tempfile.mkdtemp(prefix="mk-lkml-")
    try:
        if _git("init", "-q", "--bare", tmp).returncode != 0:
            raise RuntimeError("git init failed")
        if _git("--git-dir", tmp, "fetch", "-q", "--depth", str(depth), url, "HEAD").returncode != 0:
            raise RuntimeError(f"git fetch {url} failed")
        revs = _git("--git-dir", tmp, "rev-list", "--max-count", str(depth), "FETCH_HEAD",
                    check=True, text=True).stdout.split()
        window = revs[skip:skip + page]
        patches = []
        for c in window:
            raw = _git("--git-dir", tmp, "cat-file", "-p", f"{c}:m", check=True).stdout
            msg = email.message_from_bytes(raw, policy=email.policy.default)
            subject = " ".join(str(msg.get("Subject", "")).split())
            if not is_cover(subject):
                continue
            mid = str(msg.get("Message-ID", "")).strip().lstrip("<").rstrip(">")
            if not mid:
                continue
            try:
                body = body_text(msg)
            except Exception:
                body = ""
            patches.append({"title": subject, "url": f"{BASE}/{lst}/{mid}/", "body": body})
        # A full depth of commits means the epoch holds more behind this window. (Older
        # epochs aren't paged into -- a known limitation for very deep history.)
        more = len(revs) >= depth
        return {"patches": patches, "more": more, "next": skip + page, "epoch": epoch}
    finally:
        shutil.rmtree(tmp, ignore_errors=True)


def _selftest() -> None:
    assert is_cover("[PATCH 0/9] treewide: cleanup")
    assert is_cover("[PATCH v2] mm: single patch")
    assert is_cover("[PATCH net 1/1] tcp: bound timers")
    assert not is_cover("[PATCH 2/9] mm: one of many")
    assert not is_cover("Re: [PATCH 0/2] fix")
    assert not is_cover("just a normal email")
    print("ok  lkml-browse selftest")


def main() -> None:
    if len(sys.argv) > 1 and sys.argv[1] == "--selftest":
        return _selftest()
    lst = sys.argv[1]
    skip = int(sys.argv[2]) if len(sys.argv) > 2 else 0
    page = int(sys.argv[3]) if len(sys.argv) > 3 else PAGE
    epoch = int(sys.argv[4]) if len(sys.argv) > 4 else -1
    print(json.dumps(browse(lst, skip, page, epoch)))


if __name__ == "__main__":
    main()
