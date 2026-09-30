# Subagents and Personas

Subagents are independent child sessions that handle tasks in parallel. Each subagent has its own context window, so the main agent can delegate work (research, implementation, testing, and code review) without consuming its own context. A subagent reports a summary back to the parent when it finishes.

Subagents are enabled by default.

---

## Agents vs Personas

Agents and personas both customize behavior, but they operate at different levels:

| | **Agents** | **Personas** |
|---|---|---|
| **What they configure** | The whole session: model, tools, prompt mode, system prompt | A behavioral overlay added to a subagent's prompt |
| **Scope** | Primary session or subagent | Subagents only |
| **How you set them** | At startup, or with agent definitions (`.md` files in `.opengrok/agents/` or `~/.opengrok/agents/`) | In `config.toml` (`[subagents.personas]`) or `.toml` files under `.opengrok/personas/`; applied during subagent resolution |
| **What they control** | Model, tool availability, prompt body, skills | Tone, output format, task focus, and input/output contracts |
| **Who edits them** | You -- create, delete, or toggle them in the agents modal or by editing files | You -- define custom personas in config or files; bundled personas are read-only |
| **Examples** | `grok-build`, `explore`, `plan` | `researcher`, `concise` |

An agent defines the session itself. A persona shapes how a subagent behaves within a session. A subagent always runs as an agent type (for example, `general-purpose`), and resolution can layer a persona on top.

Manage both in the agents modal. Open it with `/config-agents` (alias `/agents`), or open the Personas tab directly with `/personas`. The modal has two tabs: **Agents** and **Personas**.

---

## Disabling Subagents

Disable subagents with a CLI flag, an environment variable, or the config file (highest priority first). The same rules apply to the interactive `open-grok` TUI, `open-grok agent stdio`, and headless runs.

```bash
grok --no-subagents                  # This session only
export GROK_SUBAGENTS=0              # Environment variable
```

```toml
# ~/.opengrok/config.toml
[subagents]
enabled = false
```

Only an explicit `enabled = false` turns subagents off. A `[subagents]` table that sets `max_depth`, `[subagents.models]`, or `[subagents.toggle]` without an `enabled` key keeps them on.

---

## How Subagents Work

When the main agent identifies work to delegate, it calls the `spawn_subagent` tool to start a child session. The child runs with:

- Its own context window, independent of the parent
- A toolset determined by its agent type and optional capability mode
- Optional persona instructions applied during resolution

The parent receives the child's output -- usually a summary -- when the child finishes.

---

## Built-in Agent Types

Built-in types still exist as host types. The model-facing spawn schema omits `subagent_type`. An omitted key is `general-purpose`.

| Type              | Description                                          |
| ----------------- | ---------------------------------------------------- |
| `general-purpose` | Default type. Full-capability agent for any task.    |
| `explore`         | Research agent. Searches, reads, greps, and runs shell commands, but does not edit files. Use it for codebase investigation. |
| `plan`            | Planning agent. Explores the codebase and produces a structured implementation plan; does not edit files. |

Project- or user-defined agents can add new types or shadow these built-ins by name.

---

## Personas

A persona is a named behavioral overlay. Its instructions are injected into the subagent's conversation as a `<system-reminder>`, which shapes tone, output format, and task focus without changing the subagent's agent type, model, or tools.

Define personas in `config.toml` or in `.toml` files:

```toml
[subagents.personas.researcher]
instructions = "You are a thorough researcher. Always cite specific file paths."
description = "Deep investigator."
```

Grok Build discovers file-based personas from these locations, in priority order:

- `.opengrok/personas/*.toml` (project)
- `~/.opengrok/personas/*.toml` (user)
- The bundled personas directory (lowest priority)

Each file defines one persona, and the file name (without the extension) becomes the persona name. Inline `config.toml` personas take precedence over files. Only `.toml` files are discovered.

Manage personas in the Personas tab of the agents modal (`/personas`). Bundled personas are read-only; personas you define are editable.

> **Note:** Grok Build applies personas through subagent resolution and roles, not through a `spawn_subagent` parameter. The main agent does not pass a persona name when it spawns a child.

### Persona Fields

