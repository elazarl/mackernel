import { useEffect, useMemo, useState } from "react";
import { compareMode, githubTree, KERNEL_REPO, KERNEL_URL, ParsedBundle, rolesOf } from "../bundle";
import { Code, langOf } from "./Code";
import { SegTabs } from "./ui/Tabs";

// Structured view of a pasted bundle: frontmatter metadata + one tab per role present.
export function BundlePreview({ parsed }: { parsed: ParsedBundle }) {
  const roles = useMemo(() => rolesOf(parsed), [parsed]);
  const [tab, setTab] = useState(roles[0] ?? "");
  useEffect(() => { if (!roles.includes(tab)) setTab(roles[0] ?? ""); }, [roles, tab]);

  const get = (k: string) => parsed.meta.find((m) => m.key === k)?.value;
  const commit = get("commit"), arch = get("arch"), patch = get("patch"), url = get("url"), tools = get("tools");
  const cmp = compareMode(parsed), threadCompare = get("thread-compare"), commitCompare = get("commit-compare");
  const requestsKernel = !!(commit || patch || url || threadCompare || commitCompare);

  const Row = ({ dt, children }: { dt: string; children: React.ReactNode }) => (
    <div className="flex gap-2 py-0.5">
      <dt className="min-w-16 text-muted">{dt}</dt>
      <dd className="m-0 font-mono break-all">{children}</dd>
    </div>
  );

  return (
    <div className="mb-2">
      {parsed.meta.length ? (
        <dl className="mb-2">
          {requestsKernel && <Row dt="repo">{KERNEL_REPO}</Row>}
          {commit && <Row dt="commit"><a className="text-accent" href={githubTree(commit)} target="_blank" rel="noreferrer">{commit}</a></Row>}
          {arch && <Row dt="arch">{arch}</Row>}
          {patch && <Row dt="patch">{patch}</Row>}
          {cmp === "patch" && <Row dt="compare">baseline vs patched (with / without patch)</Row>}
          {threadCompare && <Row dt="thread-compare">baseline vs series · {threadCompare}</Row>}
          {cmp === "commit" && <Row dt="commit-compare">baseline vs patched · {commitCompare}</Row>}
          {tools && <Row dt="tools">{tools} <span className="text-muted">· built from the tree, shipped into the guest</span></Row>}
          {url && url !== KERNEL_URL && <Row dt="url"><span className="text-muted"><s>{url}</s> · ignored</span></Row>}
        </dl>
      ) : (
        <p className="text-muted">no metadata block (builds LINUX_SRC as-is)</p>
      )}

      {roles.length ? (
        <>
          <SegTabs items={roles.map((r) => ({ key: r, label: r }))} value={tab} onChange={setTab} label="bundle roles" />
          {parsed.files.filter((f) => f.role === tab).map((f, idx) => (
            <div key={idx}>
              <div className="my-1 font-mono text-xs text-muted">{f.name}</div>
              <Code body={f.body} lang={langOf(f.name, f.role)} />
            </div>
          ))}
        </>
      ) : (
        <p className="text-muted">no code blocks detected</p>
      )}
    </div>
  );
}
