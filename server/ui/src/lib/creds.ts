// The OpenAI-compatible credentials scaffolding runs against, kept in localStorage
// (same pattern as lib/theme.ts and the mk_token gate). We persist the resolved baseUrl
// (not a provider id) so the backend stays provider-agnostic. Scaffolding is gated on
// hasCreds(): the UI won't start a run, and the backend rejects one, without all three.
export interface Creds {
  baseUrl: string;
  apiKey: string;
  model: string;
}

const KEYS = { baseUrl: "mk_openai_base_url", apiKey: "mk_openai_key", model: "mk_openai_model" } as const;

export const getCreds = (): Creds => ({
  baseUrl: localStorage.getItem(KEYS.baseUrl) || "",
  apiKey: localStorage.getItem(KEYS.apiKey) || "",
  model: localStorage.getItem(KEYS.model) || "",
});

export const setCreds = (c: Creds) => {
  localStorage.setItem(KEYS.baseUrl, c.baseUrl.trim());
  localStorage.setItem(KEYS.apiKey, c.apiKey.trim());
  localStorage.setItem(KEYS.model, c.model.trim());
};

export const hasCreds = (): boolean => {
  const c = getCreds();
  return !!(c.baseUrl && c.apiKey && c.model);
};
