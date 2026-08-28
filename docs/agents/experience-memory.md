# Experience memory

Developer architecture and evaluation contract for evidence-backed, workspace-scoped experience memory. This layer extends existing Open Grok memory; it does not replace Markdown memory, retrieve complete execution trajectories, introduce a separate always-on feature, or establish new provider credentials.

For existing enablement, storage, flush/dream behavior, and goal orchestration, see [memory-and-goals.md](memory-and-goals.md).

## Baseline and gaps

The existing `xai-grok-memory` engine stores global and workspace `MEMORY.md` files plus Markdown session/flush summaries. Workspace `index.sqlite` indexes Markdown chunks using SQLite FTS5/BM25 and, when available, sqlite-vec embeddings. Retrieval merges lexical/vector scores, applies source weighting, session-age decay, an access-frequency boost, and optional maximal marginal relevance. Dream consolidation summarizes prior session files into curated workspace Markdown.

The shell searches this index before the first model sample, formats relevant snippets inside `<memory-context>`, and reuses an already-persisted block to preserve the system-prefix KV cache. `memory_search`/`memory_get` expose Markdown-backed memory, while the dedicated `experience_search` tool exposes ranked, evidence-backed experience records; compaction can recover relevant snippets.

Current gaps:

- A chunk access counter records exposure, not whether guidance was followed or improved an outcome.
- Summaries and dream output mix observations, hypotheses, successful approaches, and failures without typed evidence or outcome dimensions.
- Ranking has no structured task/repository match, measured recommendation utility, negative-reuse penalty, contradiction model, or evidence-backed confidence.
- Planning receives historical snippets rather than explicit recommendations, anti-patterns, and uncertainty; a failure has no dedicated experience-retrieval loop.
- Legacy session summaries and access-frequency boosts are not execution trajectories, evaluator verdicts, proof of success, or usefulness feedback.

Experience memory closes those gaps with an additive loop:

```text
task → retrieve scoped experience → synthesize advisory briefing → plan
     → execute → evaluate objective evidence → extract concise lessons
     → consolidate/reinforce experience → future task
```

## Data layers and source map

Keep these representations distinct:

| Layer | Meaning | Persistence/context policy |
| --- | --- | --- |
| Raw event | One tool call, exit status, test verdict, diagnostic, or feedback item | Existing session/tool records; do not copy complete output into experience memory. |
| Execution trajectory | Ordered task, decisions, tool activity, and results | Existing session conversation/events; never retrieve a whole run as routine planning context. |
| Experience | A compact, scoped account of one attempted strategy and its observed outcome | Structured workspace SQLite row with bounded evidence and source-run references. |
| Lesson | Evidence-backed reusable interpretation of one or more experiences | Stored concise text plus applicability, confidence, and provenance. |
| Recommendation | Positive action suggested by a lesson | Render under `Recommended`; advisory, never a policy override. |
| Anti-pattern | An action or failure mode supported by negative evidence | Render under `Avoid`; preserve circumstances and exceptions. |

Intended ownership:

| Concern | Source |
| --- | --- |
| Structured types, categories, evidence, dimensions, and state | `crates/codegen/xai-grok-memory/src/experience/types.rs` |
| Objective-signal classification and bounded lesson extraction | `crates/codegen/xai-grok-memory/src/experience/extraction.rs` |
| Additive SQLite storage, consolidation, provenance, and reinforcement | `crates/codegen/xai-grok-memory/src/experience/store.rs` |
| Context-aware ranking, contradiction detection, and compact briefing | `crates/codegen/xai-grok-memory/src/experience/retrieval.rs` |
| Workspace-scoped experience tool queries and safe result projection | `crates/codegen/xai-grok-memory/src/backend.rs` |
| Read-only experience-search tool, input schema, and result rendering | `crates/codegen/xai-grok-tools/src/implementations/memory/experience_search_tool.rs` |
| Deterministic comparison metrics and retrieval ablations | `crates/codegen/xai-grok-memory/src/experience/evaluation.rs` |
| Existing `<memory-context>` formatting and cache guard | `crates/codegen/xai-grok-shell/src/session/helpers/memory_context.rs` |
| First-turn advisory injection and failure-triggered replanning | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/turn.rs` |
| Activation-scoped experience identity and authenticated evidence ledger | `crates/codegen/xai-grok-shell/src/session/memory_state.rs` |
| Authenticated Code Mode nested-tool dispatch evidence | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/tool_calls.rs` |
| Dedicated goal-planner guidance and advisory contract | `crates/codegen/xai-grok-shell/src/session/{goal_planner.rs,acp_session_impl/goal_support.rs,templates/goal_planner_prompt.md}` |
| Session-end orchestration | `crates/codegen/xai-grok-shell/src/session/acp_session_impl/memory_dream.rs` |
| Conversation and authenticated-dispatch evidence extraction | `crates/codegen/xai-grok-shell/src/session/memory/hooks.rs` |

