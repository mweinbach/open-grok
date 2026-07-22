---
name: agent-swarm
description: >
  Use Open Grok's agent_swarm tool well — fan one prompt template over a list of
  items (or resume stalled children) as a bounded foreground cohort. Covers item
  vs resume mode, the {{item}} placeholder, validation and the 128-member cap,
  launch pacing and its env knobs, batch exclusivity, the flat subagent tree,
  per-member model overrides (including antigravity: slugs), how results and
  failures come back, and the swarm cohort card. Use when deciding between
  agent_swarm, workflow, and task, when running /swarm, or when a swarm call is
  on the table.
metadata:
  short-description: "Drive Open Grok's agent_swarm tool"
---

# Agent Swarm

`agent_swarm` launches a bounded cohort of foreground subagents from **one prompt
template fanned over a list of items** (and/or a set of resumes), then returns
their results together in input-slot order. All members run in the foreground
through the same subagent backend as `task` — it is not a separate runtime.

## When to reach for swarm vs workflow vs task

Pick by the *shape* of the work, not the count:

- **`task`** — one subagent for one job. For a *few differently shaped* jobs,
  make several ordinary `task` calls; do not force them into a swarm.
- **`agent_swarm`** — the *same kind* of task repeated over many independent
  inputs: "review each of these 12 modules for auth bugs", "summarize each file
  in this list". One template, one items array, results back together.
- **`workflow`** — scripted, multi-stage orchestration: loops, staged
  research-then-verify passes, judge panels, adversarial verification,
  loop-until-done discovery. Reach for the native Rhai `workflow` tool (see the
  `create-workflow` skill) when control flow matters. Swarm has no branching,
  no stages, no dependencies between members — every member is independent.

Swarm members are independent by construction. If members would need to hand
work to each other, or run in phases, that is a workflow, not a swarm.

## Item mode: `prompt_template` + `items`

Supply `items` (an ordered list of strings) and a `prompt_template` containing
the **literal placeholder `{{item}}`**. Each member's prompt is the template with
`{{item}}` replaced by that member's item. Partition the work into distinct,
independent scopes with no duplicate or conflicting ownership (read-only
exploration may overlap slightly).

```json
{
  "description": "auth review",
  "subagent_type": "explore",
  "prompt_template": "Review the module at {{item}} for authentication and authorization bugs. Read only; report concrete findings with file:line, or state it is clean.",
  "items": [
    "crates/api/src/auth/login.rs",
    "crates/api/src/auth/session.rs",
    "crates/api/src/auth/tokens.rs"
  ]
}
```

Required fields: `description` (shared short label for every member) and, in item
mode, `prompt_template` + `items`. `subagent_type` defaults to
`general-purpose`. Note the schema exposes **only `model`** as a per-call
override — there is no `capability_mode` or `isolation` field on `agent_swarm`;
every member inherits the capability and isolation of the chosen
`subagent_type` (e.g. pick `explore` for read-only fan-out).

## Resume mode: `resume_agent_ids`

To continue unfinished or timed-out members, pass `resume_agent_ids` — a JSON
object mapping each prior `agent_id` to the **exact continuation prompt** to
append to that child's conversation (often just `"continue"`). Keys are the
`agent_id` values from a previous swarm's result XML; values are the prompts.

```json
{
  "description": "auth review",
  "resume_agent_ids": {
    "0191f3a2-...-aaaa": "continue",
    "0191f3a2-...-bbbb": "You stopped mid-file; finish reviewing tokens.rs and report."
  }
}
```

Resume semantics that matter:

- **Ordering is observable and preserved.** The object is treated as
  insertion-ordered: resume slots launch in the order the keys appear in the
  JSON, *before* any item members. (A duplicate key keeps its original slot; the
  last value wins.)
- **Resumed members keep their prior model and profile** — the source child's
  model is pinned, so a `model` override applies to new members only and is
  silently ignored on resumes.
- You **may combine** `resume_agent_ids` with `items`, but do not duplicate
  resumed work in `items`. Resume slots run first, then items.

Prefer resume over re-running from scratch: the child keeps its full transcript,
so the continuation prompt only needs to say what changed. When a swarm result
carries a `<resume_hint>`, that is your cue to resume the unfinished members.

## Validation is fail-fast, before any child starts

The whole call is validated up front; if any check fails, **no member is
spawned**. The rules and their exact messages:

- At least **2 items** unless `resume_agent_ids` is supplied —
  *"agent_swarm requires at least 2 items unless resume_agent_ids is supplied"*.
- `items` requires `prompt_template` —
  *"prompt_template is required when items is supplied"*.
- The template must contain the literal placeholder —
  *"prompt_template must contain literal {{item}} when items is supplied"*.
- Expanded prompts must be **distinct** (no two items produce the same prompt) —
  *"prompt_template must expand to distinct prompts for each item"*.
- Resume keys must be real IDs, not empty/placeholder —
  *"resume_agent_ids must not contain empty or placeholder agent IDs"*.
- **Total members (resumes + items) are capped at 128** —
  *"agent_swarm supports at most 128 total members"*.