| Field               | Description                                                          |
| ------------------- | ------------------------------------------------------------------- |
| `instructions`      | Inline instruction text applied as the persona layer.               |
| `instructions_file` | Path to an instruction file, loaded at spawn time and merged after `instructions`. |
| `description`       | Short summary shown in the persona catalog. Falls back to the first paragraph of `instructions`. |
| `inputs` / `outputs`| Declared input and output contract (see below).                     |
| `model`             | Model override applied when the persona is used.                    |
| `reasoning_effort`  | Reasoning effort applied when the persona is used.                  |
| `default_isolation` | Default isolation mode (`none` or `worktree`).                      |

### Input/Output Contracts

A persona can declare the inputs it expects and the outputs it produces. The parent agent reads these to know what context to supply and what artifacts to expect. This lets you chain personas, so one persona's output file becomes the next persona's input:

```toml
[[subagents.personas.reviewer.inputs]]
name = "review_file"
io_type = "file"
required = true
description = "Path to the code under review"

[[subagents.personas.reviewer.outputs]]
name = "summary_file"
io_type = "file"
required = false
description = "Path to write review notes"
```

Each field has a `name`, an `io_type` (defaults to `file`), a `required` flag, and a `description`.

### Persona Resolution

When a persona applies, Grok Build resolves the effective model and reasoning effort in this order, highest priority first:

1. Explicit spawn-time override
2. Role default
3. Persona default
4. Parent session

Isolation follows the same order for the first three steps but defaults to `none` (no worktree) rather than inheriting from the parent session.

If a persona is requested but cannot be resolved -- it is not found, has no instructions, or its `instructions_file` is unreadable -- the spawn fails.

---

## Spawning Subagents

The main agent calls the `spawn_subagent` tool. Its parameters:

