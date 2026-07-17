# Open Grok — agent / developer documentation

This directory is for **contributors and AI coding agents** working on the Open Grok codebase.

- **Entry point:** [`../../AGENTS.md`](../../AGENTS.md) (keep that file scannable; put deep detail here).
- **End-user product guide:** [`../../crates/codegen/xai-grok-pager/docs/user-guide/`](../../crates/codegen/xai-grok-pager/docs/user-guide/)
- **Fork contracts (providers / Code Mode):** [`../provider-architecture.md`](../provider-architecture.md), [`../codex-provider-port.md`](../codex-provider-port.md), [`../code-mode-port.md`](../code-mode-port.md)

## Contents

| Document | Audience / use |
| --- | --- |
| [architecture.md](architecture.md) | Crate map, layering, binary entry, request flow |
| [agent-runtime.md](agent-runtime.md) | Session actor, turns, tools, permissions, plan, subagents, sessions, ACP |
| [editing.md](editing.md) | How file edits work (`search_replace`, `apply_patch`, hunks, plan mode, Code Mode) |
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
