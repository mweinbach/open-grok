# Codex provider integration

This document records Open Grok's OpenAI Codex provider compatibility target.
The implementation is based on the Codex Rust workspace at the same pinned
revision as the Code Mode port:

- Repository: <https://github.com/openai/codex>
- Commit: `2be648ba4a6c159a3d80b1c07e7323cbd5efef8f`
- License: Apache-2.0

The live model-catalog, per-turn routing, response-stream, and compaction
compatibility passes were refreshed against Codex commit
`0f44bca9154e056a32fde7a89026b4620599e6f2`.
The client-side image-generation extension was refreshed against Codex `main`
at `6d4d9442c7142c08ac5c5098dfd6e82d8cd9f65a`; this includes the
`x-codex-image-turn-id` correlation added by upstream commit
`6219b7c40fc9`.

The Responses Lite and asynchronous user-message contracts were refreshed
against Codex commit `8e93b1a405e02f8797a04d747bb7d1654b839685`. The default
model-catalog compatibility version is `0.150.0`.

## Live model catalog

Open Grok embeds the current GPT-5.6 Sol, Terra, and Luna definitions so the
picker and headless model resolution still work offline. With ChatGPT Codex
credentials available, the shell follows codex-rs' live catalog contract:

- GET `https://chatgpt.com/backend-api/codex/models?client_version=<version>`
  with a five-second timeout.
- Send the Codex bearer plus `ChatGPT-Account-ID` and `X-OpenAI-Fedramp` when
  present, with one forced token refresh and retry after a 401.
- Read the response `ETag` and cache the parsed catalog for five minutes.
- Use a nonempty list-visible ChatGPT response as authoritative for the Codex
  provider partition. Empty or hidden-only responses merge with the embedded
  fallback, matching codex-rs behavior.
- Project each model's live context window, reasoning menu, reasoning-summary
  support/default, hosted-search flag, tool mode, `multi_agent_version`,
  `auto_compact_token_limit`, and `comp_hash` into the TUI/session catalog. The
  version field, independently from the effort menu, gates codex-rs's v2
  proactive multi-agent request policy.
- Apply user `[model.*]` entries last, so explicit operator configuration remains
  the highest-priority layer.

### Opt-in transport and user messaging

Both capabilities default off and are selected from model metadata, never
model names. Use synthetic model IDs in fixtures. Live `/models` responses,
the isolated Codex cache, and embedded
model JSON preserve `use_responses_lite` and `experimental_supported_tools`.
Missing fields in older caches do not enable either feature. Changing
providers clears the inherited values; there is no global enable switch.

- `use_responses_lite = true` enables the Codex-only Lite wire contract on the
  existing Responses endpoint and both compaction protocols. Requests carry
  `x-openai-internal-codex-responses-lite: true`, put client tools in a leading
  `additional_tools` input item, and put base instructions in a developer input
  message. Function/custom tools share the `functions` namespace. Top-level
  `instructions` and `tools` are omitted, `parallel_tool_calls` is false, and
  `reasoning.context` is `all_turns`. Input-image detail fields are omitted.
  Hosted tools are not advertised through Lite; the existing standalone search
  and image tools retain their own provider/auth availability gates.
- `experimental_supported_tools` must explicitly contain
  `send_user_message_async` to expose that tool. It is root-session-only and
  direct-only, including in Code Mode Only. It sends a normal visible assistant
  message and immediately returns `{"accepted":true}` without ending the turn
  or waiting for a reply. Replies use the existing user-message/interjection
  path. The message is persisted as an ACP update with the
  `x.ai/async_user_message` chunk metadata flag; buffering and replay preserve
  its separate, completed message boundary.

The two opt-ins are independent. Neither a reasoning effort nor a tool mode
enables them. Other providers cannot activate these Codex contracts by carrying
the same flags or supplying the Lite header in extra headers. Model switches
re-evaluate tool exposure and rebuild the sampling route; subagent routes
resolve their own model metadata and never expose async user messaging.

The cache is `$OPENGROK_HOME/codex_models_cache.json` and is matched against the
client version, endpoint, and non-secret Codex account identity. It is separate
from xAI's `models_cache.json`, just as Codex credentials are separate from xAI
credentials. A Codex refresh can neither remove xAI models nor read or mutate
xAI auth state.

### Native patch and v2 collaboration

The Codex Responses catalog independently selects these contracts:

