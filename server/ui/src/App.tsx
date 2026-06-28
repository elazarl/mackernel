import { useEffect, useState } from "react";
import { hasToken } from "./api";
import "./lib/theme"; // applies the saved theme at module load, before first paint
import { Unlock } from "./components/Unlock";
import { Dashboard } from "./components/Dashboard";

export function App() {
  const [authed, setAuthed] = useState(hasToken());

  // A rejected token (401) anywhere clears it and bounces back to the unlock gate.
  useEffect(() => {
    const onUnauthorized = () => setAuthed(false);
    window.addEventListener("mk-unauthorized", onUnauthorized);
    return () => window.removeEventListener("mk-unauthorized", onUnauthorized);
  }, []);

  if (!authed) return <Unlock onUnlock={() => setAuthed(true)} />;
  return <Dashboard />;
}