Two more gates reject the whole swarm before spawning: an unknown/disabled/
not-allowed `subagent_type`, and a `model` slug that is not an available slug or
whose provider has no usable credentials. **If a slug is rejected, surface the
error — do not silently re-run the swarm on a different model.**

Unless the user limits scope, decompose as finely as useful up to 128 members,
combining only genuinely inseparable work.

## Pacing and the two env knobs

The cohort does not all start at once:

- The first **5 members launch immediately** (the initial burst).
- After that, **one member launches per 700 ms** until the list is drained. The
  6th member starts ~700 ms after the burst, even if the first five already
  finished.

Two environment variables tune this (each falls back to a legacy `KIMI_*` name):

- `OPENGROK_AGENT_SWARM_MAX_CONCURRENCY` — cap on simultaneously active members.
  Must be a positive integer; unset means **no active cap** (the burst-of-5 and
  700 ms ramp still apply, so it is never a true thundering herd).
- `OPENGROK_SUBAGENT_TIMEOUT_MS` — per-member timeout in milliseconds. Unset
  defaults to **2 hours**; `0` disables the timeout. A member that times out is
  cancelled and reported as `failed` with body `subagent timed out`.

If the provider rate-limits members, the scheduler backs off (3 s, 6 s, 12 s,
24 s…) and retries the *same* child session, shrinks concurrency during repeated
limits, and recovers after a quiet period. A rate-limited member fails normally
once it is the only unfinished member, so a swarm can never hang forever.

## Batch exclusivity: one exclusive call

`agent_swarm` **must be the only tool call in its batch.** If you emit it
alongside any other tool call, every call in the batch is rejected with:

> `agent_swarm` must be the only tool call in its batch. Inspect briefly, then
> make one exclusive agent_swarm call for independent work; use ordinary task
> calls for heterogeneous small work.

So: do a small amount of exploration in earlier turns, then commit to a single,
solo `agent_swarm` call. A valid solo swarm call automatically flips the session
into swarm mode for that turn.

## Flat tree: members cannot spawn

Swarm members run at the maximum subagent depth (1). Their toolset has `task`,
`agent_swarm`, and `workflow` **stripped**, so a member cannot launch further
subagents. Design each member to do its slice and **return a complete handoff**
in its final message — do not instruct members to fan out again.

## Model overrides, including `antigravity:` slugs

`model` sets the model for every **new** member (resumes keep theirs). Any
available model slug works, including the fork's Antigravity subagent slugs of
the form **`antigravity:<model>`** (e.g. `antigravity:gemini-3.1-pro`), which run
each member through the `agy` CLI instead of an in-process child.

Antigravity slugs are gated: the **"Antigravity subagents"** setting
(`[ui].antigravity_subagents`) must be on, `agy` must be installed, and you must
be signed in to it. When the setting is off the slug is rejected with
*"Antigravity subagents are disabled. Enable the \"Antigravity subagents\"
setting…"*. Antigravity members run with full access (agy's
skip-permissions flag) by default; set `[antigravity] skip_permissions =
false` to force read-only, and members whose capability mode is pinned
read-only stay read-only either way.

## How results and failures come back

The tool returns one `<agent_swarm_result>` block, with members in **input-slot
order** (resumes first, then items):

```xml
<agent_swarm_result>
  <summary>completed=2 failed=1 aborted=0</summary>
  <resume_hint>Call agent_swarm with resume_agent_ids mapping unfinished agent_id values to continuation prompts.</resume_hint>
  <subagent agent_id="…" item="crates/api/src/auth/login.rs" outcome="completed" state="started">…report…</subagent>
  <subagent agent_id="…" item="crates/api/src/auth/tokens.rs" outcome="failed" state="started">…error text…</subagent>
</agent_swarm_result>
```

- `outcome` is `completed` (success), `failed`, or `aborted` (cancelled).
- A completed member's body is its final message; a failed/aborted member's body
  is its error text.
- The `<resume_hint>` appears only when at least one member did not complete and
  still has a usable `agent_id` — feed those IDs straight into
  `resume_agent_ids` on a follow-up call.
- Resumed members carry `mode="resume"`.

Read every member's result before reporting back: a swarm can partially succeed,
and the summary counts tell you at a glance whether a resume pass is needed.

## Swarm mode and the cohort card (TUI)

Swarm mode nudges the main agent to split suitable independent work into one
`agent_swarm` call. Control it with `/swarm`:

- `/swarm` toggles persistent manual mode; `/swarm on` / `/swarm off` set it.
- `/swarm <task>` applies swarm mode to just that one turn, then turns itself
  off (unless manual mode was already on).
- Or enable the default in **Settings → Swarm mode** / `[ui].swarm_mode = true`.

When active, the footer shows a `swarm` badge. The whole cohort renders as **one
expandable scrollback card** with a row per member, kept in input order. The card
summarizes members by status (done, failed, cancelled, running, waiting, queued)
and shows live turn/tool counts, duration, and context usage per member. Each
child's full transcript is still reachable from the tasks pane.
