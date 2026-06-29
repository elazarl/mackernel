import { useMemo, useState } from "react";
import { listLkmlPatches, LkmlPatch } from "../api";
import { LKML_LISTS } from "../lkml-lists";
import { Modal } from "./ui/Modal";

// Browse lore.kernel.org on demand: step 1 picks a mailing list (searchable over the
// bundled LKML_LISTS); step 2 pages through that list's patch cover letters from its git
// mirror (newest first), searchable and with "load more". Picking a patch hands its
// cover-letter body + thread URL to onPick.
export function LkmlBrowser(
  { onPick, onScaffold, onClose }:
  { onPick: (p: LkmlPatch) => void; onScaffold: (p: LkmlPatch) => void; onClose: () => void },
) {
  const [filter, setFilter] = useState("");
  const [list, setList] = useState<string | null>(null);
  const [patches, setPatches] = useState<LkmlPatch[]>([]);
  const [next, setNext] = useState(0);
  const [more, setMore] = useState(false);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");
  const [pfilter, setPfilter] = useState(""); // search within the loaded patches

  const lists = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return LKML_LISTS;
    return LKML_LISTS.filter((l) => l.name.includes(q) || (l.desc ?? "").toLowerCase().includes(q));
  }, [filter]);

  const shownPatches = useMemo(() => {
    const q = pfilter.trim().toLowerCase();
    return q ? patches.filter((p) => p.title.toLowerCase().includes(q)) : patches;
  }, [patches, pfilter]);

  // Fetch one page; `skip === 0` (re)starts the list, otherwise appends.
  const load = async (name: string, skip: number) => {
    setLoading(true); setError("");
    try {
      const page = await listLkmlPatches(name, skip);
      setPatches((prev) => (skip === 0 ? page.patches : [...prev, ...page.patches]));
      setNext(page.next); setMore(page.more);
    } catch {
      setError("Couldn't fetch patches (lore unreachable?).");
      if (skip === 0) setMore(false);
    } finally {
      setLoading(false);
    }
  };

  const openList = (name: string) => {
    setList(name); setPatches([]); setNext(0); setMore(false); setPfilter("");
    load(name, 0);
  };

  const inputCls = "mb-2.5 w-full box-border rounded-md border border-border bg-bg p-[9px] font-mono text-fg outline-none focus:border-accent";
  const rowCls = "flex cursor-pointer flex-col gap-0.5 rounded-md px-2 py-1.5 hover:bg-subtle";

  return (
    <Modal onClose={onClose} label="Browse LKML">
      {list == null ? (
        <>
          <h2>Browse LKML · pick a list</h2>
          <input className={inputCls} autoFocus placeholder="filter mailing lists…"
            value={filter} onChange={(e) => setFilter(e.target.value)} />
          <ul className="m-0 max-h-[420px] list-none overflow-auto p-0">
            {lists.map((l) => (
              <li key={l.name} className={rowCls} onClick={() => openList(l.name)}>
                <div className="flex flex-wrap items-baseline gap-1.5">
                  <strong>{l.name}</strong>
                  {l.desc && <span className="text-[.82em] text-muted">{l.desc}</span>}
                </div>
              </li>
            ))}
            {lists.length === 0 && <li className="px-2 py-1.5 text-muted">no lists match</li>}
          </ul>
        </>
      ) : (
        <>
          <div className="flex items-center justify-between">
            <h2>{list} · patches</h2>
            <button className="linkbtn" onClick={() => setList(null)}>← lists</button>
          </div>
          <input className={inputCls} autoFocus placeholder="search loaded patches…"
            value={pfilter} onChange={(e) => setPfilter(e.target.value)} />
          {error && <p className="text-muted">{error}</p>}
          <ul className="m-0 max-h-[420px] list-none overflow-auto p-0">
            {shownPatches.map((p) => (
              <li key={p.url} className={rowCls} onClick={() => onPick(p)}>
                <div className="flex flex-wrap items-center gap-1.5">
                  <span>{p.title}</span>
                  {/* Scaffold: let opencode write a reproducer from this series instead
                      of opening the raw cover letter. Stops row-click (= pick). */}
                  <button className="chip ml-auto" title="Let opencode write a reproducer for this series"
                    onClick={(e) => { e.stopPropagation(); onScaffold(p); }}>Scaffold ✨</button>
                  <a className="text-[.82em] text-accent no-underline" href={p.url}
                    target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>lore ↗</a>
                </div>
              </li>
            ))}
            {!loading && !error && shownPatches.length === 0 && (
              <li className="px-2 py-1.5 text-muted">
                {patches.length === 0 ? "no patch cover letters found" : "none match your search"}
              </li>
            )}
          </ul>
          <div className="mt-2 flex items-center gap-3">
            {more && (
              <button className="chip" disabled={loading} onClick={() => list && load(list, next)}>
                {loading ? "loading…" : "Load more"}
              </button>
            )}
            {loading && !more && <span className="text-muted">loading…</span>}
            {patches.length > 0 && <span className="text-[.82em] text-muted">{patches.length} loaded</span>}
          </div>
        </>
      )}
    </Modal>
  );
}
