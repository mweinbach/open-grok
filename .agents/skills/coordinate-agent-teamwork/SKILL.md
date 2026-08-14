---
name: coordinate-agent-teamwork
description: "Orchestrate parallel multi-agent waves, subagent swarms, DAG task partitioning, disjoint file ownership, lock-safe verification, review pipelines, and clean handoffs. Use when breaking down large porting milestones, major refactors, multi-crate features, or comprehensive parity audits across parallel subagents."
---

# Coordinate Agent Teamwork

Instructions for planning, coordinating, and executing parallel **multi-agent waves** and **subagent swarms** in [`swift-open-grok`](https://github.com/mweinbach/swift-open-grok) (or the local directory that houses the Swift repository) and [`open-grok`](https://github.com/mweinbach/open-grok) (or the location that houses the Rust repository).

---

## 1. When to Use Agent Teamwork

Use parallel subagents when a task can be partitioned into **disjoint slices**:
- Porting large subsystems consisting of multiple independent targets/crates.
- Multi-domain parity audits (runtime, tools, TUI, permissions, auth, ACP/MCP).
- Large refactors or test suite expansions with independent target boundaries.
- Independent investigation/research tracks across different subsystems.

Do **NOT** parallelize when:
- All changes are concentrated in a single tightly coupled file (e.g. `LiveInteractiveControllerRenderer.swift`).
- The task is a small, single-file bugfix or one-line tweak.

---

## 2. The Wave Planning & Slicing Protocol

Before spawning any subagent, the **Coordinating Lead** must establish an explicit slice manifest:

### A. Slice Definition Schema
Each slice must define:
1. **`id` & `name`**: Short identifier (e.g. `slice-01-models`, `slice-02-auth`).
2. **`goal`**: Exact, bounded objective.
3. **`rustCrates`**: Source reference crates in Rust repo (`650c1db7`).
4. **`swiftTargets`**: Primary Swift targets touched.
5. **`ownedPaths`**: **Strictly disjoint list of files**. No two subagents may own or modify the same file concurrently.
6. **`dependsOn`**: Direct dependencies (DAG structure).
7. **`acceptance`**: Concrete verification criteria (e.g. specific test filters or CLI commands).

### B. Shared File Policy (The "Single Owner" Rule)
Shared integration files (e.g. `Sources/OpenGrokCLI/LiveComposition.swift`, `Package.swift`, `PORT_STATUS.md`) represent primary contention points.
- **Rule**: Assign **exactly one owner** (usually the Coordinating Lead) to shared files.
- **Workflow**: Worker subagents write their target-local code, test it, and provide the exact glue code/diff to the Coordinating Lead as text in their handoff message. The Lead integrates the glue into the shared file.

---

## 3. Subagent Roles & Responsibilities

```text
               ┌───────────────────────────────┐
               │       Coordinating Lead       │
               │  (Slices, Spawns, Commits)    │
               └──────────────┬────────────────┘
                              │
         ┌────────────────────┼────────────────────┐
         │                    │                    │
         ▼                    ▼                    ▼
┌──────────────────┐ ┌──────────────────┐ ┌──────────────────┐
│  Worker Agent 1  │ │  Worker Agent 2  │ │  Reviewer Agent  │
│ (Target A Slices)│ │ (Target B Slices)│ │ (Parity / Audit) │
└──────────────────┘ └──────────────────┘ └──────────────────┘
```

### 1. Coordinating Lead (Parent Agent)
- Writes the wave slice manifest.
- Spawns worker subagents with unambiguous prompts and explicit file ownership boundaries.
- **Exclusively manages SwiftPM / Cargo locks**: only the Lead executes full-suite runs (`build-tests`, `test --no-parallel`).
- Integrates diffs into shared files, runs integration tests, and commits logical slices.
- Updates ledgers ([`PORT_STATUS.md`](PORT_STATUS.md)).

### 2. Domain Worker Subagents
- Work strictly within their `ownedPaths`.
- Run **target-scoped compiles only** (`swift build --target <MyTarget>`).
- Land complete, compilable units: if adding an enum case, add placeholder `switch` arms in the same edit batch so the entire package graph continues to compile.
- Submit a structured completion report to the Lead with exact diffs and test results.

### 3. Reviewer / Parity Verifier Subagents
- Perform independent audits against Rust reference pin (`git show 650c1db7:...`).
- Verify adherence to the 7-stage security pipeline order.
- Guard against the "Succeeds, does nothing, says nothing" trap.

---

## 4. Concurrency & Lock Safety Rules

1. **Never Invoke Competing Full Builds**:
   - SwiftPM serializes on `workflows/swift-safe-verify.zsh` with a shared lock (`/tmp/swift-open-grok-swiftpm.lock`).
   - Workers must NEVER run full `build-tests` or `test --no-parallel` while other workers are active.
   - Workers use package-scoped commands: `swift build --target <TargetName>`.
2. **Never Use Retry Loops Around Locks**:
   - Prohibit `while true; do swift ...` or `until swift ...` loops. They starve peer workers and hold CPU.
3. **Workspace Isolation**:
   - For subagents requiring full test runs or invasive file experimentation, launch the subagent with `Workspace: 'branch'` to isolate file changes.
4. **Clean Orphaned Test Helpers**:
   - If a build/test is interrupted, kill any orphaned `swiftpm-testing-helper` processes (`pkill -fl swiftpm-testing-helper`).

---

## 5. Inter-Agent Communication & Handoff Protocol

### A. Message Hygiene & Preventing Resurrection Traps
- When sending informational updates to completed or idle subagents, **always suffix with**:
  > *"FYI only, do not act."*
  This prevents finished subagents from resurrecting into unwanted execution cycles.

### B. Worker Handoff Template
Worker subagents must report completion using this standardized template:

```markdown
### Slice Completion: <Slice-ID> (<Slice-Name>)

1. **Owned Files Modified**:
   - `Sources/TargetName/FileA.swift`
   - `Tests/TargetNameTests/FileATests.swift`

2. **Target Build & Test Evidence**:
   - Command: `swift build --target TargetName` (exit 0)
   - Tests: `14 passed in TargetNameTests`

3. **Shared File Integration / Glue Required** (if any):
   ```swift
   // Suggested registration in LiveComposition.swift:
   registry.register(MyNewTool())
   ```

4. **Invariants Preserved**:
   - Swift 6 strict concurrency (Sendable verified).
   - Files kept under ~3,000 LOC.
   - Handled CRLF and non-blocking process execution.
```

---

## 6. Execution Cycle Checklist for Wave Leads

- [ ] **Step 1: Manifest**: Write out all slices, assigned files, and acceptance tests.
- [ ] **Step 2: Spawn**: Launch worker subagents concurrently using `invoke_subagent`.
- [ ] **Step 3: Await & Review**: Review incoming completion reports. If issues are found, send focused feedback via `send_message`.
- [ ] **Step 4: Integrate**: Land worker diffs sequentially into shared files.
- [ ] **Step 5: Verify Graph**: Run `zsh workflows/swift-safe-verify.zsh build-tests`.
- [ ] **Step 6: Gate Test**: Run `zsh workflows/swift-safe-verify.zsh test --no-parallel`.
- [ ] **Step 7: Ledger & Commit**: Update `PORT_STATUS.md` with dated test counts and commit the slice.
