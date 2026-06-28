import { useEffect, useState } from "react";
import { highlight } from "../api";

// Pick an arborium language from a bundle file. Only c/bash are built into the
// server; everything else renders as plain text.
export function langOf(name: string, role: string): string | null {
  if (/\.(c|h)$/i.test(name)) return "c";
  // kconfig (.config) is A=y with # comments — bash highlights those well enough.
  if (role === "init" || role === "kconf" || /\.(sh|bash|config)$/i.test(name)) return "bash";
  return null;
}

// A code block, syntax-highlighted server-side via arborium. Falls back to plain
// text while the highlight request is in flight or for unsupported languages.
export function Code({ body, lang }: { body: string; lang: string | null }) {
  const [html, setHtml] = useState<string | null>(null);
  useEffect(() => {
    let live = true;
    if (lang) highlight(lang, body).then((h) => { if (live) setHtml(h); });
    else setHtml(null);
    return () => { live = false; };
  }, [body, lang]);
  return html
    ? <pre className="arb log"><code dangerouslySetInnerHTML={{ __html: html }} /></pre>
    : <pre className="log">{body}</pre>;
}
