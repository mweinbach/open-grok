---
name: best-of-n
description: >
  Implement a task N ways in parallel and pick the best. Spawns multiple
  subagents in isolated worktrees, evaluates all candidates, and applies the
  winner. Use when asked to "best of n", "try multiple approaches", "parallel
  implementations", "/best-of-n", or "/bon".
metadata:
  short-description: "Parallel implementation tournament"
---

# /best-of-n -- Parallel Implementation Tournament

Implement a task multiple different ways in parallel, evaluate all candidates,
and apply the best one.

## Usage

`/best-of-n [N] <task>`

- If the first token is a number 2-10, it sets the candidate count; the rest is the task.
- If omitted, N defaults to 3.

Examples:
- `/best-of-n implement the login page` (3 candidates)
- `/best-of-n 5 refactor the auth module` (5 candidates)

## Steps

1. Parse the user's message to extract **N** (candidate count, default 3) and
   the **task description**.

2. Spawn **N** subagents in a single message (parallel tool calls). Use the
   `task` tool for each with:
   - `subagent_type`: `"general-purpose"`
   - `isolation`: `"worktree"`
   - `run_in_background`: `true`
   - `description`: `"Candidate <number>"`
   - `prompt`: the task description, plus
     `"You are candidate <number> of <N> independent implementations. Implement the task fully. When done, summarize your approach and the changes you made."`

3. Wait for all candidates to complete using `get_task_output` with `block: true`
   or `wait_tasks` with `mode: "wait_all"`.

4. Evaluate and pick the winner using the criteria below.

5. Apply the winner's changes from its worktree to the main workspace.
   Review the changes in context and fix any remaining issues.

6. End your response with `WINNER: <number>` (1-N).

## Evaluation Criteria

Evaluate each candidate on these axes, in order of importance:

1. **Correctness** -- Does the candidate actually solve the task? Does it handle
   the requirements completely, or does it miss important aspects? Are there
   logic errors, type errors, or broken imports?

2. **Code Quality** -- Is the code clean, readable, and well-structured? Does it
   follow the patterns and conventions of the surrounding codebase? Does it
   avoid unnecessary complexity?

3. **Safety** -- Does the candidate avoid introducing bugs, security issues, or
   breaking changes to existing functionality?

## How to Decide

- Focus on correctness first. A candidate that fully solves the task with minor
  style issues beats one that is beautifully written but incomplete or wrong.
- If multiple candidates are equally correct, prefer the one with cleaner code
  and better codebase integration.
- If a candidate introduces unnecessary changes beyond the task scope, count
  that against it.
- If all candidates are poor, still pick the least bad one.

## Presenting Your Evaluation

Before announcing your choice, present a structured comparison:

| Dimension | Candidate 1 | Candidate 2 | ... |
|-----------|-------------|-------------|-----|
| Correctness | Short verdict | Short verdict | ... |
| Code Quality | Short verdict | Short verdict | ... |
| Safety | Short verdict | Short verdict | ... |

Then list key findings:

| Finding | Severity | Candidate 1 | Candidate 2 | ... |
|---------|----------|-------------|-------------|-----|
| Specific issue | High/Medium/Low | How handled | How handled | ... |

State which candidate you chose and why.
