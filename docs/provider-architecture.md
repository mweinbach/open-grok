# Provider and wire-format architecture

Open Grok keeps model identity, wire format, and credentials as separate
decisions. This is the extension contract for adding a provider or API shape
without accidentally inheriting xAI or Codex behavior.

## The three independent axes

1. `ApiBackend` selects the HTTP protocol: Chat Completions, Responses, or
   Messages. It owns endpoint selection and the protocol-level conversion to
   and from the shared `ConversationRequest` / `ConversationResponse` model.
2. `ProviderProfile` selects provider policy. It declares supported backends,
   an optional Responses wire dialect, an optional hosted-tool schema, native
   web-search capability, private request-metadata policy, built-in session
   credential source, and whether xAI-only services may receive data from the
   provider.
3. `AuthScheme` and `BearerResolver` select request authentication. A model may
   use an explicit API key even when its provider also supports OAuth. A live
   OAuth resolver supplies the bearer and account-scoped headers atomically.

Selecting the Responses backend does not select Codex OAuth, and selecting the
Codex provider does not override an explicit model API key.

## Current built-in mapping

| Provider | Backends | Responses dialect | Hosted tools | Native web search | Private metadata | Session credential | xAI-only exports |
| --- | --- | --- | --- | --- | --- | --- | --- |
| xAI | Chat, Responses, Messages | xAI | xAI | yes | `x-grok-*` | xAI session | allowed |
| OpenAI Codex | Responses | Codex | OpenAI | yes | standard only | Codex OAuth | denied |
| Kimi | Chat | none | client function tools | no | standard only | provider API key | denied |
| Fireworks AI | Chat | none | client function tools | no | standard only | provider API key | denied |
| DeepSeek direct | Chat, Responses (V4 Flash) | DeepSeek | OpenAI | yes (V4 Flash) | standard only | provider API key | denied |
| Meta API | Responses | Meta | OpenAI | yes | standard only | provider API key | denied |
| Wafer AI | Chat | none | client function tools | no | standard only | provider API key | denied |
| Z AI | Chat | none | client function tools | no | standard only | provider API key | denied |
| RunInfra | Chat | none | client function tools | no | standard only | provider API key | denied |
| Google Gemini | Chat | none | client function tools | no | standard only | provider API key | denied |
| OpenCode Go | Chat, Messages | none | client function tools | no | standard only | provider API key | denied |
| OpenRouter | Chat | none | client function tools | no | standard only | provider API key | denied |

The sampler's built-in `ProviderAdapter` registry applies the transport policy
for each profile. The xAI adapter owns xAI request metadata and doom-loop
opt-in. The Codex adapter owns instruction projection, reasoning-summary and
Max/Ultra request mapping, prompt-cache affinity, sticky turn state, and
forward-compatible Responses event handling. Neither adapter resolves or
refreshes credentials. The Kimi adapter uses ordinary Chat Completions and
removes sampling fields owned by Kimi coding models; it does not advertise a
hosted-tool dialect. The Fireworks AI adapter is a plain Chat Completions
transport: standard sampling fields pass through unchanged and no hosted-tool
dialect is advertised. Curated Fireworks reasoning models expose the provider's
common `low`/`medium`/`high` Chat Completions effort controls and the
`priority` service tier used by `/fast`, including fast router variants;
models without an explicit capability remain fail-closed.
Fireworks exposes a curated model list; its `/models`
endpoint may enrich curated entries (context window) but can neither add nor
remove models. The DeepSeek adapter keeps V4 Pro on Chat Completions and routes
`deepseek-v4-flash` through DeepSeek's stateless Responses dialect. It
normalizes Open Grok's reasoning menu to `none`/`low`/`high`/`max`, exposes the
OpenAI-shaped hosted `web_search`, and never inherits Codex cache, turn-state,
OAuth, or compaction behavior. Its live catalog intersects DeepSeek's `/models`
response with curated direct entries.
The Meta adapter routes Muse Spark through Meta's stateless OpenAI-compatible
Responses endpoint. It preserves the provider's `low`/`medium`/`high`/`xhigh`
reasoning efforts, declares OpenAI-shaped hosted `web_search`, strips unsupported
OpenAI storage and prompt-cache fields, and intersects Meta's live `/models`
response with the three curated Muse Spark entries.
OpenCode Go selects Chat Completions or Messages per model from canonical
metadata rather than from provider identity alone.
OpenRouter is a plain OpenAI-compatible Chat Completions gateway at
`https://openrouter.ai/api/v1`: it uses provider-local API-key auth, discovers
models from `GET /models?output_modalities=all`, and exposes every
discovered text model in the picker (an optional enabled list can narrow
it). Per-model reasoning menus use only `reasoning.supported_efforts`
from that payload (`null` = all gateway efforts; omitted = no selector).
Inference maps `reasoning_effort` onto OpenRouter's nested
`reasoning` object, accepts `delta.reasoning` thinking tokens, and never
inherits xAI request metadata, credentials, or exports. It has no native hosted
web-search capability.
Wafer AI is a plain OpenAI-compatible Chat Completions provider at
`https://pass.wafer.ai/v1`: it uses provider-local API-key auth, discovers
models from `GET /v1/models`, and exposes only standard client function tools.
It has no native hosted web-search capability and never inherits xAI request
metadata, credentials, or exports.

