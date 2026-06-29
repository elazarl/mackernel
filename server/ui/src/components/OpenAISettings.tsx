import { useState } from "react";
import {
  Button, ComboBox, Input, Label, ListBox, ListBoxItem, Popover, TextField,
} from "react-aria-components";
import { listModels } from "../api";
import { getCreds, setCreds } from "../lib/creds";
import { PROVIDERS, providerForBaseUrl } from "../lib/providers";
import { Modal } from "./ui/Modal";

// Settings for the OpenAI-compatible endpoint scaffolding runs against. The user picks a
// known provider (which fills the base URL), enters their API key, loads the provider's
// model list, and picks a model. Saved to localStorage (lib/creds). Scaffolding is gated
// on all three being set — both here and in the backend.
const inputCls =
  "w-full rounded-md border border-border bg-bg px-2 py-2 font-mono text-sm text-fg outline-none focus:border-accent";

// A searchable single-select (React Aria ComboBox): type to filter, pick from the list.
function SearchSelect(
  { label, items, selectedKey, onSelect, placeholder, isDisabled }:
  { label: string; items: { id: string; name: string }[]; selectedKey: string;
    onSelect: (id: string) => void; placeholder?: string; isDisabled?: boolean },
) {
  return (
    <ComboBox
      aria-label={label}
      menuTrigger="focus"
      isDisabled={isDisabled}
      selectedKey={selectedKey || null}
      onSelectionChange={(k) => onSelect(k == null ? "" : String(k))}
    >
      <Label className="mb-1 block text-sm text-muted">{label}</Label>
      <div className="relative flex items-center">
        <Input className={inputCls} placeholder={placeholder} />
        <Button className="absolute right-2 cursor-pointer border-0 bg-transparent text-muted text-[10px]">▾</Button>
      </div>
      <Popover offset={4} className="rounded-lg border border-border bg-card p-1 shadow-xl">
        <ListBox className="outline-none max-h-[260px] min-w-[var(--trigger-width)] max-w-[480px] overflow-auto">
          {items.map((it) => (
            <ListBoxItem key={it.id} id={it.id} textValue={it.name}
              className="cursor-pointer rounded-md px-2.5 py-1.5 text-sm text-fg outline-none
                         data-[hovered]:bg-subtle data-[focused]:bg-subtle data-[selected]:bg-subtle">
              {it.name}
            </ListBoxItem>
          ))}
        </ListBox>
      </Popover>
    </ComboBox>
  );
}

export function OpenAISettings({ onClose }: { onClose: () => void }) {
  const init = getCreds();
  const [providerId, setProviderId] = useState(init.baseUrl ? providerForBaseUrl(init.baseUrl).id : "");
  const [baseUrl, setBaseUrl] = useState(init.baseUrl);
  const [apiKey, setApiKey] = useState(init.apiKey);
  const [model, setModel] = useState(init.model);
  const [models, setModels] = useState<string[]>(init.model ? [init.model] : []);
  const [loading, setLoading] = useState(false);
  const [error, setError] = useState("");

  const isCustom = providerId === "custom";
  const onProvider = (id: string) => {
    setProviderId(id);
    const p = PROVIDERS.find((x) => x.id === id);
    if (p && p.id !== "custom") setBaseUrl(p.baseUrl);
    setModels([]); setModel(""); setError("");
  };

  const loadModels = async () => {
    setLoading(true); setError("");
    try {
      const list = await listModels(baseUrl.trim(), apiKey.trim());
      setModels(list);
      if (list.length === 0) setError("Provider returned no models.");
    } catch {
      setError("Couldn't list models — check the endpoint and API key.");
    } finally {
      setLoading(false);
    }
  };

  const save = () => {
    setCreds({ baseUrl, apiKey, model });
    onClose();
  };
  const canSave = !!(baseUrl.trim() && apiKey.trim() && model.trim());

  return (
    <Modal onClose={onClose} label="Scaffold model settings" wide="max-w-[520px]">
      <h2>Scaffold model settings</h2>
      <p className="text-muted">
        Scaffolding runs against your own OpenAI-compatible endpoint. Pick a provider,
        enter your API key, then load and pick a model.
      </p>
      <div className="flex flex-col gap-3">
        <SearchSelect label="Provider" items={PROVIDERS} selectedKey={providerId}
          onSelect={onProvider} placeholder="search providers…" />
        {isCustom && (
          <TextField aria-label="Base URL" value={baseUrl} onChange={setBaseUrl}>
            <Label className="mb-1 block text-sm text-muted">Base URL</Label>
            <Input className={inputCls} placeholder="https://host/v1" />
          </TextField>
        )}
        <TextField aria-label="API key" value={apiKey} onChange={setApiKey}>
          <Label className="mb-1 block text-sm text-muted">API key</Label>
          <Input type="password" className={inputCls} placeholder="sk-…" />
        </TextField>
        <div className="flex items-end gap-2">
          <div className="flex-1">
            <SearchSelect label="Model" items={models.map((m) => ({ id: m, name: m }))}
              selectedKey={model} onSelect={setModel}
              placeholder={models.length ? "search models…" : "load models first"}
              isDisabled={models.length === 0} />
          </div>
          <Button className="btn mb-px whitespace-nowrap" isDisabled={!baseUrl.trim() || !apiKey.trim() || loading}
            onPress={loadModels}>{loading ? "loading…" : "Load models"}</Button>
        </div>
        {error && <p className="text-[crimson] text-sm">{error}</p>}
        <Button className="btn mt-1 self-start" isDisabled={!canSave} onPress={save}>Save</Button>
      </div>
    </Modal>
  );
}
