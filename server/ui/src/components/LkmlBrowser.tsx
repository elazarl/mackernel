import { useMemo, useState } from "react";
import { listLkmlPatches, LkmlPatch } from "../api";
import { LKML_LISTS } from "../lkml-lists";
import { Modal } from "./ui/Modal";

// Browse lore.kernel.org on demand: step 1 picks a mailing list (searchable over the
// bundled LKML_LISTS); step 2 lists that list's latest patch cover letters from
// new.atom. Picking a patch hands its cover-letter body + thread URL to onPick.
export function LkmlBrowser(
  { onPick, onClose }: { onPick: (p: LkmlPatch) => void; onClose: () => void },
) {
  const [filter, setFilter] = useState("");
  const [list, setList] = useState<string | null>(null);
  const [patches, setPatches] = useState<LkmlPatch[] | null>(null);
  const [error, setError] = useState("");

  const lists = useMemo(() => {
    const q = filter.trim().toLowerCase();
    if (!q) return LKML_LISTS;
    return LKML_LISTS.filter((l) => l.name.includes(q) || (l.desc ?? "").toLowerCase().includes(q));
  }, [filter]);

  const openList = async (name: string) => {
    setList(name); setPatches(null); setError("");
    try { setPatches(await listLkmlPatches(name)); }
    catch { setError("Couldn't fetch patches (lore unreachable?)."); setPatches([]); }
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
            <h2>{list} · latest patches</h2>
            <button className="linkbtn" onClick={() => { setList(null); setPatches(null); setError(""); }}>← lists</button>
          </div>
          {patches == null ? <p className="text-muted">loading…</p>
            : error ? <p className="text-muted">{error}</p>
            : patches.length === 0 ? <p className="text-muted">no patch cover letters in the latest messages</p>
            : (
              <ul className="m-0 max-h-[420px] list-none overflow-auto p-0">
                {patches.map((p) => (
                  <li key={p.url} className={rowCls} onClick={() => onPick(p)}>
                    <div className="flex flex-wrap items-center gap-1.5">
                      <span>{p.title}</span>
                      <a className="ml-auto text-[.82em] text-accent no-underline" href={p.url}
                        target="_blank" rel="noreferrer" onClick={(e) => e.stopPropagation()}>lore ↗</a>
                    </div>
                  </li>
                ))}
              </ul>
            )}
        </>
      )}
    </Modal>
  );
}
