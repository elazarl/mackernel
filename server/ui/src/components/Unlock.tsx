import { useState } from "react";
import { Button, Input, TextField } from "react-aria-components";
import { setToken } from "../api";

// First-visit gate: ask for the commit hash of the v7.1 tag and use it as the bearer
// token for /api/*. Remounting Dashboard on unlock kicks off a fresh poll.
export function Unlock({ onUnlock }: { onUnlock: () => void }) {
  const [value, setValue] = useState("");
  const submit = () => {
    if (!value.trim()) return;
    setToken(value);
    onUnlock();
  };
  return (
    <div className="mx-auto max-w-[1200px] px-6 py-4">
      <h1>Kernel Reproducer Runner</h1>
      <section className="card max-w-[460px]">
        <h2>Unlock</h2>
        <p className="text-muted">Enter the commit hash of the <code className="text-fg">v7.1</code> tag to continue.</p>
        <TextField aria-label="v7.1 commit hash" value={value} onChange={setValue} autoFocus>
          <Input
            type="password"
            placeholder="v7.1 commit hash"
            onKeyDown={(e) => { if (e.key === "Enter") submit(); }}
            className="w-full rounded-md border border-border bg-bg px-2 py-2 font-mono text-fg
                       outline-none focus:border-accent"
          />
        </TextField>
        <Button className="btn mt-2" onPress={submit}>Unlock</Button>
      </section>
    </div>
  );
}
