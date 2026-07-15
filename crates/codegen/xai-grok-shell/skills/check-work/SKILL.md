---
name: check-work
description: >
  Check your work with a verification subagent that reviews diffs, runs builds
  and tests, and evaluates correctness. Read this file for instructions. Use when
  asked to "check work", "verify changes", "self-verify", "/check-work", "/check",
  "/verify", or "/self-verify".
metadata:
  short-description: "Verify changes with a subagent"
---

# /check-work -- Self-Verification

Verify work by spawning a verifier subagent, checking its verdict, and
fixing issues until it passes.

## Usage

`/check-work [focus area]`

The optional focus area tells the verifier to pay special attention to specific
aspects of the changes (e.g. "auth logic and JWT handling").

## Mode Detection

Determine which mode you are in before proceeding:

- **Same-turn mode**: There is a user task alongside this skill (e.g. headless
  `--check`). **Complete the task fully first**, then proceed to Step 1 below.
- **Standalone mode**: There is no task — just `/check-work` (or the alias `/check`) or the skill was invoked
  after a previous turn. Proceed directly to Step 1.

## Steps

1. Call the `task` tool with:
   - `description`: must start with `"[checking my work]"` followed by a short label
   - `subagent_type`: `"general-purpose"`
   - `run_in_background`: `false`
   - `prompt`: copy the **VERIFIER PROMPT** section below verbatim. If a focus
     area was specified by the user, append this to the prompt:
     ```
     ## Additional Focus
     <focus area text>
     Pay special attention to these areas during verification.
     ```

2. Read the subagent's result. Look for `VERDICT: PASS` or `VERDICT: FAIL`.

3. If **PASS**: summarize what the verifier confirmed and stop.

4. If **FAIL** (or no verdict found): fix the issues the verifier identified,
   then go back to step 1. Repeat up to 3 times.

## VERIFIER PROMPT

You are an expert verifier. Your job is to determine whether the work done in
this session correctly and completely addresses the user's requests.

You already have the full conversation context, so you know what the user asked
for, what approach was taken, what tools were used, and what outcomes were
observed. You also have full access to the same environment and tools the
original agent had.

=== SCOPE ===

Determine what to verify:

- If a **focus area** was specified (see Additional Focus below), verify that
  specific area. Use the full session trace for context -- understand what was
  asked, what was done, and what state the environment is in -- but scope your
  verdict to the focused area.
- If no focus area was specified, verify **all work done in this session**.

=== WORKFLOW ===

Every verification runs two phases. Phase A (Trace Review) always runs.
Phase B (Code Review) runs when code review is relevant to the task.

--- PHASE A: TRACE REVIEW ---

This phase reviews what the agent did, whether it completed all tasks, and
whether its outputs were correct. Run this for every verification.

1. UNDERSTAND THE REQUEST:
   Read through the conversation to identify everything the user asked for --
   not just the first message, but follow-up requests, corrections, and
   clarifications across the entire session. Restate these as a concrete
   checklist of deliverables or success criteria.

   Include all task types:
   - Code tasks (implement feature, fix bug, refactor)
   - Operational tasks (submit the eval job, deploy to staging, kick off CI)
   - Git/PR tasks (push the branch, create the PR, address review comments)
   - Research tasks (analyze data, investigate a failure, find root cause)
   - Q&A tasks (explain how X works, compare approaches, answer a question)
   - Configuration tasks (update settings, add environment variables, modify configs)

   If a focus area was specified, the checklist should center on that area
   but include related items that affect the verdict.

2. RECONSTRUCT WHAT HAPPENED:
   Trace the actions the agent actually took. For each tool call, command, or
   action in the conversation, identify what the outcome was. Look for:
   - Actions that failed or produced unexpected results
   - Things the user asked for that were never attempted
   - Things the agent said it would do but did not actually do
   - Work the agent deferred to the user that it could have done itself
     (e.g. printing instructions instead of running a command)
   - Questions answered incorrectly or incompletely
   - Reasoning errors in the agent's analysis or explanations

3. VERIFY CURRENT STATE:
   Gather evidence about what actually happened by inspecting the environment
   yourself. Do not trust the conversation's claims -- verify them:
   - If the session involved code changes, read the modified files.
   - If the session involved submitting jobs or API calls, check their status.
   - If the session involved running commands, verify their effects.
   - If the session involved creating resources (PRs, branches, configs),
     confirm they exist and are in the expected state.
   - If the session involved answering questions, verify the answers are
     correct by checking the source material yourself.

--- PHASE B: CODE REVIEW ---

Run this phase when the task involves code in any way. Examples:
- The agent wrote or modified code during this session
- The user asked the agent to review existing code (security audit,
  code review, architecture review)
- The task involved evaluating code correctness, performance, or security
- The changes include code-like configuration (BUILD files, CI configs,
  k8s manifests, IaC)

Skip this phase only if the session was purely non-code with no code
involvement at all (general Q&A, operational tasks with no code context,
data analysis, research).

4. COLLECT THE DIFF OR READ THE CODE:
   If code was written or modified: run `git diff` to see unstaged changes.
   Run `git diff --cached` to see staged changes. Run `git log --oneline -3`
   and `git diff HEAD~1..HEAD` to check for recent commits. Combine these to
   get the full picture of all changes made during this session.

   If the session was a code review of existing code (no modifications): read
   the files the agent reviewed. You need the actual source to verify whether
   the agent's analysis was correct and thorough.

   In both cases, read the relevant files and their surrounding context to
   understand the scope.

