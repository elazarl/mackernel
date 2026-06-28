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


def test_no_compare():
    meta = parse("---\ncommit: v6.12\n---\n")
    assert rk.compare_variants(meta) is None


# A minimal public-inbox thread Atom feed: a 0/2 cover (no diff) + two patch mails,
# one of which carries a diff with entity-escaped chars and a tab-indented line.
_THREAD_ATOM = """<feed>
<entry><title>[PATCH 0/2] fix the thing</title>
<author><name>Pat Submitter</name><email>pat@example.com</email></author>
<updated>2026-06-25T12:00:00Z</updated>
<content type="xhtml"><div><pre>This series fixes a thing.
See &lt;https://example.com&gt; for context.</pre></div></content></entry>
<entry><title>[PATCH 1/2] core: fix off-by-one</title>
<author><name>Pat Submitter</name><email>pat@example.com</email></author>
<updated>2026-06-25T12:00:01Z</updated>
<content type="xhtml"><div><pre>Signed-off-by: Pat Submitter &lt;pat@example.com&gt;
---
diff --git a/core.c b/core.c
--- a/core.c
+++ b/core.c
@@ -1,3 +1,3 @@ int main(void)
-\tint x = len;
+\tint x = len - 1;
</pre></div></content></entry>
<entry><title>[PATCH 2/2] docs: note the fix</title>
<author><name>Pat Submitter</name><email>pat@example.com</email></author>
<updated>2026-06-25T12:00:02Z</updated>
<content type="xhtml"><div><pre>no diff here, just prose</pre></div></content></entry>
</feed>""".replace("\\t", "\t")


def test_atom_to_messages_reconstructs_subject_and_body():
    msgs = rk.atom_to_messages(_THREAD_ATOM)
    assert len(msgs) == 3
    assert msgs[0]["Subject"] == "[PATCH 0/2] fix the thing"
    assert msgs[0]["From"] == "Pat Submitter <pat@example.com>"
    body0 = rk._message_text(msgs[0])
    # entity-escaped angle brackets come back literal
    assert "<https://example.com>" in body0


def test_select_patches_keeps_diffs_in_order():
    patches = rk._select_patches(rk.atom_to_messages(_THREAD_ATOM))
    # cover (0/2, no diff) and the prose 2/2 (no diff) are dropped; only 1/2 remains.
    assert len(patches) == 1
    assert patches[0]["Subject"] == "[PATCH 1/2] core: fix off-by-one"
    body = rk._message_text(patches[0])
    assert "diff --git a/core.c b/core.c" in body
    assert "\tint x = len - 1;" in body          # tab indentation preserved


if __name__ == "__main__":
    for name, fn in sorted(globals().items()):
        if name.startswith("test_"):
            fn()
            print(f"ok  {name}")
    print("all passed")