These paths describe the intended implementation boundary; the source itself, rather than this document, determines which pieces have landed.

## Structured experience schema

Logical `ExperienceMemory` fields, grouped by purpose:

```text
identity:
  id, category, status, created_at, updated_at, last_used_at,
  source_run_id/source_run_ids, superseded_by

applicability:
  task_type, task_summary, context, environment, repository_id,
  repository_revision, scope, generalizability, novelty

attempt:
  strategy, strategy_rationale, key_decisions, implementation_pattern,
  changed_paths

observed outcome:
  success, tests_run, test_results, evaluator_scores, judge_feedback,
  failure_reason, evidence_signals, outcome_dimensions

derived guidance:
  what_worked, what_failed, lesson, recommendation, anti_pattern,
  confidence, evidence_count

reuse calibration:
  retrieved_count, followed_count, successful_reuse_count,
  failed_reuse_count
```

An optional historical `usage_count` must mean exposure or be derived from `retrieved_count`; it must never be misrepresented as successful reuse. Optional/missing evaluator fields mean **unknown**, not zero, and descriptive model text is never substituted for an external verdict.

### Categories, scope, and lifecycle

| Category | Typical evidence | Conservative confidence behavior |
| --- | --- | --- |
| Successful pattern | Passing targeted checks plus acceptable quality signals | One run remains narrow/conditional; repeated independent confirmations strengthen it. |
| Failure / anti-pattern | Reproduced failing command, regression, timeout, or reviewer rejection | Strong objective failures can support a high-confidence warning within their observed scope. |
| Environmental fact | Verifiable build output, repository metadata, or generated-file behavior | Confidence reflects direct observation; dependency/revision changes can invalidate it. |
| Tool/process lesson | Observed check ordering, runtime, or diagnostics | Applies to the matching repository/toolchain unless repeatedly reproduced elsewhere. |
| Architectural lesson | Existing boundaries plus validated implementation evidence | Starts at module/repository scope; preserve architectural exceptions. |
| Uncertain hypothesis | Limited or conflicting observations | Explicitly low-confidence and advisory; never present as an established rule. |

Applicability progresses conservatively through `exact_file`, `module`, `repository`, `framework`, `task_type`, and `global`. Broader scope requires independent objective evidence from distinct source runs explicitly declared in the lesson's validated provenance; bare identifiers and undeclared evidence-only run IDs cannot establish replication. Experience creation and retrieval capture the current Git `HEAD`, when available, and a privacy-safe operating-system/architecture identifier. Git repository identity includes the normalized remote host, any explicit non-default port, and organization/repository path, preventing unrelated repositories on different hosts from sharing workspace experience. Repository/environment identifiers and revisions remain available even when a lesson is generalized.

Lifecycle states are `active`, `low_confidence`, `superseded`, `deprecated`, and `invalidated`. Superseded records retain provenance and their replacement identifier; deprecated or invalidated guidance is excluded from normal retrieval rather than silently deleted.

### Evidence and outcome dimensions

An evidence signal records its kind, observed verdict, bounded/redacted summary, optional command or check identifier, optional numeric score, timestamp, and source-run reference. Relevant kinds include command exit, compilation, tests, lint, type checks, benchmarks, runtime behavior, regression detection, code review/judge verdict, and explicit user feedback. Multiple checks from one source run are supporting detail, not independent replications; confidence and cross-repository generalization depend on distinct source runs.

The experience run ID is scoped to one session-actor activation, not to the stable session ID used for persisted conversation and resume. Retrieval attribution, extracted evidence, followed recommendations, and finalization must all use the same activation-scoped ID. Resuming a persisted session into a newly spawned actor creates a fresh experience run, so its new checks can independently reinforce prior lessons instead of being rejected by the previous activation's finalized-run tombstone; reattaching to an actor that is still running preserves its current run ID. A bounded workspace-local run-to-session mapping makes newly recorded activation references traceable to the stable persisted session without conflating either identity. Older records without a mapping retain their run references but do not invent session provenance.

Keep outcome dimensions separate:

```text
functional_correctness   completeness          code_quality
maintainability          architectural_fit     efficiency
regression_risk          test_coverage         judge_score
user_preference
```