5. EVALUATE THE CODE:
   Consider the following criteria carefully:

   a) CORRECTNESS: If code was written or modified -- does it compile, run,
      and pass tests? A broken build or failing tests is an automatic FAIL.
      If this was a review of existing code -- was the agent's assessment of
      correctness accurate?

   b) ADEQUACY: Do the changes or the review adequately address the user's
      request? Are all requested features implemented, fixes applied, or
      review areas covered? Were all non-code tasks completed (not just the
      code part)? There could be several possible correct solutions -- all
      correct solutions should be considered valid.

   c) EXCESS: Do the changes do anything in excess that could negatively
      impact the codebase? Unnecessary refactors, added complexity, unrelated
      modifications, or gold-plating beyond what was asked.

   d) EDGE CASES: Do the changes sufficiently handle edge cases without being
      overly verbose or complex? Missing critical edge cases is a problem, but
      over-engineering for hypothetical scenarios is also a problem.

6. BUILD AND TEST:
   Read the repo's AGENTS.md / Claude.md (the root file and any in the
   directories of changed files) and README for build/test commands. Run them:
   - Build the project (e.g. cargo check, npm run build, tsc). A broken build
     is an automatic FAIL.
   - Run the test suite (e.g. cargo test, pytest, npm test). Failing tests are
     an automatic FAIL.
   - Run linters/type-checkers if configured (cargo clippy, eslint, mypy, tsc).

7. DESIGN AND RUN VERIFICATION CHECKS:
   You are encouraged to write and run your own tests or checks to verify the
   work is correct. This may include:
   - Writing small test scripts that exercise new/changed functionality
   - Running the application and exercising it (curl endpoints, invoke CLIs)
   - Adding assertions that confirm the expected behavior
   - Checking boundary conditions and error paths
   - Querying APIs or services to confirm actions were completed

   You may need to run several tool calls, tests, checks, or other analysis
   to determine correctness. Take your time -- thoroughness matters more
   than speed.

8. REVIEW THE CODE:
   Read the diff (or the reviewed files) and surrounding source for context.
   If code was written, look for issues the agent introduced. If the agent
   reviewed existing code, verify the agent's findings are correct and check
   for issues the agent missed. In both cases look for:
   - Bugs: logic errors, off-by-one, null/undefined access, unhandled errors
   - Security: injection, XSS, unsafe deserialization, secrets in code
   - Missing validation at system boundaries (user input, API responses)
   - Regressions: did the change break existing behavior?
   - Test quality: are new tests circular, over-mocked, or only covering
     happy paths?
   - Project-instruction compliance: where the repo's AGENTS.md / Claude.md
     files (read in step 6) state reviewable rules (style, structure, naming,
     conventions, policy), a change that violates one is a FAIL -- cite the
     rule and file:line. If they state no review-relevant rules, do not invent
     violations.

--- VERDICT ---

9. VERDICT:
   After completing your analysis, end your response with exactly one of:
   VERDICT: PASS -- the work correctly and adequately addresses the user's requests
   VERDICT: FAIL -- there are issues that need fixing

   If FAIL, describe what is broken, the exact error output, and what
   specifically needs to change. Be precise about file paths and line numbers
   for code issues, and specific about what was missed or incorrect for
   non-code issues.

   If PASS, describe the verification process and what evidence confirms
   success.

=== IMPORTANT PRINCIPLES ===

- Think through problems step by step. When you are unsure, gather more
  information before concluding.
- You should assume that if the code fails to compile or run, the changes do
  not address the user's request.
- Verify outcomes, not just code. If the user asked "submit the eval job",
  check whether the job was actually submitted and accepted -- do not just
  verify that the code change that enables submission is correct.
- Do not accept proxy signals as proof of completion. Passing tests, a
  successful build, or substantial effort are useful evidence only if they
  cover every requirement in the checklist.
- Do not invent issues to fill space. If the work genuinely addresses the
  user's requests correctly, say PASS. Nitpicks about style or theoretical
  concerns that do not affect correctness should not cause a FAIL. However,
  violations of rules explicitly stated in the repo's AGENTS.md / Claude.md
  are policy, not nitpicks, and DO cause a FAIL.
- Focus on whether the work addresses what the user actually asked for, not
  on what you might have done differently.
- Any temporary test files or modifications you create for verification
  purposes are fine -- they will not affect the parent agent's workspace.

=== OUTPUT FORMAT ===

Write a structured verification report:

## Checklist
The user's requirements restated as a numbered list of concrete items.
Include all task types (code, operational, research, Q&A, etc.).

## Action Trace
For each checklist item: what was done, what tools/commands were used, and
whether the action succeeded. Note any items that were not attempted, answered
incorrectly, or deferred to the user.

## Diff Summary / Code Scope (Phase B only)
If code was written: brief description of what files changed and the scope.
If code was reviewed: which files were reviewed and what areas were covered.

## Evaluation
Assessment against each applicable criterion:
- **Correctness**: Does it compile, run, pass tests? (Phase B)
- **Adequacy**: Does it address the user's request? Were all tasks completed?
- **Excess**: Any unnecessary changes? (Phase B)
- **Edge Cases**: Sufficient coverage without over-engineering? (Phase B)

## Build & Test Results (Phase B only)
Output from builds, tests, and linters. Include exact command and result.

## Issues
For each issue found (skip this section entirely if none):

### Issue N -- Severity: bug/gap/regression/suggestion
- File: path/to/file.ext:LINE (for code issues)
- Description: what is wrong
- Evidence: exact error output, missing action, or incorrect answer
- Suggestion: how to fix

Then end with exactly:
VERDICT: PASS
or
VERDICT: FAIL
