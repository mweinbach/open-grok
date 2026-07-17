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

## Compatibility contract

When Code Mode Only is effective:

1. The Responses API exposes provider-compatible `exec`, the JSON-schema
   `wait` function tool, and Codex-style direct-only exceptions for human
   interaction and multi-agent lifecycle controls. Codex uses native
   custom/freeform `exec`; xAI uses an ordinary function envelope.
2. Codex native `exec` accepts raw JavaScript. xAI's function envelope carries
   the same raw JavaScript in its required `source` string field.
3. Ordinary Grok Build tools remain registered but are hidden from the model's
   top-level tool list. JavaScript reaches them through the generated `tools.*`
   namespace.
4. A JavaScript cell may complete, yield for nested tool calls, or continue in the
   background. `wait` resumes or terminates a yielded cell by identifier.
5. Tool results and errors cross the JavaScript boundary without losing their
   structured content.
6. The JavaScript runtime is persistent for a compatible agent timeline,
   replaced on rewind or incompatible provider/transport changes, and disposed
   when that session ends. Stale callbacks and yielded cell IDs fail closed.
7. Direct-only collaboration controls remain top-level and are excluded from the
   generated `tools.*` namespace, matching Sol's multi-agent-v2 policy.
8. `exec` and `wait` remain in model history but are transport details, not TUI
   tool cards. The UI shows only the decoded nested tools and their ordinary
   structured results; raw JavaScript, wait arguments, and cell transport output
   stay hidden during live streaming and session replay.

An implementation that exposes Codex native `exec` as a JSON-schema function,
sends native custom tools to xAI, or starts a fresh JavaScript process for every
call is not compatible with this contract.

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
`FunctionEnvelope` for xAI, and fail-closed `Unsupported` elsewhere.

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

All six phases are implemented against the pinned revision. Grok Build uses the
upstream embedded V8 provider; the optional out-of-process `code-mode-host` is not
included. This keeps the execution and persistence contract while avoiding a
second process-management path.

The user-visible event behavior was rechecked against Codex commit
`cbc83d961e8132bfff4d340ab8342d181b79e95e`. That revision records outer custom
calls as raw response history but does not map them to typed TUI turn items;
nested Code Mode invocations re-enter the normal tool dispatcher. Open Grok
mirrors that split and also removes transport wrappers from legacy replay data.

## Provenance and maintenance

Ported source must retain its Apache-2.0 headers where present and be listed in the
repository's third-party notices. Any deliberate divergence from the pinned Codex
behavior should be documented beside the adapter and covered by a regression test.
