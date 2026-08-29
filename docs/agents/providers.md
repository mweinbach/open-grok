# Multi-provider architecture

How Open Grok treats **xAI**, **OpenAI Codex**, **Kimi** (Platform vs Code), **Fireworks AI**, **DeepSeek API direct**, **Meta API**, **Wafer AI**, **Z AI**, **RunInfra**, **Google Gemini**, **OpenCode Go**, and **OpenRouter** without leaking credentials, tools, or opaque history.

**Canonical contracts:**

- [`../provider-architecture.md`](../provider-architecture.md) — extension axes and invariants
- [`../codex-provider-port.md`](../codex-provider-port.md) — Codex parity details
- [`../code-mode-port.md`](../code-mode-port.md) — Code Mode parity details

This page is the agent-oriented map: where code lives, how to change it safely, and what not to break.

## Three independent axes

| Axis | Owns | Does **not** own |
| --- | --- | --- |
| **`ApiBackend`** | HTTP protocol: Chat Completions / Responses / Messages | Credentials, private headers |
| **`ProviderProfile`** | Supported backends, Responses dialect, hosted-tool schema, metadata policy, session-auth kind, xAI export policy | Live token refresh |
| **`AuthScheme` + `BearerResolver`** | Request authentication (API key vs OAuth, atomic bearer + account headers) | Wire dialect / hosted tools |

**Invariants:**

- Selecting Responses ≠ selecting Codex OAuth.
- Selecting Codex ≠ overriding an explicit model API key.
- Provider identity comes from **model metadata**, never slug or URL alone.

Types: `xai-grok-sampling-types` (`ApiBackend`, `ModelProvider`, `ProviderProfile`, `ToolMode`, …).  
Adapters (credential-free): `xai-grok-sampler/src/provider.rs`.

## Built-in mapping

| Provider | Backends | Dialect | Hosted tools | Session auth | xAI-only services |
| --- | --- | --- | --- | --- | --- |
| xAI | Chat, Responses, Messages | xAI | xAI | xAI session | allowed |
| OpenAI Codex | Responses | Codex | OpenAI | Codex OAuth | **denied** |
| Kimi | Chat | none | client function tools | API key only | **denied** |
| Fireworks AI | Chat | none | client function tools | API key only | **denied** |
| DeepSeek direct | Chat, Responses (V4 Flash) | DeepSeek | OpenAI hosted search + client functions | API key only | **denied** |
| Meta API | Responses | Meta | OpenAI hosted search + client functions | API key only | **denied** |
| Wafer AI | Chat | none | client function tools | API key only | **denied** |
| Z AI | Chat | none | client function tools | API key only | **denied** |
| RunInfra | Chat | none | client function tools | API key only | **denied** |
| Google Gemini | Chat | none | client function tools | API key only | **denied** |
| OpenCode Go | Chat, Messages (per model) | none | client function tools | API key only | **denied** |
| OpenRouter | Chat | none | client function tools | API key only | **denied** |

### Wafer AI ([wafer.ai](https://www.wafer.ai/))

Wafer AI is an isolated, API-key-only OpenAI-compatible provider. Its base URL
is `https://pass.wafer.ai/v1`; use Chat Completions for inference and
`GET /v1/models` for dynamic model discovery. Wafer accepts standard client
function tools, does not provide native hosted web search, and must not receive
xAI credentials, private metadata, or xAI-only exports. Live `/models` objects
are ids only, so Open Grok assigns published windows by normalized id:
`kimi-k3` / `kimi-k3-fast`, `deepseek-v4-flash`, and `glm-5.2` / `glm-5.2-flash`
→ 1_000_000; `kimi-k2.6` → 262_144; `glm-5.1` → 203_000; unknown ids stay at
the 200_000 fallback. User `[model.*]` `context_window` overrides still win.

### Z AI ([z.ai](https://z.ai/))

Z AI is an isolated, API-key-only OpenAI-compatible provider offering GLM
models. Its default base URL is the GLM Coding Plan endpoint
`https://api.z.ai/api/coding/paas/v4` (overridable via
`OPENGROK_ZAI_API_BASE_URL`); use Chat Completions for inference and
`GET /models` for dynamic model discovery, with a curated fallback catalog when
the endpoint is unavailable. Known reasoning-capable models (e.g. GLM-4.6,
GLM-5.x) expose an authoritative `reasoning_effort` menu of low/medium/high/max
(high default); the `ZaiProvider` adapter turns any requested effort into the
explicit `thinking` object (`{"type":"enabled","clear_thinking":false}`) Z AI
requires alongside it. Z AI accepts standard client function tools,
does not provide native hosted web search, and must not receive xAI credentials,
private metadata, or xAI-only exports.

