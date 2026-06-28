import type { ReactNode } from "react";
import { Tabs, TabList, Tab } from "react-aria-components";

// Accessible segmented tab strip (arrow-key nav + ARIA, courtesy of React Aria).
// Selection only — panel content is rendered by the caller from `value`, since the
// panels (log panes, issue blocks, code) are driven by other state.
export function SegTabs(
  { items, value, onChange, label = "tabs" }:
  { items: { key: string; label: ReactNode }[]; value: string; onChange: (k: string) => void; label?: string },
) {
  return (
    <Tabs selectedKey={value} onSelectionChange={(k) => onChange(String(k))}>
      <TabList aria-label={label} className="flex flex-wrap gap-1 mb-2">
        {items.map((it) => (
          <Tab
            key={it.key}
            id={it.key}
            className="cursor-pointer rounded-md px-2.5 py-1 text-sm bg-subtle text-fg outline-none
                       data-[selected]:bg-tab-active data-[selected]:text-white
                       data-[focus-visible]:ring-2 data-[focus-visible]:ring-accent"
          >
            {it.label}
          </Tab>
        ))}
      </TabList>
    </Tabs>
  );
}
