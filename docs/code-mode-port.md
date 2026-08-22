# Codex Code Mode port

This document records the compatibility target and implementation plan for bringing
OpenAI Codex Code Mode to Grok Build.

Provider authentication, quota, and hosted-search compatibility are recorded in
[Codex provider integration](codex-provider-port.md).

## Upstream baseline

- Repository: <https://github.com/openai/codex>
- Commit: `2be648ba4a6c159a3d80b1c07e7323cbd5efef8f`
- License: Apache-2.0
- Model contract: the upstream `gpt-5.6-sol` catalog entry selects
  `tool_mode: code_mode_only`.

The commit is intentionally pinned. Future upstream changes must be reviewed and
ported explicitly rather than silently changing the runtime contract.

The July 2026 selective sync reviewed upstream through
`53d06e24ea318a963812030fa8fed1bd0fc42d42`. It ports the 30-second buffered
`exec` default, the one-second grace for yields of at least ten seconds,
first-wins normalized tool-name collision coverage, V8 150.4.0, and standalone
provider search. It deliberately does not adopt upstream's external
`codex-code-mode-host` process.

This sync also leaves upstream #34588's sampled MCP catalog-revision binding for
a dedicated reconnect-during-turn correctness and security audit. It does not
change MCP startup/runtime assembly, telemetry metadata, structured read/edit
tools, or audio output.

An August 2, 2026 parity audit reviewed upstream through
`2b5bdcf67547860f2e5c5a605009a70026796b2b`. No web-search or `wait` protocol
semantics changed after the July selective-sync baseline. A later runtime
hardening commit made sandboxed V8 mandatory upstream. The pinned rusty_v8
150.4.0 release does not publish sandbox-enabled archives for Open Grok's
supported Apple Silicon or Windows targets, so enabling that feature would
break normal and release builds. This remains an explicit hardening gap pending
a reproducible source-built or upstream-published archive; the external
`codex-code-mode-host` process also remains intentionally unported.

## Compatibility contract

When Code Mode Only is effective:

1. The Responses API exposes provider-compatible `exec`, the JSON-schema
   `wait` function tool, and Codex-style direct-only exceptions for human
   interaction and multi-agent lifecycle controls. Codex uses native
   custom/freeform `exec`; xAI and DeepSeek use an ordinary function envelope.
2. Codex native `exec` accepts raw JavaScript. The xAI/DeepSeek function
   envelope carries the same raw JavaScript in its required `source` string
   field.
3. Ordinary Grok Build tools remain registered but are hidden from the model's
   top-level tool list. JavaScript reaches them through the generated `tools.*`
   namespace.
4. A JavaScript cell may complete, yield for nested tool calls, or continue in the
   background. `wait` resumes or terminates a yielded cell by identifier.
5. Tool results and errors cross the JavaScript boundary without losing their
   structured content. Successful nested `apply_patch` may resolve to `{}`;
   failed patches must reject the JS promise with the patcher's diagnostic.
   A pending nested-tool promise supports `p.onProgress(handler)` for actual
   incremental `{ text, payload? }` chunks; an absent structured payload is
   omitted. Register before awaiting; registering again replaces the handler.
   Delivery is observation-only, FIFO, and bounded to 64 queued chunks per
   invocation; overflow drops the oldest chunk. Missing or throwing handlers,
   closed calls, and stale runtime generations never change the terminal tool
   result.
6. The JavaScript runtime is persistent for a compatible agent timeline,
   replaced on rewind or incompatible provider/transport changes, and disposed
   when that session ends. Stale callbacks and yielded cell IDs fail closed.
7. Direct-only collaboration controls remain top-level and are excluded from the
   generated `tools.*` namespace, matching Sol's multi-agent-v2 policy.
8. `exec` and `wait` remain in model history but are transport details, not TUI
   activity. The UI streams the actual nested tools, their genuine progress,
   and their ordinary structured results, whether or not JavaScript registers
   `onProgress`. Raw JavaScript, wait arguments, source/payload fragments, cell
   transport output, wrapper titles/spinners, and ephemeral transport blocks
   stay hidden during live streaming, continuation chunks, and session replay.
9. On a supported native Codex Responses route, `web__run` replaces the hosted
   web-search declaration and is callable inside JavaScript as
   `tools.web__run(...)`. Unsupported routes keep hosted search.

An implementation that exposes Codex native `exec` as a JSON-schema function,
sends unsupported native custom tools to xAI or DeepSeek, or starts a fresh
JavaScript process for every call is not compatible with this contract.