Live `/models` objects are `{id, object, created, owned_by}` only — there is
no `context_window`, `context_length`, or `max_model_len`. Open Grok assigns
curated windows by case-insensitive prefix (most-specific first): `glm-5.3`
and `glm-5.2` → 1_000_000 (max completion 131072); `glm-4-32b` or any id containing `128k`
→ 128_000; other `glm-5`, `glm-4.7`, `glm-4.6`, `glm-4.5`, and `glm-4` ids
→ 200_000; unknown ids → 200_000. `glm-5`, `glm-5.1`, `glm-5.2`, and
`glm-5.3` (including Fireworks `glm-5p2` slugs) are text-only: they do not
accept image input, and a `read_file` / `view_image` of an image returns a
model-facing error. Vision variants such as `glm-5v` stay multimodal. A positive wire context field, if the
endpoint ever sends one, wins over the curated value. After a live Z AI (or
Wafer) catalog replace, user `[model.*]` / `cfg.config_models` are re-applied
so custom entries and field overrides survive.

### RunInfra ([runinfra.ai](https://runinfra.ai/))

RunInfra is an isolated, API-key-only OpenAI-compatible provider. Its base URL
is `https://api.runinfra.ai/v1` (overridable via
`OPENGROK_RUNINFRA_API_BASE_URL`); use Chat Completions only — `/v1/responses`
is a compatibility adapter, not a real Responses dialect. Auth is Bearer
(`RUNINFRA_GATEWAY_KEY`, alias `RUNINFRA_API_KEY`; keys start with `rp_`).
Stored keys are sent only to `https://api.runinfra.ai`. Live `GET /v1/models`
is authoritative when present (including `context_window` / `max_output_tokens`
when > 0); a curated hosted fallback keeps the picker populated when the
endpoint is empty or unreachable. Known hosted models reason by default:
`deepseek-v4-flash` defaults to Max and rewrites High/Xhigh/Max/Ultra to
`max`; `qwen3-8-2-4t-a95b` cannot turn thinking off; other known hosted ids
expose none/low/medium/high/max (High default). Unknown live deployments stay
fail-closed with no reasoning menu. RunInfra accepts standard client function
tools, does not provide native hosted web search, and must not receive xAI
credentials, private metadata, or xAI-only exports.

### Google Gemini (AI Studio)

Google Gemini is an isolated, API-key-only OpenAI-compatible Chat Completions
provider. Its base URL is
`https://generativelanguage.googleapis.com/v1beta/openai/` (overridable via
`OPENGROK_GEMINI_API_BASE_URL`). Auth is Bearer (`GEMINI_API_KEY`, alias
`GOOGLE_API_KEY`). Stored keys are sent only to
`https://generativelanguage.googleapis.com`. Live `/models` enrich-only updates
the curated four models (`gemini-3.7-flash`, `gemini-3.6-flash`,
`gemini-3.5-flash-lite`, `gemini-3.1-pro-preview`); catalog keys use
`gemini:{id}`. Gemini 3 cannot use reasoning effort `none`. `gemini-3.7-flash`
and `gemini-3.1-pro-preview` reject `minimal` (menu is low/medium/high);
`gemini-3.6-flash` and `gemini-3.5-flash-lite` offer minimal/low/medium/high.
Defaults: 3.7-flash Medium, 3.6-flash Medium, 3.5-flash-lite Minimal,
3.1-pro-preview High. Gemini accepts standard client function tools, has no
Responses dialect, hosted tools, or native search, and must not receive xAI
credentials or xAI-only exports.

### OpenRouter ([openrouter.ai](https://openrouter.ai/))

OpenRouter is an isolated, API-key-only OpenAI-compatible Chat Completions
gateway. Its base URL is `https://openrouter.ai/api/v1` (overridable via
`OPENGROK_OPENROUTER_API_BASE_URL`). Auth is Bearer (`OPENROUTER_API_KEY`).
Stored keys are sent only to `https://openrouter.ai`. Open Grok queries
`GET /models` and shows a checklist of discovered text/tool-capable models;
none are enabled by default. Only selected models appear in normal model
settings and are eligible for subagents. Requests include the optional
`HTTP-Referer` and `X-Title` attribution headers. Models that advertise
`reasoning` expose none/low/medium/high/xhigh (Medium default). OpenRouter
accepts standard client function tools, has no Responses dialect, hosted
tools, or native search, and must not receive xAI credentials or xAI-only
exports.

