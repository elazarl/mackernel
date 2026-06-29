// Known OpenAI-compatible providers the scaffold settings can pick from. Selecting one
// fills the base URL; the API key + model come from the user. The host of each baseUrl
// must also be in the scaffold container's egress allowlist (docs/opencode-egress.md).
// The "custom" entry lets the user type any base URL (its host must be allowlisted too).
export interface Provider {
  id: string;
  name: string;
  baseUrl: string; // "" for the custom sentinel
}

export const PROVIDERS: Provider[] = [
  { id: "openai", name: "OpenAI", baseUrl: "https://api.openai.com/v1" },
  { id: "crusoe-prod", name: "Crusoe Managed Inference (prod)", baseUrl: "https://api.inference.crusoecloud.com/v1" },
  { id: "crusoe-dev", name: "Crusoe Managed Inference (dev)", baseUrl: "https://api.inference.crusoecloud.xyz/v1" },
  { id: "crusoe-openrouter", name: "Crusoe OpenRouter gateway", baseUrl: "https://openrouter.inference.crusoecloud.com/api/v1" },
  { id: "openrouter", name: "OpenRouter", baseUrl: "https://openrouter.ai/api/v1" },
  { id: "groq", name: "Groq", baseUrl: "https://api.groq.com/openai/v1" },
  { id: "fireworks", name: "Fireworks", baseUrl: "https://api.fireworks.ai/inference/v1" },
  { id: "together", name: "Together", baseUrl: "https://api.together.xyz/v1" },
  { id: "deepinfra", name: "DeepInfra", baseUrl: "https://api.deepinfra.com/v1/openai" },
  { id: "hyperbolic", name: "Hyperbolic", baseUrl: "https://api.hyperbolic.xyz/v1" },
  { id: "custom", name: "Custom (enter base URL)", baseUrl: "" },
];

// The provider whose baseUrl matches, else "custom" (so a saved custom URL stays custom).
export const providerForBaseUrl = (baseUrl: string): Provider =>
  PROVIDERS.find((p) => p.baseUrl && p.baseUrl === baseUrl) ??
  PROVIDERS.find((p) => p.id === "custom")!;