```json
{
  "apply_patch_tool_type": "freeform",
  "multi_agent_version": "v2"
}
```

Missing or unknown selectors do not opt in. Neither capability is inferred
from a model name, reasoning effort, or the Lite flag.

- **Freeform patch:** Direct and mixed Code Mode replace the registered
  `apply_patch` function declaration with a native custom tool using the
  upstream Lark grammar. Code Mode Only keeps patches nested under
  `tools.apply_patch(raw_patch)`. The adapter reuses normal patch parsing,
  plan-mode gates, hooks, permissions, and write attribution. The original
  custom call identity produces matching custom outputs, including failures
  and denials; it does not manufacture an `exec` transport card.
- **Native collaboration:** Opted-in sessions use `spawn_agent`,
  `send_message`, `followup_task`, `wait_agent`, `interrupt_agent`, and
  `list_agents` with the v2 contract. The shared mailbox names switch
  implementations only on these routes. Other models retain the existing
  steering/passive-mail semantics. Ordinary host task, swarm, and workflow
  tools remain available, including plaintext cross-provider delegation.
- **Lifecycle:** Named agents use stable `/root/<task_name>` paths. Spawn
  acknowledges initialization, not task completion. `send_message` delivers
  promptly without starting an idle turn; `followup_task` starts or resumes
  a non-root agent through its owning parent. `interrupt_agent` cancels a
  turn without deleting the task identity or saved history. `wait_agent`
  returns activity summaries rather than message contents and yields to
  steered user input. Team, root, and self-target restrictions remain enforced.
- **Persistence and context:** An owner-only, atomically replaced
  `native_agents.json` in the root session stores names, resume references,
  and queued messages. A resumed child can have a new physical session ID;
  its canonical task name stays stable. `fork_turns` accepts `none`, `all`
  (the default), or a positive integer string. Existing clean-prefix,
  context-window, same-model fork, and cross-model digest safeguards remain.
- **Opaque wire data:** Codex requests that advertise encrypted collaboration
  schemas retain `namespace` and `encrypted_function_args` through the SDK
  boundary. Native `agent_message` items preserve provider-owned encrypted
  content, including Lite and compaction replay. Encrypted messages require
  a v2-capable Codex Responses destination; they cannot be forwarded as
  plaintext to other routes. Cross-provider projections, digests, and
  plaintext compaction artifacts omit private tool arguments.

This is an adapter over Open Grok's flat-team coordinator, not a replacement
with Codex's unrestricted agent hierarchy. Host spawn limits still apply
(default depth one), while v2 children keep communication and roster tools
even when they cannot spawn. Async user messaging remains root-only.

## Authentication

`open-grok login --codex` uses Codex's ChatGPT OAuth client, PKCE authorization
contract, callback ports, device-code flow, token refresh rules, and best-effort
revocation behavior. `open-grok login --codex --device-auth` is the headless form.

Codex credentials are auxiliary and isolated in `~/.opengrok/codex-auth.json`.
They never enter Grok's primary xAI `auth.json`, ACP auth-method ordering, or xAI
logout and billing state. The file uses the Codex `auth.json` token shape and
owner-only permissions. Bare `open-grok login` and `open-grok logout` remain xAI
commands; use `open-grok logout --codex` for Codex or `open-grok logout --all`
for both.

A Codex-selected headless session can start with only its model API key or this
isolated OAuth store; xAI authentication is not required. Codex provenance stays
in the resolved sampling config and never populates the process-wide ACP auth
cell. The session still observes that live cell, so signing into xAI later and
switching the same session to an xAI model activates xAI refresh normally.

OAuth sampling binds the session to the authenticated Codex account identity.
Bearer, account, and FedRAMP headers are resolved from one credential snapshot
and installed atomically on each request. A logout, missing credential, or
mid-session account change fails closed instead of falling back to a stale token
or mixing account headers. Permanent refresh failures are cached only for the
exact stored credential; a later login or token rotation clears that verdict.

The per-turn preflight uses only the selected provider's credential source. A
Codex 401 receives one immediate forced refresh and retry; xAI keeps its existing
auth-manager retry schedule. No Codex path may invoke or update xAI auth state.

Each logical Codex prompt also follows codex-rs's sticky-routing contract. The
first successful `/responses` response (or a legacy `/responses/compact`
operation) may supply
`x-codex-turn-state`; Open Grok replays that exact first value across retries,
tool-loop continuations, client rebuilds, and in-turn compaction. The value is
bound when a request is enqueued and never crosses into the next prompt or an
xAI request. Manual compaction owns a private operation-scoped value so a
concurrent prompt cannot replace its routing generation.

