# Multi-provider architecture

How Open Grok treats **xAI**, **OpenAI Codex**, and **Kimi** (Platform vs Code) without leaking credentials, tools, or opaque history.

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
6. **xAI-only services** (relay, some uploads, etc.) close via monotonic export boundary after non-xAI denied profiles. Compatibility field name remains `ever_used_codex` even when the triggering provider is not Codex; subagents mark the parent tree.
7. **xAI media / Imagine** must not receive Codex bearer; hide media tools while Codex is active.
8. **Hosted search** is dialect-scoped: xAI web/X search vs OpenAI `web_search` — no silent cross-provider fallback.
9. **Opaque history** (e.g. Codex compaction carriers, xAI-only items) is projected only by the matching dialect.

## Sampling, routing, compaction

### Routing

- Catalog entry sets `provider` + `api_backend`.
- Shell builds `SamplerConfig` from chat state + credentials; `/model` rebuilds harness in place.
- Auxiliary models (recap, memory, titles): inherit active provider unless user explicitly picks cross-provider.

### Adapter differences (summary)

| Behavior | xAI | Codex | Kimi |
| --- | --- | --- | --- |
| Private headers | `x-grok-*` | stripped | stripped |
| Doom-loop opt-in | yes | no | no |
| Responses extras | minimal | Max/Ultra mapping, multi-agent mode, reasoning summary | N/A |
| Prompt cache key | no | session id | no |
| Sticky turn state | no | `x-codex-turn-state` | no |
| Unknown `response.*` events | strict | ignore unknown side-channels when opted | N/A |
| Chat sanitization | — | — | clears temp/top_p/penalties |

### Compaction

| | xAI | Codex |
| --- | --- | --- |
| Default | Grok Build local/summary compaction | Remote Compaction V2 over streaming `/responses` |
| Legacy | — | unary `/responses/compact` if feature flag off |
| Cross-provider switch | — | compacted Codex → xAI uses **plaintext fallback only**; never replay opaque Codex items |

## Code Mode and tools

When Code Mode is effective (a Codex Code Mode Only requirement beats Settings):

1. Responses exposes provider-compatible `exec` plus schema `wait`; Code Mode Only also retains direct-only exceptions.
2. Codex uses native custom/freeform raw JavaScript. xAI uses a function envelope with that JavaScript in the required `source` string; native custom items are projected or rejected before xAI network I/O.
3. Mixed mode retains ordinary top-level tools. Only mode keeps them registered for `tools.*` only.
4. Persistent V8 for a compatible timeline; reset on rewind/provider boundaries and disposed on session end.
5. UI hides transport; shows nested tools.
6. Requires Responses-backed models.

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
6. Filter hosted + local tools by provider; never reuse another provider’s credentials for media/search.
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

## See also

- [architecture.md](architecture.md)
- [agent-runtime.md](agent-runtime.md)
- [editing.md](editing.md)
- [development.md](development.md)
