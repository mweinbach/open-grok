# Provider and wire-format architecture

Open Grok keeps model identity, wire format, and credentials as separate
decisions. This is the extension contract for adding a provider or API shape
without accidentally inheriting xAI or Codex behavior.

## The three independent axes

1. `ApiBackend` selects the HTTP protocol: Chat Completions, Responses, or
   Messages. It owns endpoint selection and the protocol-level conversion to
   and from the shared `ConversationRequest` / `ConversationResponse` model.
2. `ProviderProfile` selects provider policy. It declares supported backends,
   an optional Responses wire dialect, an optional hosted-tool schema, private
   request-metadata policy, built-in session credential source, and whether
   xAI-only services may receive data from the provider.
3. `AuthScheme` and `BearerResolver` select request authentication. A model may
   use an explicit API key even when its provider also supports OAuth. A live
   OAuth resolver supplies the bearer and account-scoped headers atomically.

Selecting the Responses backend does not select Codex OAuth, and selecting the
Codex provider does not override an explicit model API key.

## Current built-in mapping

| Provider | Backends | Responses dialect | Hosted tools | Private metadata | Session credential | xAI-only exports |
| --- | --- | --- | --- | --- | --- | --- |
| xAI | Chat, Responses, Messages | xAI | xAI | `x-grok-*` | xAI session | allowed |
| OpenAI Codex | Responses | Codex | OpenAI | standard only | Codex OAuth | denied |

The sampler's built-in `ProviderAdapter` registry applies the transport policy
for each profile. The xAI adapter owns xAI request metadata and doom-loop
opt-in. The Codex adapter owns instruction projection, reasoning-summary and
Max/Ultra request mapping, prompt-cache affinity, sticky turn state, and
forward-compatible Responses event handling. Neither adapter resolves or
refreshes credentials.

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
- Unknown future events may be ignored only when the selected adapter opts in;
  malformed known events still fail loudly.
