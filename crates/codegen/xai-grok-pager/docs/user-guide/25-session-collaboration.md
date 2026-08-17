# Session collaboration across terminals

Open Grok sessions running on the same machine can see and message each other — across every project, terminal window, and process. This is the **session bus**, and it is on by default.

## What it does

While at least one session is live in an Open Grok process, that process announces its sessions under `~/.opengrok/session-bus/`. Every other Open Grok process reads that directory to discover live sessions and dials the owning process directly to deliver messages. There is no separate server to start or manage; a crashed terminal just stops appearing in listings after a few seconds.

## The tools

Every session gets three tools for this (they appear alongside the subagent collaboration tools):

| Tool | What it does |
| --- | --- |
| `list_sessions` | Live Open Grok sessions on this machine — project, model, title, busy/idle status, and which one is the calling session |
| `read_session` | The recent conversation of another live session, from its saved history |
| `message_session` | Send a message to another live session |

When you ask something like *"check whether my other session already fixed this"* or *"tell the session in the web app to hold off"*, the model uses these tools. A recipient that is mid-turn receives the message at its next turn boundary; an idle recipient wakes up with it. The recipient's model decides what to do and can reply the same way. Incoming peer messages show up in the timeline as a card and are saved with the session.

Example: with five terminals open across five projects, ask one session to `list_sessions`, read what a busy session in another project is doing, and send it a short coordination message — no copy-pasting context between terminals.

## Safety

- Peer messages are **untrusted model-authored input**. They never carry your permissions or approvals — a message from another session cannot authorize anything on its own.
- The bus is machine-local and per-user: sessions under your own `~/.opengrok` only. Nothing crosses the network.
- Message bodies are capped (32 KiB) and rendered as bounded previews.

## Turning it off

Add to `~/.opengrok/config.toml`:

```toml
[session_bus]
enabled = false
```

A process with the bus disabled announces nothing, cannot be messaged, and its sessions' `list_sessions` reports the bus as disabled.
