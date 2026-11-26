// The provider registry: one line per embedded provider. Keys are wire
// identities (the handshake allowlist), never package names. `load` runs
// on the first call that needs the module — everything else stays out
// of memory until asked for.

function standardSettings(provider) {
  const settings = {};
  if (provider.apiKey) settings.apiKey = provider.apiKey;
  if (provider.url) settings.baseURL = provider.url;
  return settings;
}

const registry = [
  {
    key: "anthropic",
    load: () => import("@ai-sdk/anthropic"),
    create: (module, settings) => module.createAnthropic(settings),
    settings: standardSettings,
  },
  {
    key: "cerebras",
    load: () => import("@ai-sdk/cerebras"),
    create: (module, settings) => module.createCerebras(settings),
    settings: standardSettings,
  },
  {
    key: "cohere",
    load: () => import("@ai-sdk/cohere"),
    create: (module, settings) => module.createCohere(settings),
    settings: standardSettings,
  },
  {
    key: "deepinfra",
    load: () => import("@ai-sdk/deepinfra"),
    create: (module, settings) => module.createDeepInfra(settings),
    settings: standardSettings,
  },
  {
    key: "gateway",
    load: () => import("@ai-sdk/gateway"),
    create: (module, settings) => module.createGateway(settings),
    settings: standardSettings,
  },
  {
    key: "google",
    load: () => import("@ai-sdk/google"),
    create: (module, settings) => module.createGoogle(settings),
    settings: standardSettings,
  },
  {
    key: "groq",
    load: () => import("@ai-sdk/groq"),
    create: (module, settings) => module.createGroq(settings),
    settings: standardSettings,
  },
  {
    key: "mistral",
    load: () => import("@ai-sdk/mistral"),
    create: (module, settings) => module.createMistral(settings),
    settings: standardSettings,
  },
  {
    key: "openai",
    load: () => import("@ai-sdk/openai"),
    create: (module, settings) => module.createOpenAI(settings),
    settings: standardSettings,
  },
  {
    key: "openai-compatible",
    load: () => import("@ai-sdk/openai-compatible"),
    create: (module, settings) => module.createOpenAICompatible(settings),
    // baseURL is required by this factory; the resolver refuses a call
    // without a url before we get here.
    settings: (provider) => ({
      name: "openai-compatible",
      baseURL: provider.url,
      ...(provider.apiKey && { apiKey: provider.apiKey }),
    }),
  },
  {
    key: "togetherai",
    load: () => import("@ai-sdk/togetherai"),
    create: (module, settings) => module.createTogetherAI(settings),
    settings: standardSettings,
  },
  {
    key: "xai",
    load: () => import("@ai-sdk/xai"),
    create: (module, settings) => module.createXai(settings),
    settings: standardSettings,
  },
];

export const PROVIDER_KEYS = registry.map((entry) => entry.key);

/// Resolve a call's model. Provider-agnostic: the runner never names a
/// provider — this table is the only place that knows they exist.
export async function resolveModel(call) {
  const entry = registry.find((candidate) => candidate.key === call.provider.key);
  if (!entry) {
    return { error: `unknown provider \`${call.provider.key}\`` };
  }
  if (entry.key === "openai-compatible" && !call.provider.url) {
    return { error: "provider `openai-compatible` requires a url" };
  }
  try {
    const module = await entry.load();
    const provider = entry.create(module, entry.settings(call.provider));
    return { model: provider.languageModel(call.model) };
  } catch (error) {
    return { error: String(error?.message ?? error) };
  }
}