### Custom endpoint (user-supplied server address)

`custom` (`ModelProvider::Custom`, profile `CUSTOM`) is not a service but the
profile for an address the user typed. There is no stored credential scope and no
curated catalog: each wizard-saved `[model.<key>]` row owns its `base_url`,
`api_backend`, `auth_scheme`, context window, and credential
(`api_key`/`env_key`). Provider identity still comes from model metadata; the
host name is never consulted to pick a protocol or a credential.

Backends: Chat Completions, Responses, and Messages. The Responses variant is the
`OpenAi` dialect (vanilla, stateless, `store: false`), which strips
`previous_response_id`, cache-key affinity, service tier, `x-grok-*` fields, and
any background/stream-options state, and replays no opaque history. Code Mode,
hosted tools, native web search, and xAI services are all off.

Discovery is `GET {base}/models`: `Authorization: Bearer` for the OpenAI formats,
`x-api-key` + `anthropic-version` for Messages, no redirect following, 20 s
timeout, key-redacted error excerpts. `open-grok/custom-providers/discover` only
reads; `open-grok/custom-models/upsert-many` is what persists the user's
selection as one atomic config write.

## Layer map (paths)

```text
Identity & policy
  xai-grok-sampling-types/src/types.rs
  xai-grok-sampling-types/src/conversation.rs   # provider-neutral Conversation*

Transport adapters (no auth)
  xai-grok-sampler/src/provider.rs              # Xai / Codex / Kimi + PROVIDER_REGISTRY
  xai-grok-sampler/src/client.rs
  xai-grok-sampler/src/stream/{chat_completions,responses,messages}.rs

xAI auth
  xai-grok-shell/src/auth/                      # AuthManager, OIDC, storage
  xai-grok-shell/src/auth/storage.rs            # auth.json scopes (incl. Kimi)

Codex auth (isolated)
  xai-grok-shell/src/codex_auth.rs              # codex-auth.json, OAuth, BearerResolver
  xai-grok-shell/src/codex_models.rs            # live catalog + cache

Kimi
  xai-grok-shell/src/kimi_models.rs             # endpoints, discovery, trusted hosts
  auth/storage.rs                               # kimi::api_key vs kimi_code::api_key

Fireworks AI
  xai-grok-shell/src/fireworks_models.rs        # curated catalog, enrichment query, trusted host
  auth/storage.rs                               # fireworks::api_key (generic provider scope)

DeepSeek direct
  xai-grok-shell/src/deepseek_models.rs         # curated direct catalog, live availability, trusted host
  auth/storage.rs                               # deepseek::api_key (generic provider scope)

Meta API
  xai-grok-shell/src/meta_models.rs             # curated Muse Spark catalog, live availability, trusted host
  auth/storage.rs                               # meta::api_key (generic provider scope)

Wafer AI
  xai-grok-shell/src/wafer_models.rs            # dynamic /models catalog, trusted host
  auth/storage.rs                               # wafer::api_key (generic provider scope)

Z AI
  xai-grok-shell/src/zai_models.rs              # dynamic /models catalog + curated fallback, trusted host
  auth/storage.rs                               # zai::api_key (generic provider scope)

RunInfra
  xai-grok-shell/src/runinfra_models.rs         # live /models catalog + curated hosted fallback, trusted host
  auth/storage.rs                               # runinfra::api_key (generic provider scope)

Google Gemini
  xai-grok-shell/src/gemini_models.rs           # curated four models + live /models enrich-only, trusted host
  auth/storage.rs                               # gemini::api_key (generic provider scope)

OpenCode Go
  xai-grok-shell/src/opencode_go_models.rs      # live availability + models.dev protocol mapping
  auth/storage.rs                               # opencode_go::api_key (generic provider scope)

OpenRouter
  xai-grok-shell/src/openrouter_models.rs       # live /models catalog + opt-in enable list
  auth/storage.rs                               # openrouter::api_key (generic provider scope)

Custom endpoint (no credential store, no curated catalog)
  xai-grok-shell/src/custom_providers.rs        # address rules, wire formats, GET /models
  xai-grok-shell/src/custom_models.rs           # [model.<key>] rows incl. auth_scheme
  xai-grok-shell/src/util/config/persist.rs     # single/batch [model.*] table writes

Session routing / tools / compaction
  xai-grok-shell/src/session/
  xai-grok-shell/src/session/compaction.rs
  xai-grok-shell/src/session/code_mode.rs
  xai-grok-shell/src/agent/handlers/model_switch.rs
  crates/common/xai-grok-compaction/

UI / login
  xai-grok-pager/src/settings/
  xai-grok-pager slash login/logout/model/usage
```