## Model routing

A model entry selects this contract explicitly with `provider = "codex"`.
ChatGPT OAuth requests use the Codex Responses endpoint, live bearer refresh,
account and FedRAMP headers when present, and the Codex originator header. An
explicit model API key remains authoritative and is never replaced by OAuth.

GPT-5.6 Sol's fallback entry uses the Responses API, a 353,000-token effective
context window, `code_mode_only`, and backend-hosted search. Max and Ultra stay
distinct in local session state while both encode as Codex `max`; Ultra adds the
same request-local proactive delegation policy used by codex-rs. Live catalog
metadata replaces these fallback capabilities when OpenAI changes them.

## Response continuity and reasoning summaries

Open Grok sends a stable `prompt_cache_key` derived from the session identity on
normal Codex Responses turns and both compaction protocols. Full-input HTTP
requests continue to send the complete provider-visible history and do not send
`previous_response_id`, matching codex-rs's HTTP contract. Response IDs are
observed for diagnostics; codex-rs reserves ID-based prefix reuse for its
WebSocket transport after validating the server prefix.

The stream treats `response.output_item.done` as the durable Codex output
carrier because `response.completed` may contain only response metadata and
usage. Finished messages, complete tool arguments, reasoning IDs, and
`encrypted_content` therefore survive persistence and byte-stable replay even
when the terminal response has an empty output array. A nonempty terminal output
remains authoritative so xAI and legacy streams are not duplicated.

Reasoning-summary support and its default mode come from the authenticated
model catalog. The request omits `reasoning.summary` for unsupported models or
the catalog's `none` mode and passes `auto`, `concise`, or `detailed` otherwise.
Summary deltas are scoped by output item and summary index, rendered once, and
attached to the matching durable reasoning item without losing its encrypted
payload. Unknown future top-level `response.*` side-channel events are ignored;
malformed known or nested output events still fail loudly.

Implicit auxiliary requests stay on the active Codex provider. In particular,
the compiled xAI defaults for session titles and image descriptions fall back
to the active Codex model instead of silently sending user content to xAI. A
non-default auxiliary model configured by the user remains an explicit
cross-provider opt-in. Auto-mode classification already inherits the active
model unless a dedicated classifier model is configured.

## Image preparation (tool outputs and history)

Codex's Responses endpoint rejects unsendable `image_url` values with a
non-retryable 400 of the form
`Invalid 'input[N].output[M].image_url' … invalid base64-encoded value`.
Open Grok follows codex-rs `image_preparation::prepare_response_items` on the
outbound request path:

- Before each Codex sample (and compaction), `ConversationRequest::prepare_images_for_codex`
  walks user messages, tool results, and custom-tool outputs.
- Remote `http(s)://` URLs, invalid `data:` base64 payloads (including unpadded
  or whitespace-tainted payloads), non-image MIME types, and tool-output
  `detail: low` are replaced with short text placeholders so the turn can
  continue.
- Code Mode's JS `image()` / `generatedImage()` helpers require a `data:` scheme
  (parity with codex-rs code-mode), rejecting bare paths and other schemes at
  the V8 boundary.
- As a safety net, `SamplingError::is_image_processing_error` also matches the
  Codex invalid-`image_url` 400 so the sampler can strip remaining images and
  retry once if a bad image still reaches the wire.

Open Grok does not yet re-encode/resize images through the full codex-utils-image
pipeline on every send; valid padded base64 data URLs pass through as-is after
the checks above.

## Sticky provider boundary

Each session persists an `ever_used_codex` marker. Once set, it is monotonic:
switching the session back to an xAI model does not reopen xAI remote sync,
relay, registry, feedback, or prompt-trace exports. A Codex subagent also sets
the marker on its parent, closing the boundary for the entire agent tree rather
than only the child session.

Codex sessions do not read, write, reindex, or embed the shared legacy xAI
memory store. Provider-less cumulative memory archives, full diagnostic-log
uploads, and recovered upload-queue spills are disabled until those artifacts
carry enough provider provenance to enforce the same boundary safely.

## Compaction