### Standalone web-search adapter

Upstream registers a `web` namespace containing `run`; Open Grok's registry
stores the equivalent flat name `web__run`. Both produce the same Code Mode
JavaScript binding, `tools.web__run(...)`. The model-facing description mirrors
upstream verbatim apart from that three-place name substitution, and optional
command fields remain optional but non-null in the exported schema.

The `/alpha/search` request keeps the previous and current visible user turns,
shares a 1,000-token budget across intervening assistant text, omits `input`
when no visible user message exists, and uses the 10,000-token output policy
advertised by the latest Codex Code Mode models. Open Grok intentionally
requests direct live external search. It reuses the active Codex route's bearer
resolver, account and originator headers; it does not synthesize upstream's
`x-codex-turn-metadata` payload, which Open Grok does not otherwise model.
Structured result DTOs remain opaque and forward-compatible while the tool
returns the endpoint's textual `output` to JavaScript.
Supporting a future model with a different search-output policy requires
explicit model metadata; the adapter must not infer that policy from a slug.

## Configuration behavior

Settings gains a restart-required **Code mode** selector with three explicit
values: `direct`, mixed `code_mode`, and `code_mode_only`. Mixed Code Mode keeps
ordinary tools available top-level alongside `exec` and `wait`, and is the
normal choice for xAI. Legacy booleans remain readable (`false` maps to Direct,
`true` maps to mixed Code Mode), while new writes use the enum strings.

Only an OpenAI Codex model requirement takes precedence: a model such as
GPT-5.6 Sol that declares `code_mode_only` cannot be made incompatible through
Settings. That requirement is rejected at spawn or model switch if the route is
not Responses-backed. User Code Mode preferences on other unsupported backends
fall back to Direct. Restart Open Grok after changing the setting because the
running process retains the configuration loaded at startup.

The resolved mode, precedence source, and provider transport are persisted for
cold resume. Existing sessions therefore retain their policy after Settings or
catalog drift, except that a current Codex model requirement still wins. Code
Mode routes are capability-driven: `NativeCustomGrammar` for Codex,
`FunctionEnvelope` for xAI and DeepSeek Responses, and fail-closed
`Unsupported` elsewhere.

## Implementation phases

1. Port the upstream Code Mode protocol and embedded V8 session runtime into
   isolated, attributed crates with focused runtime tests.
2. Extend the Responses transport to serialize Codex custom/freeform tools and
   round-trip `custom_tool_call` plus `custom_tool_call_output` items, while
   projecting Code Mode declarations and history to function calls for xAI.
3. Add a tool-mode selector to model metadata and compute the effective mode
   from provider/backend capability, the user preference, and Codex-only model
   requirements.
4. Adapt the finalized tool registry so both Code Mode variants expose `exec`
   and `wait`, while Code Mode Only moves ordinary tools exclusively behind the
   nested dispatcher.
5. Add the persisted Settings switch, restart messaging, reset/rollback behavior,
   and end-to-end Settings coverage.
6. Run focused protocol, runtime, sampler, tool-registry, configuration, and pager
   tests followed by formatting and lint checks for the affected crates.

All six phases are implemented against the pinned revision plus the selective
sync recorded above. Open Grok keeps an embedded V8 provider, now on V8 150.4.0,
even though upstream removed that path in favor of `codex-code-mode-host`. This
keeps the execution and persistence contract while avoiding a second
process-management path.

`exec` defaults to a 30-second observation window while `wait` remains at ten
seconds. Both receive a one-second runtime grace when the requested observation
window is at least ten seconds, allowing a nested tool that completes just after
the advertised deadline to return without another model turn.

The user-visible event behavior was rechecked against Codex commit
`cbc83d961e8132bfff4d340ab8342d181b79e95e`. That revision records outer custom
calls as raw response history but does not map them to typed TUI turn items;
nested Code Mode invocations re-enter the normal tool dispatcher. The required
Open Grok split keeps genuine nested tool cards/progress visible, suppresses all
outer transport fragments rather than rendering transient wrappers, and removes
transport wrappers from legacy replay data. JavaScript progress observation
through `p.onProgress` is additive and must not replace visible ACP progress.

## Provenance and maintenance

Ported source must retain its Apache-2.0 headers where present and be listed in the
repository's third-party notices. Any deliberate divergence from the pinned Codex
behavior should be documented beside the adapter and covered by a regression test.
