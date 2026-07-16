# Codex provider integration

This document records Grok Build's OpenAI Codex provider compatibility target.
The implementation is based on the Codex Rust workspace at the same pinned
revision as the Code Mode port:

- Repository: <https://github.com/openai/codex>
- Commit: `2be648ba4a6c159a3d80b1c07e7323cbd5efef8f`
- License: Apache-2.0

The live model-catalog compatibility pass was refreshed against Codex commit
`cbc83d961e8132bfff4d340ab8342d181b79e95e`.

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
- Apply user `[model.*]` entries last, so explicit operator configuration remains
  the highest-priority layer.

The cache is `$OPENGROK_HOME/codex_models_cache.json` and is matched against the
client version, endpoint, and non-secret Codex account identity. It is separate
from xAI's `models_cache.json`, just as Codex credentials are separate from xAI
credentials. A Codex refresh can neither remove xAI models nor read or mutate
xAI auth state.

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

## Model routing

A model entry selects this contract explicitly with `provider = "codex"`.
ChatGPT OAuth requests use the Codex Responses endpoint, live bearer refresh,
account and FedRAMP headers when present, and the Codex originator header. An
explicit model API key remains authoritative and is never replaced by OAuth.

GPT-5.6 Sol's compatibility entry uses the Responses API, a 353,000-token
context window, `code_mode_only`, and backend-hosted search.

Implicit auxiliary requests stay on the active Codex provider. In particular,
the compiled xAI defaults for session titles and image descriptions fall back
to the active Codex model instead of silently sending user content to xAI. A
non-default auxiliary model configured by the user remains an explicit
cross-provider opt-in. Auto-mode classification already inherits the active
model unless a dedicated classifier model is configured.

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

## Usage and hosted search

The combined `/usage` command fetches xAI billing and Codex `/wham/usage`
independently. A failure from one provider does not hide the other. Codex quota
windows retain the backend's duration and reset time rather than assigning
fixed meanings to the primary and secondary slots.

Hosted search is provider-aware. xAI keeps its native web and X search tools.
Codex emits the OpenAI `web_search` tool, including supported filters and source
items, and never receives xAI-only `x_search`. Code Mode Only keeps hosted web
search top-level while local tools remain behind the JavaScript dispatcher.
If hosted search is unavailable, Codex does not silently fall back to the
compiled local xAI search model. The local tool remains hidden across in-place
model switches unless the user configured a non-default search model, which is
treated as an explicit cross-provider opt-in.

xAI Imagine media tools are provider scoped. Codex bearer tokens are never
reused as static credentials for xAI image or video endpoints, and xAI media
tools are hidden while Codex is active, including after an in-place model
switch.

## Maintenance

Future upstream changes must be reviewed and ported explicitly. Deliberate
divergences should be documented here and covered by focused regression tests.
The isolated credential store is intentional: sharing xAI's auth manager or ACP
primary-auth state would allow one provider's refresh or logout to damage the
other provider's session.