Codex sessions default to codex-rs's Remote Compaction V2 protocol for manual
and automatic compaction. Open Grok sends a normal streaming `POST /responses`
request with the active instructions, provider-visible input, tools, reasoning
controls, service tier, prompt-cache key, account-scoped auth, encrypted
reasoning include, and a final `{"type":"compaction_trigger"}` input item. The
request advertises `x-codex-beta-features: remote_compaction_v2`.

The dedicated collector requires `response.completed` and exactly one durable
`response.output_item.done` item of type `compaction`; unrelated stream items do
not become assistant history. On success, replacement history contains the
newest real-user messages within the same 64,000 approximate-token text budget,
followed by the opaque encrypted compaction carrier. User metadata wrappers are
removed, images and prompt markers are preserved, and a concurrent human steer
is retained only when the in-flight snapshot remains an exact prefix. Failed or
partial attempts install nothing. Three total attempts and one Codex auth
refresh are allowed, without retrying a partial V2 operation against a different
endpoint.

Setting `[features] remote_compaction_v2 = false` selects the retained legacy
unary `POST /responses/compact` implementation. Its provider-native output is
filtered with codex-rs's allowlist and replayed byte-for-byte. The two protocols
are selected before the operation; a V2 failure never silently falls back to
the unary endpoint.

Automatic compaction uses the live model's absolute
`auto_compact_token_limit`, clamped to 90% of the raw context window as in
codex-rs. When the field is absent, Open Grok derives the same 90%-of-raw
fallback from the catalog's effective context. A changed nonempty `comp_hash`
forces compatibility compaction before sampling. Explicit non-default Open
Grok threshold configuration remains an operator override. Switching a compacted
session to xAI uses only a bounded provider-neutral plaintext fallback; opaque
Codex payloads never cross the provider boundary.

## Usage and hosted search

The combined `/usage` command fetches xAI billing and Codex `/wham/usage`
independently. A failure from one provider does not hide the other. Codex quota
windows retain the backend's duration and reset time rather than assigning
fixed meanings to the primary and secondary slots.

Hosted search is provider-aware. xAI keeps its native web and X search tools.
Codex emits the OpenAI `web_search` tool, including supported filters and source
items, and never receives xAI-only `x_search`. Code Mode Only keeps hosted web
search top-level while local tools remain behind the JavaScript dispatcher.
The Responses adapter accepts xAI's current `x_search_call` output and progress
frames as a provider-executed X Search lifecycle, while retaining compatibility
with the earlier custom-tool representation.
If hosted search is unavailable, Codex does not silently fall back to the
compiled local xAI search model. The local tool remains hidden across in-place
model switches unless the user configured a non-default search model, which is
treated as an explicit cross-provider opt-in.

Image generation follows codex-rs's client-extension shape rather than
advertising the hosted Responses `image_generation` tool. Settings can select
**Grok Imagine** (the default) or **OpenAI Images**:

- Grok Imagine remains xAI-scoped. Codex bearer tokens are never reused for
  xAI image or video endpoints, and these tools stay hidden on non-xAI routes.
- OpenAI Images calls `POST /images/generations` and `/images/edits` below the
  Codex inference base, uses `gpt-image-2`, includes the Codex originator,
  account/FedRAMP headers, and `x-codex-image-turn-id`, and resolves the live
  bearer only from the identity-anchored `codex-auth.json` credential. The
  request/response shape mirrors `codex-api/src/endpoint/images.rs` and
  `ext/image-generation` at the pinned refresh.
- OpenAI Images is ChatGPT-OAuth-only. Upstream's `image_generation_available`
  gate (`core/src/tools/spec_plan.rs`) requires auth that `uses_codex_backend`,
  which excludes `AuthMode::ApiKey`; the fork mirrors this by surfacing only
  the isolated OAuth token store and by having the tool client refuse any
  static or legacy-provider bearer — no OpenAI API key path is offered.
- Known ChatGPT Free accounts do not receive the OpenAI image tools, matching
  codex-rs's plan gate (`account_plan_type() == Some(PlanType::Free)`).
  Unknown plan labels fail open to the server's authoritative entitlement
  check.
- The OpenAI selection is an explicit cross-provider opt-in, so an xAI or
  third-party chat model may use OpenAI Images without credential crossover.
  Video generation remains xAI-only.

## Maintenance

Future upstream changes must be reviewed and ported explicitly. Deliberate
divergences should be documented here and covered by focused regression tests.
The isolated credential store is intentional: sharing xAI's auth manager or ACP
primary-auth state would allow one provider's refresh or logout to damage the
other provider's session.
