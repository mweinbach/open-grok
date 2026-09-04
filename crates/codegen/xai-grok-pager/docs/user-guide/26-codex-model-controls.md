# Codex context and persistent work

Open Grok uses the Codex 0.153.1 model-catalog contract. GPT-6-astra appears by
its public name. Live catalog entries control reasoning, tools, and service
tiers. Ultra uses the model's advertised multi-agent effort; Max remains Max.
Replacement and retirement notices appear in model descriptions without
automatically switching your selected model.

## Context overrides

In **Settings → Models → Custom models**, use the model's existing catalog key
to override it. For GPT-6-astra, use `gpt-6-astra` and provider `codex`.
**Codex raw context override** accepts a budget above the catalog maximum.
When it is nonzero, saving uses that raw budget instead of the usable-context
field. Set it to zero to use the usable-context field.

You can also edit `$OPENGROK_HOME/config.toml` (normally `~/.opengrok/config.toml`):

```toml
[model.gpt-6-astra]
max_context_window = 1000000
auto_compact_token_limit = 850000 # optional
```

`max_context_window` is the raw budget, including headroom. At 95% effective
context, 1,000,000 raw tokens gives 950,000 usable tokens. Without an explicit
compaction override, automatic compaction begins at 900,000 tokens.

Existing `context_window` values mean usable tokens. If you set both fields,
`context_window` takes precedence. An explicit `auto_compact_token_limit` is
capped at 90% of the resolved raw budget; a custom compaction percentage keeps
its existing precedence. Overrides change the client's accounting and
compaction behavior; the inference server still enforces its actual limits.

To restore catalog defaults, remove the model override fields or remove the
custom entry in Settings. Model configuration changes refresh the catalog and
rebind the session through the normal provider/settings path.

## Persistent work and browser review

**Settings → Agent → Codex persistent work** keeps the root agent working on
relevant authorized follow-ups and monitoring while it sends progress messages.
It requires a model with async user messaging. Codex's persistent mode disables
reasoning on the wire. Escape cancels normally; the mode does not expand your
authorization or enable itself on subagents.

**Codex browser action review** uses available model Guardian guidance to
review `node_repl`/`cua_repl` browser and computer calls. Calls needing review
use the normal permission prompt. If review is unavailable, permission is
requested. Existing deny rules and sandbox restrictions remain in force.

Both settings are off by default and require restarting Open Grok:

```toml
[ui]
codex_persistent_mode = true
codex_guardian_review = true
```

Browser/computer confirmation policies are supplied to compatible MCP servers
per call and follow the active model. They do not change shell permissions.