## Credential stores (never cross)

Home root: `$OPENGROK_HOME` or `~/.opengrok` via `xai_grok_config::grok_home()`.

| Store | Path | Commands |
| --- | --- | --- |
| xAI primary | `$OPENGROK_HOME/auth.json` | `open-grok login` / `logout` |
| Codex OAuth | `$OPENGROK_HOME/codex-auth.json` | `login --codex` / `logout --codex` |
| Kimi Platform | `auth.json` scope `kimi::api_key` | Settings / `/login kimi` |
| Kimi Code | `auth.json` scope `kimi_code::api_key` | Settings / `/login kimi` |
| Fireworks AI | `auth.json` scope `fireworks::api_key` | Settings / `/login fireworks` |
| DeepSeek direct | `auth.json` scope `deepseek::api_key` | Settings / `/login deepseek` |
| Meta API | `auth.json` scope `meta::api_key` or `META_API_KEY` | Settings / `/login meta` / environment |
| Wafer AI | `auth.json` scope `wafer::api_key` | Settings / `/login wafer` |
| Z AI | `auth.json` scope `zai::api_key` or `ZAI_API_KEY` | Settings / `/login zai` / environment |
| RunInfra | `auth.json` scope `runinfra::api_key` or `RUNINFRA_GATEWAY_KEY` / `RUNINFRA_API_KEY` | Settings / `/login runinfra` / environment |
| Google Gemini | `auth.json` scope `gemini::api_key` or `GEMINI_API_KEY` / `GOOGLE_API_KEY` | Settings / `/login gemini` / environment |
| OpenCode Go | `auth.json` scope `opencode_go::api_key` | Settings / `/login opencode-go` |
| OpenRouter | `auth.json` scope `openrouter::api_key` or `OPENROUTER_API_KEY` | Settings / `/login openrouter` / environment |
| Perplexity Search fallback | `auth.json` scope `perplexity::api_key` | Settings |
| Both providers | — | `logout --all` |

Also isolated:

- Codex model cache: `$OPENGROK_HOME/codex_models_cache.json` (not xAI `models_cache.json`)
- Codex inference trust override: `GROK_CODEX_INFERENCE_BASE_URL` (process-level)

### Isolation rules

1. **Codex never uses xAI `AuthManager` / primary ACP auth cell for its tokens.**
2. **Explicit model API keys win over OAuth** for that model.
3. **Bearer + account headers are one snapshot** — account drift mid-session fails closed.
4. **401 refresh paths are provider-local** — never refresh or mutate the other store.
5. **Kimi Platform vs Code** keys, catalogs, and trusted hosts are non-interchangeable.
   Platform embeds/discovers `kimi-k3` on Moonshot; Code embeds/discovers `k3`,
   `k3-256k`, and the `kimi-for-coding*` family on `api.kimi.com/coding/v1`.
