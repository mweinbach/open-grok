pub(crate) mod host_service;
pub(crate) mod listing;
pub(crate) mod manager;
pub(crate) mod notify;
pub(crate) mod registry;
pub(crate) mod schema_contract;
pub(crate) mod store;
pub(crate) mod tracker;

#[cfg(test)]
mod builtin_tests {
    #[test]
    fn every_builtin_validates_and_matches_its_registered_name() {
        for builtin in super::registry::BUILTIN_WORKFLOWS {
            let meta = xai_workflow::extract_meta(builtin.script)
                .unwrap_or_else(|e| panic!("builtin '{}' must validate: {e}", builtin.name));
            assert_eq!(
                meta.name, builtin.name,
                "registry key must equal meta.name for '{}'",
                builtin.name
            );
        }
    }

    #[test]
    fn every_builtin_passes_a_canned_host_dry_run() {
        for builtin in super::registry::BUILTIN_WORKFLOWS {
            let report = xai_workflow::validate_script(builtin.script, None)
                .unwrap_or_else(|e| panic!("builtin '{}' failed its dry run: {e}", builtin.name));
            assert!(
                report.outcome_ok,
                "builtin '{}' dry run ended badly: {}",
                builtin.name, report.outcome_summary
            );
        }
    }

    #[test]
    fn ultracode_escalates_blockers_and_keeps_verification_bounded() {
        let script = super::registry::BUILTIN_WORKFLOWS
            .iter()
            .find(|builtin| builtin.name == "ultracode")
            .map(|builtin| builtin.script)
            .expect("ultracode builtin registered");
        // Blocked phases hand the issue back to the resuming model instead of
        // dying: escalate() carries the blocker and the resume_note is used.
        assert!(script.contains("escalate(\"ultracode is blocked in the Understand phase"));
        assert!(script.contains("escalate(\"ultracode is blocked in the Implement phase."));
        assert!(script.contains("escalate(\"ultracode cannot converge in the Verify phase"));
        // Agent-authored text inside escalate messages stays fenced.
        assert!(script.contains("json_encode(blocker)"));
        assert!(script.contains("json_encode(hard_failure)"));
        assert!(script.contains("json_encode(issue.title)"));
        assert!(script.contains("resume_note"));
        // A missing task escalates (args are immutable, a plain pause would
        // dead-end) and an all-unusable review round keeps known blockers.
        assert!(script.contains("escalate(\"ultracode has no task"));
        assert!(script.contains("blocking_issues = prev_blocking;"));
        // Adversarial verification is bounded and lens-diverse.
        assert!(script.contains("while round <= max_fix_rounds + 1"));
        assert!(script.contains("CORRECTNESS lens"));
        assert!(script.contains("BLAST-RADIUS lens"));
        assert!(script.contains("capability_mode: \"execute\""));
        // Implementer and fixers are the only writers.
        assert!(script.contains("capability_mode: \"all\""));
        assert!(!script.contains("isolation_worktree"));
        // Deterministic delivery contract.
        assert!(script.contains("status: status"));
        assert!(script.contains("blocking_issues: blocking_issues"));
        assert!(script.contains("write_scratch_file(\"report.md\", report)"));
    }

    #[test]
    fn deep_research_binds_shards_and_renders_verified_claims() {
        let script = super::registry::BUILTIN_WORKFLOWS
            .iter()
            .find(|builtin| builtin.name == "deep-research")
            .map(|builtin| builtin.script)
            .expect("deep-research builtin registered");
        assert!(script.contains("expected_ids[shard_idx]"));
        assert!(script.contains("verification_results[assigned_shard]"));
        assert!(script.contains("verified_claim_ids"));
        assert!(script.contains("**Status: Partial**"));
        assert!(!script.contains("label: \"research-reporter\""));
        assert!(script.contains("label: \"report-synthesizer\""));
        assert!(script.contains("<report-body>"));
        assert!(!script.contains("output_schema: synthesis_schema"));
        assert!(script.contains("failed citation validation"));
        assert!(script.contains("let findings_fallback"));
        assert!(script.contains("full_report += \"\\n## Sources\\n\""));
        assert!(script.contains("report: chat_report"));
        assert!(!script.contains("chat_report += \"\\n## Sources\\n\""));
    }
}
