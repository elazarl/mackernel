import { Select, Button, Popover, ListBox, ListBoxItem } from "react-aria-components";
import type { SummarizerInfo } from "../api";

type Server = NonNullable<SummarizerInfo["servers"]>[number];

// Global model switcher: which backend's summaries the whole app shows. Replaces the
// old native <select> with a React Aria Select so the dropdown can render rich rows
// (backend label + "primary" badge on top, the full model id muted underneath) while
// staying keyboard-navigable and ARIA-correct. Controlled by `view`.
export function ModelSwitcher(
  { servers, view, onChange }:
  { servers: Server[]; view: string; onChange: (label: string) => void },
) {
  const selected = servers.find((s) => s.label === view) ?? servers[0];
  return (
    <Select
      aria-label="summary model"
      selectedKey={view}
      onSelectionChange={(k) => onChange(String(k))}
    >
      <Button
        className="inline-flex items-center gap-1.5 rounded-md border border-border bg-subtle px-2 py-1
                   font-mono text-xs text-fg cursor-pointer outline-none
                   data-[hovered]:border-accent data-[focus-visible]:border-accent
                   data-[focus-visible]:ring-2 data-[focus-visible]:ring-accent/50"
      >
        <span className="max-w-[220px] truncate" title={selected?.model}>
          {selected?.label}{selected?.primary ? " (primary)" : ""}
        </span>
        <span aria-hidden className="text-muted text-[10px]">▾</span>
      </Button>
      <Popover
        offset={4}
        className="rounded-lg border border-border bg-card p-1 shadow-xl"
      >
        <ListBox className="outline-none min-w-[var(--trigger-width)] max-w-[360px]">
          {servers.map((s) => (
            <ListBoxItem
              key={s.label}
              id={s.label}
              textValue={s.label}
              className="group flex cursor-pointer flex-col gap-0.5 rounded-md px-2.5 py-1.5 outline-none
                         data-[hovered]:bg-subtle data-[focused]:bg-subtle data-[selected]:bg-subtle"
            >
              <span className="flex items-center gap-1.5 text-sm text-fg">
                <span className="font-medium">{s.label}</span>
                {s.primary && (
                  <span className="rounded-full border border-accent px-1.5 text-[10px] leading-tight text-accent">
                    primary
                  </span>
                )}
                <span className="ml-auto text-accent opacity-0 group-data-[selected]:opacity-100">✓</span>
              </span>
              <span className="font-mono text-[11px] text-muted">{s.model}</span>
            </ListBoxItem>
          ))}
        </ListBox>
      </Popover>
    </Select>
  );
}