6. **DeepSeek direct credentials are host-scoped.** UI-stored keys are sent only to the official trusted API host; `DEEPSEEK_API_KEY` may accompany an explicit process-level base-URL override. `deepseek-v4-flash` uses DeepSeek's stateless Responses API; V4 Pro remains on Chat Completions until the provider exposes Responses support for it.
7. **Meta API credentials are host-scoped.** `META_API_KEY` is sent to `https://api.meta.ai/v1`; an explicit process-level `OPENGROK_META_API_BASE_URL` override may redirect the environment key, while stored provider credentials remain restricted to the official host. Meta uses stateless Responses, native hosted web search, and only the curated Muse Spark catalog.
8. **OpenCode Go is opt-in per model.** Its live `/models` IDs are intersected with canonical metadata; unsupported or unclassified models are omitted. The enabled list defaults empty, and only enabled entries reach normal model settings or subagent selection.
9. **OpenCode Go transport is model-owned.** `@ai-sdk/anthropic` entries use Messages + `x-api-key`; OpenAI-compatible entries use Chat Completions + Bearer. Never choose the protocol from the provider alone.
10. **Wafer AI is API-key-only and provider-local.** `WAFER_API_KEY` is sent only to `https://pass.wafer.ai/v1`; its dynamic `/models` catalog, client function tools, and standard metadata do not inherit xAI behavior. Wafer has no native hosted web search.
11. **RunInfra is API-key-only and provider-local.** `RUNINFRA_GATEWAY_KEY` / `RUNINFRA_API_KEY` are sent only to `https://api.runinfra.ai`; its live `/models` catalog, client function tools, and standard metadata do not inherit xAI behavior. RunInfra has no native hosted web search and no Responses dialect.
12. **Google Gemini is API-key-only and provider-local.** `GEMINI_API_KEY` / `GOOGLE_API_KEY` are sent only to `https://generativelanguage.googleapis.com`; its curated catalog (live enrich-only), client function tools, and standard metadata do not inherit xAI behavior. Gemini has no native hosted web search and no Responses dialect.
13. **xAI-only services** (relay, some uploads, etc.) close via monotonic export boundary after non-xAI denied profiles. Compatibility field name remains `ever_used_codex` even when the triggering provider is not Codex; subagents mark the parent tree.
14. **Image generation is explicitly routed.** The default `grok` route uses
    xAI Imagine credentials and remains hidden outside eligible xAI sessions.
    Selecting `openai` in Settings is an explicit cross-provider opt-in: it
    uses only the isolated Codex OAuth store (ChatGPT login — no OpenAI API
    key path, mirroring upstream's `uses_codex_backend` gate), calls the Codex
    Images endpoint, blocks known ChatGPT Free accounts, and never reuses an
    xAI credential. Video generation remains xAI-only.
15. **Hosted search** is dialect-scoped: xAI web/X search vs OpenAI `web_search`. Optional client search fallbacks are declared only when the provider profile permits them. Never infer this from model names or URLs.
16. **Opaque history** (e.g. Codex compaction carriers, xAI-only items) is projected only by the matching dialect.
17. **Standalone search is route-scoped.** Official Codex OAuth Responses
    routes default to the provider-local `/alpha/search` endpoint. API-key and
    custom Responses routes must opt in with
    `supports_standalone_web_search = true`; when unavailable, hosted search
    remains declared.
18. **OpenRouter is opt-in per model.** Live `/models` is authoritative for
    availability and limits. Image/embedding-only and non-tool models are
    omitted. The enabled list defaults empty, and only enabled entries reach
    normal model settings or subagent selection. Stored keys are sent only to
    `https://openrouter.ai`.

## Sampling, routing, compaction

### Routing

- Catalog entry sets `provider` + `api_backend`.
- Shell builds `SamplerConfig` from chat state + credentials; `/model` rebuilds harness in place.
- Auxiliary models (recap, memory, titles): inherit active provider unless user explicitly picks cross-provider.

### Adapter differences (summary)

| Behavior | xAI | Codex | Kimi | Fireworks | DeepSeek | Meta | Wafer | OpenCode Go | OpenRouter |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Private headers | `x-grok-*` | stripped | stripped | stripped | stripped | stripped | stripped | stripped | stripped |
| Doom-loop opt-in | yes | no | no | no | no | no | no | no | no |
| Responses extras | minimal | Max/Ultra mapping, multi-agent mode, reasoning summary | N/A | N/A | stateless fields; effort normalized to none/low/high/max | stateless fields; preserves low/medium/high/xhigh | N/A | N/A | N/A |
| Prompt cache key | no | session id | no | no | no | no | no | no | no |
| Sticky turn state | no | `x-codex-turn-state` | no | no | no | no | no | no | no |
| Unknown `response.*` events | strict | ignore unknown side-channels when opted | N/A | N/A | strict | strict | N/A | N/A | N/A |
| Chat sanitization | — | — | clears temp/top_p/penalties | schema normalization | strips internal message model IDs | N/A | standard | per backend | strips service tier / message model IDs |

### Compaction

| | xAI | Codex |
| --- | --- | --- |
| Default | Grok Build local/summary compaction | Remote Compaction V2 over streaming `/responses` |
| Legacy | — | unary `/responses/compact` if feature flag off |
| Cross-provider switch | — | compacted Codex → xAI uses **plaintext fallback only**; never replay opaque Codex items |

Before a conversation is sent to an auxiliary summarizer, preparation removes
reasoning, tool outputs, and provider-native `BackendToolCall` items. Do not
forward opaque provider tool-call payloads to a summary route, including one
that happens to use the same model family.

## Code Mode and tools

When Code Mode is effective (a Codex Code Mode Only requirement beats Settings):

1. Responses exposes provider-compatible `exec` plus schema `wait`; Code Mode Only also retains direct-only exceptions.
2. Codex uses native custom/freeform raw JavaScript. xAI and DeepSeek use a function envelope with that JavaScript in the required `source` string; native custom items are projected or rejected before those network boundaries.
3. Mixed mode retains ordinary top-level tools. Only mode keeps them registered for `tools.*` only.
4. Persistent V8 for a compatible timeline; reset on rewind/provider boundaries and disposed on session end.
5. UI hides transport; shows nested tools.
6. Requires Responses-backed models.
7. A supported native Codex route exposes `web__run` through `tools.*` and
   suppresses hosted search only after that client tool is registered.

Codex sessions use Codex file tools (`apply_patch`, …) where the toolset selects them; Grok sessions use `search_replace`. Shared multi-agent / plan / goal / scheduler features remain available across providers when the harness supports them.

## How to add or modify a provider safely

Follow [`../provider-architecture.md`](../provider-architecture.md):

1. Add stable provider identity + complete `ProviderProfile` in `xai-grok-sampling-types`.
2. Add **exactly one** sampler adapter + registry entry (no credentials in adapter).
3. Map catalog models → `ApiBackend`. New backend only when HTTP protocol differs.
4. Auth:
   - API-key: `ApiKeyOnly` policy, scoped storage, empty live resolver.
   - OAuth: **separate file** (like `codex-auth.json`), own `BearerResolver`, fail-closed identity, never xAI `AuthManager`.
5. If `xai_services: Denied`, participate in monotonic export boundary (`ever_used_codex` field name frozen for compatibility).
6. Filter hosted + local tools by provider; never reuse another provider’s credentials for media/search. The user-selected OpenAI Images route is the explicit exception for cross-provider image use, but its Codex credential and endpoint remain isolated.
7. Add table-driven registry coverage + request/stream/tool/credential-isolation/retry/export-boundary tests.
8. Custom endpoints may reuse an existing profile + explicit API key; unknown remote catalog providers fail closed.

### Do not

- Infer provider from model id or URL alone.
- Let backend selection attach private headers or OAuth.
- Share refresh/logout between stores.
- Copy Codex wire behavior onto Kimi “because OpenAI-compatible.”
- Silently fall back across providers for search, media, compaction, or auth.
- Put Codex tokens in `auth.json` or xAI tokens in `codex-auth.json`.

## Tests (provider-related)

```sh
cargo test --locked -p xai-grok-shell --lib custom_providers
cargo test --locked -p xai-grok-shell --lib custom_models
cargo test --locked -p xai-grok-sampling-types
cargo test --locked -p xai-grok-sampler --test test_actor
cargo test --locked -p xai-grok-shell --test codex_auth_contract
cargo test --locked -p xai-grok-shell --test auxiliary_provider_routing
cargo test --locked -p xai-grok-code-mode
cargo test --locked -p xai-grok-code-mode-protocol
```

Also: shell `session/acp_session_tests/` (auth isolation, model switch, compaction), `codex_oauth_retry_e2e.rs`.

## Common pitfalls

| Pitfall | Why it breaks |
| --- | --- |
| Mixing providers on one request | Wrong dialect, tools, or credentials |
| Using xAI AuthManager for Codex | Wrong logout/refresh |
| Platform key for Kimi Code (or reverse) | Non-interchangeable hosts/catalogs |
| Code Mode `exec` as JSON function | Incompatible with Sol / contract |
| Fresh JS process per `exec` | Breaks session persistence |
| Showing `exec`/`wait` as normal tool cards | Transport leakage |
| `previous_response_id` on Codex HTTP full-input | Diverges from codex-rs HTTP contract |
| Replaying Codex compaction to xAI | Opaque items / policy violation |
| Forgetting export boundary on subagent | Parent tree reopens xAI export paths |
| Inferring a custom endpoint's protocol or auth header from its host | Proxies and gateways speak any format; only the saved `api_backend` / `auth_scheme` may decide |
| Deriving `auth_scheme` for rows that are not `provider = "custom"` | Silently switches a built-in or first-party model to `x-api-key` |
| Following a redirect during BYO discovery | Sends the user's key to a host they never typed |

## See also

- [architecture.md](architecture.md)
- [agent-runtime.md](agent-runtime.md)
- [editing.md](editing.md)
- [development.md](development.md)
