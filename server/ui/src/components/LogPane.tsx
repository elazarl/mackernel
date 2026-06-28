import { useEffect, useState } from "react";
import { getLog } from "../api";

// One log pane: fetches the chosen log kind (optionally for a compare variant) and
// refetches as the job advances. Compare mode renders two of these side by side.
export function LogPane({ id, kind, variant, status }:
  { id: number; kind: string; variant?: string; status?: string }) {
  const [text, setText] = useState("");
  useEffect(() => { getLog(id, kind, variant).then(setText); }, [id, kind, variant, status]);
  return <pre className="log">{text}</pre>;
}