| Parameter           | Description                                                       |
| ------------------- | ---------------------------------------------------------------- |
| `prompt`            | The full task prompt for the subagent.                           |
| `description`       | A short label for the task (3-5 words).                          |
| `subagent_type`     | The agent type to launch. Defaults to `general-purpose`.         |
| `run_in_background` | Run in the background and return a subagent ID. Defaults to `true`. |
| `capability_mode`   | Restrict the subagent's tools: `read-only`, `read-write`, `execute`, or `all`. |
| `isolation`         | `none` (shared workspace, the default) or `worktree` (isolated git worktree). |
| `resume_from`       | Continue a completed subagent's conversation. Pass its subagent ID. |
| `cwd`               | Working directory for the subagent. Mutually exclusive with `isolation: worktree`; ignored when `resume_from` is set (the resumed child inherits its source's directory). |
| `model`             | Optional model slug. Omit it to inherit the parent model; resumed agents keep their source model. |
| `reasoning_effort`  | Optional effort: `none`, `minimal`, `low`, `medium`, `high`, `xhigh`, `max`, or `ultra`. Omit it to use role/persona defaults and then the parent session. It may be supplied when resuming and must be supported by the effective model. |

When you run a subagent in the background, retrieve its result later with `get_command_or_subagent_output`.

---

## Agent Mailboxes

The root agent and its live subagents form one collaboration team. Every member
can discover the team and exchange explicitly addressed messages without
exposing another agent's transcript.

| Tool | Behavior |
| --- | --- |
| `list_agents` | Lists the root and subagents with stable IDs, lifecycle status, task labels, resume provenance, and worktree paths. |
| `send_message` | Steers a live agent: running recipients receive it mid-turn, idle recipients are woken, and recipients that have not started yet receive it as soon as they start. |
| `followup_task` | Queues a follow-up task in the recipient's mailbox without interrupting its current work. The recipient reads queued tasks with `wait_agent`. |
| `wait_agent` | Reads only the calling agent's inbox. Steering messages arrive automatically; use this to pick up queued follow-up tasks. Omit `timeout_ms` to wait up to 30 seconds, or pass `0` to poll. |

Mailboxes are scoped to the root session: an agent cannot list or address agents
owned by another session. Messages are shown in the root conversation and
persisted in its session history for auditability. They are always treated as
untrusted model-authored input, never as user consent or permission approval.

Completed children are no longer live mailbox recipients. Continue one with
`spawn_subagent(resume_from="<agent-id>")`; the resumed run receives a new ID.

---

## Agent Swarms

Swarm mode asks the main agent to split suitable independent work into one coordinated `agent_swarm` call. All members run in the foreground and their results return together in the original input order.

Use the slash controls:

```text
/swarm                    # toggle persistent manual mode
/swarm on
/swarm off
/swarm review each API module for auth bugs
```

`/swarm <task>` applies to that task for one turn and then turns itself off. If manual swarm mode was already on, it stays on. You can also enable the default from **Settings → Swarm mode** or in config:

```toml
[ui]
swarm_mode = true
```

When active, the footer shows a `swarm` badge. A swarm appears in scrollback as one expandable purple card with a tree row for every member, kept in input order. The purple accent pulses while work is active; per-member states retain green, amber, and red status colors. Ordinary subagent cards use a warm orange label and running accent, so they remain visually distinct. The swarm card summarizes queued, running, completed, failed, and cancelled members and shows live turn/tool counts, duration, and context usage when available. Child transcripts are still available from the tasks pane.

The model-facing `agent_swarm` tool supports:

| Parameter | Description |
| --- | --- |
| `description` | Shared short label for the swarm. |
| `subagent_type` | Type for new members; defaults to `general-purpose`. |
| `model` | Optional model slug for new members. Resumed members keep their original model. |
| `reasoning_effort` | Optional effort applied to every new or resumed member. |
| `items` | Ordered work items. At least two are required unless resuming; total members are capped at 128. |
| `prompt_template` | Required with `items`; must contain literal `{{item}}`. |
| `resume_agent_ids` | Ordered object mapping completed subagent IDs to continuation prompts. Resumed members run first and keep their original profile. |

Open Grok validates the full swarm before starting any child. It launches up to five members immediately, then ramps additional members every 700 ms. If an in-process provider rate-limits a member, the swarm card shows that live child as waiting while the scheduler retries the same session after 3 s, 6 s, 12 s, and progressively longer delays. Waiting retries take priority over resumes and new members; concurrency shrinks during repeated rate limits and recovers after a quiet period. A rate-limited member fails normally when it is the only unfinished member, so the swarm cannot remain suspended forever.

The transcript shows each send as a one-line `Message` row: a verb for the outcome, then the subagent's label (its persona, role, tag, or Subagent fallback) and its description in curly quotes, as its `Subagent …: “…”` scrollback row quotes it, clamped to the first line and 40 characters. The verb carries the delivery, so a steer stays unmarked:

- `Message sent to Subagent “find callers”` (steer)
- `Message queued for Subagent “find callers”` / `Message interjected to Subagent “find callers”`
- `Message sending to …` with an animated bullet while the send is in flight
- `Message rejected · Subagent “find callers”` for a refused send, `Message unconfirmed · Subagent “find callers”` for one the shell could not confirm
- `Message sent to parent` when a child messages its parent

Swarms use the same flat subagent tree: swarm members cannot spawn `task` or another `agent_swarm`.

---

## Workflows

The `workflow` tool lets the agent orchestrate many subagents with a [Rhai](https://rhai.rs) script instead of individual tool calls — deterministic control flow (loops, fan-out, verification passes) with real concurrency. Where `agent_swarm` runs one prompt template over a list of items, a workflow script can express multi-stage pipelines, adversarial verification, judge panels, and loop-until-done discovery.

Every script starts with a literal `meta` header and then drives agents with the built-in host functions:

```rhai
let meta = #{
    name: "review-changes",
    description: "Review changed files, verify findings",
    phases: [ #{ title: "Review" }, #{ title: "Verify" } ],
};
phase("Review");
let jobs = [];
for f in files {
    jobs.push(#{ prompt: "Review " + f + " for bugs", label: "review:" + f, phase: "Review" });
}
let findings = parallel(jobs);
phase("Verify");
let verdicts = [];
for finding in findings {
    verdicts.push(agent("Adversarially verify: " + json_encode(finding), #{ phase: "Verify" }));
}
complete(#{ verdicts: verdicts });
```

Key host functions:

| Function | Behavior |
| --- | --- |
| `agent(prompt, opts?)` | Spawns a subagent and returns `#{agent_id, success, output, cancelled, tokens_used, duration_ms}`. `opts`: `label`, `phase`, `model`, `reasoning_effort`, `agent_type`, `capability_mode` (`"read-only"`/`"read-write"`/`"execute"`/`"all"`), `isolation_worktree`, `resume_from`, `output_schema` (JSON-Schema contract enforced host-side with one corrective retry). |
| `parallel([opts, ...])` | Runs many agent specs concurrently; order-preserving, failures become `()`. |
| `phase(title)` / `log(msg)` | Progress grouping and narration in the workflow card and `/workflows` overlay. |
| `complete(value?)` / `pause(kind, msg)` / `await_user(kind, msg)` | Terminate the run, pause it, or pause once for user input (resume passes through). |
| `escalate(msg)` | Pause once as **blocked**, waking the session agent with `msg`; on resume it returns the resume note the agent supplied (empty string without one), so scripts can hand a blocker back instead of dying. |
| `budget()` | `#{total, spent, reserved, remaining}` agent-call accounting. |
| `write_scratch_file` / `read_scratch_file` / `render_template` / `git_diff_since` | Scratch artifacts, prompt templates, and a bounded diff of the workspace. |
| `fingerprint(text)` / `json_encode(value)` | Hash and safely fence untrusted data into prompts. |

Workflows always run **in the background**: the launch returns immediately, the conversation stays free while agents work, and completion is injected back into the session automatically — no polling. A run that pauses itself with any kind except `user`, or that fails, is handed back the same way: the session agent is woken with the blocking issue and resume instructions, so it can fix the cause and resume the run — a failed step re-executes live, an `await_user` gate is simply passed, and an `escalate()` gate receives the resume note as its return value (a plain `pause()` replays deterministically, so a pause about launch input needs a corrected new run). Only user-kind pauses — a deliberate `/workflow pause`, or a script pause with kind `user` — stay quiet until resumed. Watch and manage runs in the `/workflows` overlay (live phases, per-agent progress and tokens), or with `/workflow pause|resume|stop|save <name>`.

Budgets are counted in **agent calls** (default 128, max 1024 per run): every `agent()` call and `parallel()` item consumes one slot. A budget-limited run can be resumed with a strictly higher `agent_budget`; journaled calls replay without re-running.

Runs are journaled under the session directory. Resuming (`resume_from_run_id`, same process only) replays completed host calls instantly and re-runs only what hadn't finished; the script and `args` must be byte-identical to the original run. Wall-clock (`timestamp()`), `sleep()`, and `exit()` are unavailable inside scripts so replays stay deterministic — pass timestamps through `args`. A process restart marks active runs interrupted (start a new run; the launch persists an editable `script_path` for iteration).

Reusable workflows live in a three-scope registry — builtin (`deep-research` and `ultracode` ship in the binary), project (`<repo>/.opengrok/workflows/*.rhai`, folder-trust gated), and user (`~/.opengrok/workflows/*.rhai`) — and each registered workflow also surfaces as its own slash command (e.g. `/deep-research`, `/ultracode`). `/workflow save <name>` persists a run's script to the project scope.

A session runs at most 4 active workflows; a run is capped at 1024 agent calls; and workflow members follow the same flat tree: they cannot spawn `task`, `agent_swarm`, or another `workflow`.

---

## Capability Modes

A capability mode is an optional, coarse filter on a subagent's tools:

| Mode         | Read | Write | Execute | Description                                  |
| ------------ | ---- | ----- | ------- | -------------------------------------------- |
| `read-only`  | Yes  | No    | No      | Read, search, and inspect (also web search and LSP); no file edits or shell. |
| `read-write` | Yes  | Yes   | No      | Read, plus create, edit, delete, and move files. No shell. |
| `execute`    | Yes  | No    | Yes     | Read, plus run shell commands and background tasks. No file edits. |
| `all`        | Yes  | Yes   | Yes     | Unrestricted tool access.                    |

If you omit `capability_mode`, the subagent uses its agent type's toolset. The built-in `explore` and `plan` types read, search, and run shell commands but cannot edit files; `general-purpose` ships the full toolset.

---

## Context Inheritance

### resume_from

The `resume_from` parameter lets a new subagent continue where a completed subagent left off, which is useful for multi-stage workflows:

1. Spawn a research subagent to investigate a problem.
2. Spawn a second subagent with `resume_from` set to the first subagent's ID, so it picks up with the full research context.

The new subagent inherits the source's transcript, tool state, and model; its system prompt and tools are re-rendered from the current agent definition. The source must be completed (not running), belong to the current session, and use the same agent type.

### MCP inheritance

The primary session overlays the active agent’s `mcpServers` frontmatter onto the disk/client merge by name (agent.md headers beat `config.toml`). Switching the primary agent replaces that overlay with the new seat only. Child inline `mcpServers` still become owned clients and beat inherited shared clients. Plugin agents cannot declare `mcpServers`.

Subagents inherit the parent session’s **already-connected** MCP servers by default. That includes local stdio/HTTP servers and plugin-sourced agents (for example `my-plugin:reviewer`). The child discovers and calls those tools with `search_tool` / `use_tool` the same way the parent does.

Control inheritance with agent frontmatter `mcpInheritance`:

| Value | Effect |
| ----- | ------ |
| `all` (default if omitted) | Inherit every parent-connected MCP server |
| `none` | Inherit no parent MCP servers |
| `named: [server, …]` | Inherit only the listed server names |
| `except: [server, …]` | Inherit all parent servers except the listed names |

Example:

```yaml
---
name: research-only
description: Read MCP tools but not internal connectors
tools: search_tool, use_tool, Read
mcpInheritance:
  except:
    - internal-tools
---
```

**Plugin agents** inherit parent MCP the same way. For security they still cannot:

- Declare their own `mcpServers` in agent frontmatter (ignored with a warning)
- Declare hooks in agent frontmatter
- Set `permissionMode: bypassPermissions`

Plugin-bundled MCP servers (plugin `.mcp.json`) still attach to the **parent/session** after the plugin is trusted — they are not a child-only frontmatter declaration. See [Plugins](09-plugins.md) and [MCP Servers](07-mcp-servers.md).

---

## Isolation: Worktree Mode

For tasks that modify files, run a subagent in an isolated git worktree with `isolation: worktree`. This keeps the child's edits from conflicting with the parent's:

- The subagent works in its own copy of the working tree.
- Its changes stay isolated from the parent until you merge them.
- The subagent's result includes the worktree path.

Grok Build manages worktrees through the `x.ai/git/worktree/*` extension methods, including an apply operation that merges changes back into the main working directory.

---

## Configuration

### Per-Type Toggles and Model Overrides

Disable specific agent types, or route them to a different model:

```toml
[subagents.toggle]
explore = true                       # default -- omit to keep enabled
plan = false                         # disable the plan subagent

[subagents.models]
explore = "grok-build"               # route explore to a specific model
```

Per-type model overrides apply for any parent. Without an override, a subagent inherits the parent's model.

### Model Selection by the Agent

The `spawn_subagent` tool offers the agent a `model` argument, and its description lists the models you can pick, for when you explicitly ask for a subagent on a different model. With `[features] subagent_model_inheritance = true` (or `GROK_SUBAGENT_MODEL_INHERITANCE=1`), both are hidden whenever every model in your picker is an xAI model: subagents then always inherit the parent's model, and a spawn that still names one fails with a message asking the agent to retry without it. Catalogs with a third-party model, a model with no declared family, or a catalog still loading keep the argument. `[subagents.models]` pins, roles, and personas are unaffected. Read when a session starts; changing it requires a restart. Precedence: a `requirements.toml`/MDM pin, then the environment variable, then `config.toml`, then remote settings, then the default (off).

You can also toggle it from `/settings` → Models → **Subagent model inheritance**:

- On: Grok cannot set models for subagents
- Off: Grok may choose a different model for a subagent. Takes effect after restart.
- NOTE: This setting only applies when all models are xAI "model_family". You likely don't need to configure this setting.

The row shows the value that applies after restart. Toggling writes `[features] subagent_model_inheritance = true` or `= false` (an explicit `false` overrides a remote `true`); `d` (reset) deletes the key so `managed_config.toml`, remote settings, or the default apply again. Agents already running keep the mode they started with. When a layer your `config.toml` cannot override decides the value — a `requirements.toml`/MDM pin, the environment variable, the `GROK_CONFIG` overlay, or an active campaign — both the toggle and the reset are refused with a toast that names that layer.

### Custom Roles and Personas

Define custom roles with their own capability and model defaults:

```toml
[subagents.roles.researcher]
description = "Deep research agent"
default_capability_mode = "read-only"
model = "grok-build"
prompt_file = ".opengrok/prompts/researcher.md"
```

Define custom personas with behavioral instructions:

```toml
[subagents.personas.concise]
instructions = "Be concise. No filler words."
# instructions_file = ".opengrok/personas/concise.md"  # or load from a file
```

Grok Build also discovers roles from `.opengrok/roles/*.toml` and personas from `.opengrok/personas/*.toml`. Inline `config.toml` definitions take precedence over files.

---

## The Tasks Pane (TUI)

Grok Build shows running and finished work in side panes on the agent screen:

- Press `Ctrl+G` to toggle the tasks pane, which lists active and completed subagents and background commands with their status.
- Press `Ctrl+T` to toggle the separate todo pane.

To view the available agent types and personas, open the command palette with `Ctrl+P` and choose **Manage Agents** (`/config-agents`).

Subagents appear at the top of the tasks pane in their own collapsible "Subagents" group.

---

## Viewing Subagents in the TUI

Subagents appear in several places in the interactive TUI:

### Scrollback (parent conversation history)

When a subagent is spawned, a compact lifecycle block is added to the *parent's* scrollback:

- `Subagent running: "do the thing" (Implementer · grok-3) — Thinking`
- Or for background subagents: `Subagent started: "..."`

While running, the block shows a live activity suffix (e.g. "Running: cargo test", "Compacting", "Retrying (2/3)") pulled from the child's turn tracker. The bullet animates (or is colored) according to state.

Press **Enter** (or Ctrl-F) on the block to open the subagent's full transcript.

For blocking subagents the single entry updates its bullet color when the child finishes. For background ones, a follow-up `Subagent completed/failed/cancelled in Xs: "..."` block is appended.

### Tasks pane (Ctrl+G)

As noted above — grouped under "Subagents", with spinners, elapsed times, and quick access to kill or inspect.

### Fullscreen framed view (the child transcript)

When you open a subagent (from a scrollback block or the tasks pane), the parent view is replaced by a bordered frame containing the child's full transcript:

- Title bar inside the frame: status icon (spinner / ✓ / ✗), label + bold description + model, optional "resumed"/"forked" badge, live activity · elapsed time, and [✗] close button.
- The child's own scrollback, thinking, and tool calls render inside the frame.
- The parent tasks pane, todos pane, dock, and catalog hide for the duration of the view.

This view is observational. The composer is hidden (zero rows). You cannot focus it, type a prompt, stash a draft, or send a follow-up from here. The parent session still owns prompts. To steer a running child, close the view and use `send_message` from the parent (see [Agent Mailboxes](#agent-mailboxes)).

**What still works**

- Scroll, fold, copy, open links, and open the block viewer on the child's transcript.
- `Ctrl+C` cancels **this child's** turn. It does not cancel the parent.
- `Ctrl+.` / `Ctrl+X` opens the shortcuts cheatsheet for the child's keys.
- The child view paints no `[Dashboard]` button. Inside the dashboard overlay the button is the way back.
- Idle `Enter` in the **block viewer** quotes the selected line into the parent composer and closes the view.

**What does nothing (fail closed)**

Root-only chords never start on this surface. They do not open a modal on the child, and they do not leak to the parent:

- Command palette (`Ctrl+P`), model picker (`Ctrl+M`), session picker (`F3`)
- Settings, extensions, always-approve (`Ctrl+O`), send-to-background (`Ctrl+B`)
- External prompt editor, Shift+Tab mode cycle

A denied action is a silent redraw. There is no toast.

If a prompt-queue overlay appears, it is a **read-only mirror**. You cannot edit, send-now, or remove rows. Queue RPCs always target the parent session.

**How to leave**

- `q` or `Esc` from bare scrollback, or click [✗].
- If scrollback search is open, `q` / `Esc` closes search first. A later press closes the view.
- `Ctrl+Q` always quits Open Grok. It is never swallowed here.

Use `q`, `Esc`, or click the close button to pop back to the parent view. The parent's scrollback continues to show the subagent's status.

---

## Depth Limits

Only the top-level session spawns subagents. A subagent cannot spawn its own subagents: the maximum nesting depth is one. If a subagent calls `spawn_subagent`, the call fails with a depth-limit error. This keeps the agent tree flat and prevents runaway spawning.

---

## When to Use Subagents

**Good use cases:**

- Researching a codebase while the parent continues other work
- Running tests in parallel while the parent implements changes
- Reviewing generated changes before you commit them
- Delegating independent tasks that do not depend on each other

**When not to use:**

- Simple tasks that the parent can handle directly
- Tasks that require tight back-and-forth with the user, since a subagent runs autonomously and isn't suited to interactive exchanges
- Tasks where the context setup cost exceeds the parallelism benefit