Normalize known scores before comparison, but preserve their individual values. A passing check does not erase a regression, poor review, failing lint, or bad architectural fit. A failed run can still yield a useful partial recommendation or a strongly supported anti-pattern. Surface material disagreements rather than rewarding the easiest measurable signal.

## Storage, migration, and enablement

```text
$OPENGROK_HOME/memory/
  ├── MEMORY.md
  └── {project-slug}-{hash8}/
        ├── MEMORY.md
        ├── index.sqlite
        │     ├── existing meta/chunks/chunks_fts[/chunks_vec]
        │     └── additive experiences, lexical index, reuse, source provenance,
        │         bounded run-to-session references, and finalized-run tombstones
        └── sessions/*.md
```

Use the **same workspace-scoped `index.sqlite`** and existing filesystem/journal policy. Create experience, lexical-search, reuse-attribution, compact source-provenance, bounded run-to-session references, and bounded finalized-run tables additively and idempotently (`CREATE ... IF NOT EXISTS`); do not rebuild or reinterpret legacy chunks, require embeddings, create a second database, migrate Markdown eagerly, or cross repository boundaries. Legacy hostless workspace directories are migrated only when their recorded workspace ownership and current origin prove the same host-qualified repository; ambiguous legacy directories remain untouched rather than risking disclosure. Existing installations with no experience rows retain the legacy retrieval path unchanged. Missing optional fields deserialize with conservative defaults, and unrecognized evidence is never considered objective, so future schema evolution is non-destructive and fails safely.

Experience collection and retrieval inherit the existing memory enablement/configuration and workspace-storage rules: memory is off by default; `--no-memory`, `--experimental-memory`, `GROK_MEMORY`, `[memory] enabled`, session `/memory on|off`, and ephemeral-workspace protections retain their existing meaning. Respect session retention/save policy; do not introduce a separate default-on subsystem or write under `~/.grok`.

Workspace garbage collection must treat an SQLite index containing experience or indexed memory rows as durable content even when no Markdown session summaries exist; experience-only workspaces are not disposable empty directories, including legitimate repositories whose names begin with `tmp`.

### Extraction and privacy

Derive lessons from observable tool/conversation evidence and available evaluator outputs, not unsupported assistant self-reflection:

1. Identify the real task, strategy/decisions when observable, repository context, and source run.
2. Classify actual exit codes, test summaries, compiler/lint/type diagnostics, regressions, benchmark results, review/judge results, and explicit user feedback.
3. Retain objective successes and failures separately; do not infer overall task success solely from a normal assistant reply or session shutdown.
4. Emit only concise actionable patterns supported by the observed signals; keep weak or conflicting interpretations as hypotheses.
5. Start at the narrowest defensible scope, link all source runs, and bound both record count and evidence text.

Programmable Code Mode `exec` output is not authenticated nested-tool execution evidence: a model-written JavaScript cell can fabricate arbitrary command names, exit-code JSON, and passing test summaries with `text()`. Nested execution counts only when the real shell dispatch independently records the prepared tool identity, arguments, and terminal result in its activation-scoped evidence ledger. Session-end extraction combines those authenticated dispatch records with correlated, directly invoked execution results; it never trusts programmable `exec` output or appends nested results to the model conversation. This lets Code Mode Only models learn from real nested checks without weakening the history sink or accepting fabricated JavaScript output. Successful wrapper transport, ordinary `git diff`, file contents, and assistant prose still never prove functional correctness.

Store redacted summaries and safe command/check identifiers, never complete transcripts, terminal dumps, authorization headers, bearer tokens, API keys, AWS access/secret/session credentials, cookies, passwords, sensitive environment values, or opaque provider history. Redact before persistence and again before prompt rendering; avoid embedding secrets inside deduplication keys or telemetry. Preserve provider/export isolation and fail open without blocking session shutdown if extraction or persistence fails.

## Outcome-aware retrieval

Filter by active/eligible status and applicability before ranking. Exact-file/module/repository lessons must match their corresponding context; repository revision and environment mismatches reduce confidence or expose an exception instead of silently exporting repository-local advice. Candidate matching is lexical over compact task/lesson/strategy/failure text; legacy Markdown semantic/vector search remains a separate, compatible path.

An explicit initial calibration for normalized experience signals is:

```text
reuse_posterior = (successful_reuse_count + 1)
                  / (successful_reuse_count + failed_reuse_count + 2)

base_score = 0.35 × lexical_task_relevance
           + 0.20 × repository/task/environment_context_match
           + 0.15 × polarity_aware_outcome_and_evidence_quality
           + 0.15 × Bayesian_reuse_posterior
           + 0.10 × calibrated_confidence
           + 0.05 × justified_generalizability

experience_score = base_score × freshness_decay × lifecycle_modifier
```

