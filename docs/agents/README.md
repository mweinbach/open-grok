# Open Grok — agent / developer documentation

This directory is for **contributors and AI coding agents** working on the Open Grok codebase.

- **Entry point:** [`../../AGENTS.md`](../../AGENTS.md) (keep that file scannable; put deep detail here).
- **End-user product guide:** [`../../crates/codegen/xai-grok-pager/docs/user-guide/`](../../crates/codegen/xai-grok-pager/docs/user-guide/)
- **Fork contracts (providers / Code Mode):** [`../provider-architecture.md`](../provider-architecture.md), [`../codex-provider-port.md`](../codex-provider-port.md), [`../code-mode-port.md`](../code-mode-port.md)

## Contents

| Document | Audience / use |
| --- | --- |
| [architecture.md](architecture.md) | Crate map, layering, binary entry, request flow |
| [agent-runtime.md](agent-runtime.md) | Session actor, turns, tools, permissions, plan, subagents, sessions |
| [acp.md](acp.md) | ACP transports, methods, extensions, reverse-RPC, meta keys, leader, headless |
| [sessions.md](sessions.md) | Session identity, on-disk layout, persistence actor, resume/fork/rewind, idle flush/dream |
| [subagents.md](subagents.md) | Task spawn, coordinator drain, depth, isolation, resume, usage fold, orphan reconcile |
| [session-bus.md](session-bus.md) | Machine-local cross-session bus: presence, peer messaging, list/read/message tools |
| [editing.md](editing.md) | How file edits work (`search_replace`, `apply_patch`, hunks, plan mode, Code Mode) |
| [code-mode.md](code-mode.md) | Code Mode / Only, V8 runtime, exec/wait, nested tools, transport UI, lifecycle |
| [tools.md](tools.md) | Tool packs, registry/finalize, taxonomy, major tools, caps, Computer Hub, add-a-tool checklist |
| [permissions-and-sandbox.md](permissions-and-sandbox.md) | Permission pipeline, rules, bash policy, folder trust, OS sandbox |
| [memory-and-goals.md](memory-and-goals.md) | Cross-session memory, flush/dream, memory tools, goal tracker / `update_goal` |
| [hooks-plugins-skills.md](hooks-plugins-skills.md) | Hooks discovery/events, plugins/marketplace, skills/commands session load |
| [tui-and-config.md](tui-and-config.md) | Pager Action/Effect, config layers, slash commands, hooks, plugins, skills, MCP |
| [providers.md](providers.md) | xAI / Codex / Kimi isolation, auth stores, compaction, safe extension checklist |
| [development.md](development.md) | Build, test, release, PR hygiene |

## How to use these docs

1. Read **AGENTS.md** for non-negotiables and a short feature map.
2. Open the specialized doc for the area you are changing.
3. Prefer **links into source modules** over re-copying large code samples.
4. When behavior changes, update the matching doc **in the same PR** if the contract for agents would otherwise go stale.

## Related paths in-tree

```text
AGENTS.md                          # repo root agent instructions
docs/                              # fork architecture + release notes
docs/agents/                       # this set
crates/codegen/xai-grok-pager/docs/user-guide/   # product UX docs
CONTRIBUTING.md
README.md
SECURITY.md
```