Z AI is a plain OpenAI-compatible Chat Completions provider at the GLM Coding
Plan endpoint: it uses provider-local API-key auth, discovers models from
`GET /models` with a curated fallback, and exposes only standard client
function tools.

RunInfra is a plain OpenAI-compatible Chat Completions provider at
`https://api.runinfra.ai/v1`: it uses provider-local API-key auth
(`RUNINFRA_GATEWAY_KEY` / `RUNINFRA_API_KEY`), discovers models from
`GET /v1/models` with a curated hosted fallback, and exposes only standard
client function tools. It has no Responses dialect, hosted tools, native web
search, or xAI export path. Stored keys stay on `https://api.runinfra.ai`.

Google Gemini (AI Studio) is a plain OpenAI-compatible Chat Completions
provider at `https://generativelanguage.googleapis.com/v1beta/openai/`: it uses
provider-local API-key auth (`GEMINI_API_KEY` / `GOOGLE_API_KEY`), enriches the
curated four-model catalog from live `/models`, and exposes only standard
client function tools. It has no Responses dialect, hosted tools, native web
search, or xAI export path. Stored keys stay on
`https://generativelanguage.googleapis.com`.

`ConversationRequest` and `ConversationResponse` remain provider neutral.
Provider-native opaque history is retained with a typed backend item and is
projected only by the matching Responses dialect, so xAI X Search history and
Codex compaction history cannot cross providers on the wire.

## Adding a built-in provider

1. Add the stable provider identity and a complete `ProviderProfile`. Reuse an
   existing dialect or hosted-tool schema when the wire contract is actually
   compatible; do not copy credential behavior merely because endpoints look
   alike.
2. Add exactly one sampler adapter and registry entry. Keep request patches,
   response normalization, unknown-event policy, private headers, cache keys,
   and turn-state behavior behind that adapter.
3. Map each catalog model to an `ApiBackend`. Add a new backend only when the
   request/stream protocol differs; a provider-specific Responses variant
   belongs in the adapter or dialect instead.
4. For API-key models, select the explicit `ApiKeyOnly` session policy,
   configure `AuthScheme`, and leave the live resolver empty. This prevents a
   model without credentials from inheriting the global xAI key. For OAuth,
   implement a provider-owned credential store and
   `BearerResolver`; keep refresh/logout/account headers isolated from every
   other provider.
5. Preserve the monotonic export boundary. Its serialized compatibility field
   is still named `ever_used_codex`, but its runtime and persistence semantics
   apply to every profile that denies xAI services. A schema rename may happen
   later without changing that safety contract.
6. Add table-driven registry coverage plus request, stream, tool, structured
   output, credential-isolation, retry, and export-boundary tests.

Custom endpoints can already reuse an existing provider profile and select any
backend supported by that profile with an explicit API key. A genuinely
different provider contract is a compile-time registration so missing security
and wire policies fail closed rather than silently inheriting xAI defaults.
Remote catalog entries with an explicit unknown provider or backend are
rejected; provider omission remains the legacy xAI default for old catalogs.

## Load-bearing invariants

- Provider identity comes from model metadata, never a model slug or URL.
- API backend selection never grants credentials or provider-private headers.
- Explicit model API keys remain authoritative over built-in OAuth.
- OAuth bearer and account-scoped headers come from one credential snapshot.
- Built-in session credentials are sent only to that provider's trusted
  inference endpoint. Codex development proxies require the explicit
  `GROK_CODEX_INFERENCE_BASE_URL` process-level trust override.
- A provider that denies xAI services closes the session export boundary
  monotonically.
- Hosted tools and opaque response history are serialized only for their
  declared dialect.
- Native and fallback web-search declarations are gated from
  `ProviderProfile`, never model slugs, endpoints, or URL inspection. The
  opt-in Perplexity fallback is therefore Kimi-only; xAI and Codex retain their
  native declarations.
- Unknown future events may be ignored only when the selected adapter opts in;
  malformed known events still fail loudly.