The source implementation and tests are authoritative for exact constants; these weights are the initial design calibration, not a claim of experimentally optimized coefficients. Require meaningful lexical/context overlap before scoring, so an otherwise confident unrelated lesson cannot dominate. Neutral Bayesian priors prevent one reuse from becoming certainty. The outcome component is **polarity-aware**: a well-evidenced failed strategy can rank highly as a warning without being mislabeled a successful solution. Recent contradictory evidence, failed reuse, revision changes, and poor quality reduce influence; stronger independent evidence offsets ordinary age decay.

### Explicit experience search and references

`experience_search` is a dedicated read-only model tool for inspecting structured lessons; it does not change the Markdown-backed `memory_search`/`memory_get` contract. Its required `query` searches ranked experience for the current workspace and repository. Optional `max_results` uses the configured memory-search default when omitted and is capped at 20. An optional `outcome` accepts only `"success"` or `"failure"`; omitting it searches both:

```json
{
  "query": "authentication middleware integration test",
  "max_results": 5,
  "outcome": "failure"
}
```

The same tool also dereferences safe exact references: `{"query":"experience:abc123"}` loads that lesson, `{"query":"run:019abc"}` retrieves lessons supported by that activation, and `{"query":"session:019def"}` retrieves lessons from runs verifiably mapped to that stable session. Older records without run-to-session mappings remain searchable by run reference but cannot be resolved by session reference. Direct lookup preserves workspace/repository isolation, lifecycle and outcome filtering, response bounds, and evidence redaction; malformed references do not become unrestricted queries.

Results expose concise task/lesson/strategy context, outcome and confidence, what worked or failed, verification commands, failure reasons, and bounded objective evidence where available. Each result includes its stable `experience:<id>` reference and source `run:<id>` references; `session:<id>` references appear only when the corresponding activation has a verified workspace-local session mapping. Resumed activations can therefore link to the same stable session while remaining independent experience runs. Historical unmapped runs are not guessed or matched across workspaces. All three references are queryable through `experience_search`; a resolvable session can additionally be reopened through existing session-resume flows, such as `open-grok --resume <id>`. Experience and run references are not standalone resume commands.

Search returns bounded, redacted details rather than complete transcripts, provider history, raw terminal output, or credentials. It inherits the existing memory enablement, permission, and workspace-isolation rules; disabling memory removes the tool. Memory-enabled subagents may search inherited workspace experience, but only root sessions persist lessons and run-to-session mappings. Code Mode Only exposes the same registered tool through nested `tools.experience_search(...)`, not as a special top-level transport or a second memory backend. Availability requires running an Open Grok binary that includes this implementation.

### Briefings and contradictions

Render only a small, bounded briefing, generally 3–10 distinct lessons:

```text
Relevant prior experience (advisory; verify against current evidence)

Recommended:
- Extend the existing registry. Evidence: 4 successful related runs; high confidence.

Avoid:
- Editing generated schema files directly. Evidence: overwritten in 3 runs; high confidence.

Uncertain:
- Batched writes may reduce latency. Evidence: 1 benchmark; low confidence.

Contradictions:
- Batching helped small workloads but timed out on large workloads; inspect workload and revision.
```

Compare overlapping positive and negative lessons using scope, repository revision, environment/workload, task type, timestamps, evidence strength, and reuse history. Preserve compatible exceptions and surface unresolved contradictions explicitly; never merge opposite conclusions, select randomly, or present memory as permission/policy instructions.

## Planning, failures, and reinforcement

The first task turn retrieves experience before the model's first planning/sample step. Merge compact guidance into the existing first-turn memory briefing without displacing legacy relevant snippets. Dedicated `/goal` planning also receives an objective-specific briefing before its planner subagent writes a plan. Reuse `conversation_has_memory_context`: once the leading system-memory block exists or survives resume, do **not** re-search and rewrite it.

When an observed tool failure is classifiable and correlated with a trusted direct execution call, use its diagnostic as a scoped additional query. Programmable `exec`/`wait` output and unmatched tool-result text cannot trigger an authoritative failure reminder. If useful prior anti-patterns exist, append a bounded **trailing** advisory/system reminder through the existing turn-loop mechanism and replan; do not mutate the immutable cached system prefix, inject raw failure trajectories, or repeat the same warning indefinitely. Current objective evidence can always override older memory.

Reuse accounting has distinct transitions:

