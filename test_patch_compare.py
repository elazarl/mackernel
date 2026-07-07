#!/usr/bin/env python3
"""Self-check for patch-compare / thread-compare bundle parsing + variant split.
No kernel build: exercises only the pure parse/branch logic in run-kernel.py.

    python3 test_patch_compare.py
"""
import importlib.util
import sys
import tempfile
from pathlib import Path

HERE = Path(__file__).resolve().parent
spec = importlib.util.spec_from_file_location("runkernel", HERE / "run-kernel.py")
rk = importlib.util.module_from_spec(spec)
sys.modules["runkernel"] = rk  # @dataclass resolves field types via sys.modules
spec.loader.exec_module(rk)


def parse(text: str) -> dict:
    with tempfile.NamedTemporaryFile("w", suffix=".md", delete=False) as f:
        f.write(text)
        p = Path(f.name)
    meta = rk.parse_bundle(p).meta
    rk.enforce_hardened(meta)
    return meta


def test_tools_parse_and_registry():
    # `tools:` is kept in meta as a space-separated string the runner splits + validates.
    meta = parse("---\ncommit: v6.12\ntools: perf bpftool\n---\n")
    assert meta["tools"] == "perf bpftool"
    assert meta["url"] == rk.KERNEL_URL          # requesting a tree hardens the url
    # Every name a bundle may request must be in the build registry; junk is not.
    for name in meta["tools"].split():
        assert name in rk.TOOLS, f"{name} missing from TOOLS registry"
    assert "definitely-not-a-tool" not in rk.TOOLS


def test_truthy():
    assert rk._truthy("true") and rk._truthy("YES") and rk._truthy("1") and rk._truthy("on")
    assert not rk._truthy("false") and not rk._truthy("") and not rk._truthy("no")


def test_patch_compare():
    meta = parse("---\ncommit: v6.12\npatch: https://x/fix.patch\npatch-compare: true\n---\n")
    assert meta["patch-compare"] == "true"
    assert meta["url"] == rk.KERNEL_URL          # hardened forces Linus's tree
    base, patched = rk.compare_variants(meta)
    assert "patch" not in base and base["commit"] == "v6.12"
    assert patched["patch"] == "https://x/fix.patch"


def test_patch_compare_needs_patch():
    # patch-compare without a patch: -> no comparison, single run.
    meta = parse("---\ncommit: v6.12\npatch-compare: true\n---\n")
    assert rk.compare_variants(meta) is None


def test_thread_compare():
    url = "https://lore.kernel.org/lkml/abc@mail/"
    meta = parse(f"---\ncommit: v6.12\nthread-compare: {url}\n---\n")
    assert meta["thread-compare"] == url
    assert meta["url"] == rk.KERNEL_URL          # thread-compare also hardens
    base, patched = rk.compare_variants(meta)
    assert "thread-compare" not in base and "thread" not in base
    assert patched["thread"] == url and patched["commit"] == "v6.12"


def test_commit_compare():
    meta = parse("---\ncommit-compare: v6.11 v6.12\n---\n")
    assert meta["commit-compare"] == "v6.11 v6.12"
    assert meta["url"] == rk.KERNEL_URL          # commit-compare also hardens
    base, patched = rk.compare_variants(meta)
    assert base["commit"] == "v6.11" and "commit-compare" not in base
    assert patched["commit"] == "v6.12" and "commit-compare" not in patched


def test_commit_compare_needs_two():
    # commit-compare with one token -> no comparison, single run.
    meta = parse("---\ncommit-compare: v6.11\n---\n")
    assert rk.compare_variants(meta) is None


def test_no_compare():
    meta = parse("---\ncommit: v6.12\n---\n")
    assert rk.compare_variants(meta) is None


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print(f"ok  {name}")
    print("all passed")