1. Increment `retrieved_count` when a lesson is actually included in a rendered advisory.
2. Increment `followed_count` only when following the recommendation/warning is observable or explicitly attributed; retrieval alone is insufficient.
3. Increment `successful_reuse_count` or `failed_reuse_count` only for a followed lesson with a later objective outcome in an applicable context.
4. Recalculate confidence/usefulness conservatively; repeated failures narrow scope, lower confidence, or supersede/deprecate outdated guidance.

A coincidental successful session does not prove that every retrieved memory caused the outcome. If adherence or attribution cannot be observed, retain exposure telemetry without inventing follow/reuse evidence.

Bounded finalized-run tombstones preserve attribution idempotency after detailed reuse rows expire: replaying the same completed run cannot repeatedly increase confidence.

### Consolidation, scope, and forgetting

Merge normalized near-identical lessons only when category/polarity, applicability, repository context, and important exceptions agree. Consolidation increments independent evidence counts, deduplicates source-run identifiers, preserves the strongest objective signals and outcome dimensions, updates confidence, and keeps bounded record/source lists. Never count repeated extraction of the same run as independent evidence.

Keep contradictory lessons, materially different environments, and narrowly scoped exceptions separate. Promote scope only after reproducible evidence across that broader scope. Apply temporal/revision decay, demote repeatedly contradicted guidance, and mark superseded/deprecated/invalidated rows without erasing their traceable source history. This consolidation is distinct from model-driven Markdown dream consolidation; neither path should corrupt the other.

## Deterministic evaluation and ablations

Use fixed clocks, deterministic repository/task fixtures, explicit objective outcomes, and matched workloads to exercise extraction, ranking, warnings, contradiction handling, consolidation, decay, reinforcement, prompt bounds, and migration. Do not call an evaluator score “ground truth” when independent signals disagree.

Track these dimensions separately for baseline and experience-enabled runs:

```text
task_success_rate          test_pass_rate             judge_score
code_quality_score         retries                    repeated_failure_rate
tokens_consumed            wall_clock_execution       memory_context_tokens
regression_rate            repeated_failure_avoidance recommendation_utility
```

Definitions:

```text
repeated_failure_avoidance_rate = known_prior_failure_opportunities_avoided
                                  / applicable_known_prior_failure_opportunities

recommendation_utility = outcome_rate_with_followed_recommendation
                         - matched_outcome_rate_without_recommendation

exposure_utility = outcome_rate_with_retrieved_recommendation
                   - matched_outcome_rate_without_recommendation
```

Define an avoidance opportunity only when a prior anti-pattern was relevant, available before the action, and comparable to the current task; a zero-opportunity denominator is unknown, not perfect avoidance, and `failure_avoidance_available` explicitly marks this condition. Track retrieval/exposure associations separately from followed-recommendation utility. Match utility comparisons by repository, revision, task type, baseline difficulty, environment, and available evaluation signals; report sample sizes and uncertainty. Normalize valid judge scores from either 0–1 or 0–100 scales before aggregation and utility calculations, exclude invalid or out-of-range quality scores, and preserve missing dimensions as unknown. Observational utility indicates association unless the evaluation randomizes or otherwise controls exposure.

Run five matched ablations:

1. Existing semantic/lexical Markdown retrieval only.
2. Semantic retrieval plus outcome weighting.
3. Semantic/outcome retrieval plus positive recommendations.
4. Positive recommendations plus negative anti-patterns.
5. Full experience-weighted retrieval with scope, uncertainty, contradictions, reuse calibration, consolidation, and decay.

Deterministic unit/fixture coverage verifies mechanics; it does **not** establish improved live-agent task success, lower token use, or causal recommendation benefit. Report real comparisons only after representative runs with recorded denominators, outcomes, token costs, and regressions. No production benchmark or efficacy claim is implied by this architecture document.

### Focused validation

```sh
# Default non-linking checks for the affected packages.
cargo check --locked -p xai-grok-memory
cargo check --locked -p xai-grok-shell

# Run behavior tests only when their linked artifacts/results are needed.
OPENGROK_HOME="$(mktemp -d)" cargo test --locked -p xai-grok-memory --lib experience
OPENGROK_HOME="$(mktemp -d)" cargo test --locked -p xai-grok-shell --lib experience

# Review only the owned documentation paths in a shared checkout.
git diff --check -- docs/agents/experience-memory.md docs/agents/memory-and-goals.md
```

Keep runtime tests isolated from the real `~/.opengrok`, use package-scoped checks by default, and do not format or stage unrelated concurrent edits.
