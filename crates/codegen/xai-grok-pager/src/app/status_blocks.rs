//! Read-only system-block text for `/queue`, `/tasks`, and `/usage`.
//!
//! Plain text committed into scrollback — the primary inspection surface in
//! minimal mode (no interactive panes). Kept out of `dispatch` for easy
//! unit tests.

use crate::app::agent::BgTaskStatus;
use crate::app::agent_view::AgentView;
use crate::app::subagent::format_subagent_label;
use crate::util::{format_duration, group_thousands};

/// `/queue` body — a read-only list of the queued prompts.
///
/// Server-authoritative shared-queue rows (the in-flight prompt excluded) come
/// first in broadcast order, then the local drip-feed queue — matching
/// [`crate::views::queue_pane::QueuePane::sync_from_merged`]'s ordering.
pub(crate) fn queue_block_text(agent: &AgentView) -> String {
    let running_id = agent.session.current_prompt_id.as_deref();

    let mut rows: Vec<String> = Vec::new();
    let mut pos = 1usize;
    for wire in &agent.shared_queue {
        if running_id == Some(wire.id.as_str()) {
            continue;
        }
        rows.push(format_queue_row(pos, &wire.text));
        pos += 1;
    }
    for prompt in &agent.session.pending_prompts {
        rows.push(format_queue_row(pos, &prompt.text));
        pos += 1;
    }

    if rows.is_empty() {
        "Queue is empty.".to_string()
    } else {
        let header = format!(
            "Queued prompt{} ({}):",
            if rows.len() == 1 { "" } else { "s" },
            rows.len()
        );
        join_header_rows(header, rows)
    }
}

///
/// [`crate::views::tasks_pane::TasksPane`] without its styled rows.
pub(crate) fn tasks_block_text(agent: &AgentView) -> String {
    let mut rows: Vec<String> = Vec::new();

    let mut workflows: Vec<_> = agent.workflow_runs.iter().collect();
    workflows.sort_by(|a, b| {
        b.is_active()
            .cmp(&a.is_active())
            .then(b.received_at.cmp(&a.received_at))
            .then(a.run_id.cmp(&b.run_id))
    });
    for run in workflows {
        let active = run.active_agent_count();
        let agents = match active {
            0 => String::new(),
            1 => " · 1 agent".to_string(),
            n => format!(" · {n} agents"),
        };
        let phase = run
            .current_phase
            .as_deref()
            .map(str::trim)
            .filter(|phase| !phase.is_empty())
            .map(|phase| format!(" · {phase}"))
            .unwrap_or_default();
        rows.push(format!(
            "  {:<9}Workflow · {}{phase}{agents}  ({})",
            if run.is_active() {
                "running".to_string()
            } else {
                run.status.replace('_', " ")
            },
            run.name,
            format_duration(std::time::Duration::from_millis(run.live_elapsed_ms()))
        ));
    }

    // ── Subagents ──
    let mut subs: Vec<_> = agent
        .subagent_sessions
        .values()
        .filter(|s| s.workflow_run_id.is_none())
        .collect();
    subs.sort_by(|a, b| {
        b.is_running()
            .cmp(&a.is_running())
            .then(b.started_at.cmp(&a.started_at))
            .then(a.child_session_id.cmp(&b.child_session_id))
    });
    for info in subs {
        let (type_label, desc) = format_subagent_label(info);
        let status = if info.pending_kill {
            "stopping"
        } else if info.is_running() {
            "running"
        } else {
            info.status.as_deref().unwrap_or("done")
        };
        let label = if desc.is_empty() {
            type_label
        } else {
            format!("{type_label} · {desc}")
        };
        rows.push(format!(
            "  {status:<9}{label}  ({})",
            format_duration(info.display_elapsed())
        ));
    }

    // ── Background tasks / monitors ──
    let mut tasks: Vec<_> = agent.session.bg_tasks.values().collect();
    tasks.sort_by(|a, b| {
        let (ar, br) = (
            a.status == BgTaskStatus::Running,
            b.status == BgTaskStatus::Running,
        );
        br.cmp(&ar)
            .then(b.start_time.cmp(&a.start_time))
            .then(a.task_id.cmp(&b.task_id))
    });
    for task in tasks {
        let kind = if task.is_monitor { "Monitor" } else { "Task" };
        let one_line = task
            .description
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| first_nonempty_line(&task.command));
        let status = if task.pending_kill {
            "stopping"
        } else {
            match task.status {
                BgTaskStatus::Running => "running",
                BgTaskStatus::Done => "done",
                BgTaskStatus::Failed => "failed",
            }
        };
        rows.push(format!(
            "  {status:<9}{kind} · {one_line}  ({})",
            format_duration(task.elapsed())
        ));
    }

    // ── Scheduled (/loop) tasks ──
    let mut sched: Vec<_> = agent.session.scheduled_tasks.values().collect();
    sched.sort_by(|a, b| {
        a.tag
            .cmp(&b.tag)
            .then(a.human_schedule.cmp(&b.human_schedule))
            .then(a.task_id.cmp(&b.task_id))
    });
    for info in sched {
        rows.push(format!(
            "  {:<9}{} · {} · {}",
            "scheduled",
            info.tag,
            info.human_schedule,
            first_nonempty_line(&info.prompt)
        ));
    }

    if rows.is_empty() {
        "No background tasks, workflows, or subagents.".to_string()
    } else {
        let header = format!(
            "Task{} ({}):",
            if rows.len() == 1 { "" } else { "s" },
            rows.len()
        );
        join_header_rows(header, rows)
    }
}

/// `/usage` body — per-session token and cost totals, scoped to the ledger's
/// lifetime: since session start, or since the last `/resume`.
pub(crate) fn session_usage_block_text(
    usage: &xai_grok_shell::extensions::notification::PromptUsage,
) -> String {
    let t = &usage.totals;
    if t.model_calls == 0 && usage.model_usage.is_empty() {
        return if usage.usage_is_incomplete {
            "Session usage: none recorded, but tracking is incomplete and may under-count."
                .to_string()
        } else {
            "Session usage: no model calls yet in this session.".to_string()
        };
    }

    let mut rows = Vec::new();
    let cache_suffix = if t.input_tokens > 0 && t.cached_read_tokens > 0 {
        format!(
            " ({} cached · {:.1}% hit rate)",
            group_thousands(t.cached_read_tokens),
            t.cache_hit_rate_pct()
        )
    } else {
        format!(" ({} cached)", group_thousands(t.cached_read_tokens))
    };
    rows.push(format!(
        "  Input tokens:   {}{}",
        group_thousands(t.input_tokens),
        cache_suffix,
    ));
    rows.push(format!(
        "  Output tokens:  {} ({} reasoning)",
        group_thousands(t.output_tokens),
        group_thousands(t.reasoning_tokens),
    ));
    rows.push(format!(
        "  Total tokens:   {}",
        group_thousands(t.total_tokens)
    ));
    rows.push(format!(
        "  Model calls:    {} · API time: {}",
        group_thousands(t.model_calls),
        format_duration(std::time::Duration::from_millis(t.api_duration_ms)),
    ));
    rows.push(format!("  Cost:           {}", format_cost(t)));

    if usage.model_usage.len() > 1 {
        rows.push("  By model:".to_string());
        for (model, m) in &usage.model_usage {
            let cache_str = if m.input_tokens > 0 && m.cached_read_tokens > 0 {
                format!(" · {:.1}% cache hit", m.cache_hit_rate_pct())
            } else {
                String::new()
            };
            rows.push(format!(
                "    {model} — {} in / {} out{cache_str} · {}",
                group_thousands(m.input_tokens),
                group_thousands(m.output_tokens),
                format_cost(m),
            ));
        }
    }

    if usage.usage_is_incomplete {
        rows.push("  Note: usage is incomplete and may under-count.".to_string());
    }

    join_header_rows(
        "Session usage (since start or last resume):".to_string(),
        rows,
    )
}

/// `/cache` body — prompt cache hit rates, break diagnostics, and recent turn history.
pub(crate) fn session_cache_block_text(
    cache: &xai_grok_shell::extensions::cache::SessionCacheResponse,
) -> String {
    let s = &cache.summary;
    if s.total_turns == 0 {
        return "Prompt cache telemetry: no turns recorded yet in this session.".to_string();
    }

    let mut rows = Vec::new();
    if s.steady_input_tokens > 0 {
        rows.push(format!(
            "  Cache hit rate: {:.1}% ({} of {} steady-state input tokens cached; cold start excluded)",
            s.overall_hit_rate_pct,
            group_thousands(s.steady_cached_tokens),
            group_thousands(s.steady_input_tokens),
        ));
    } else {
        rows.push("  Cache hit rate: n/a (cold-start request only so far)".to_string());
    }
    rows.push(format!(
        "  Turns tracked:  {} ({} hits · {} partial · {} breaks)",
        s.total_turns, s.hits, s.partial_hits, s.breaks,
    ));

    if let Some(ref last_break) = s.last_break_diagnostic {
        rows.push(format!("  Last break:     {last_break}"));
    }

    if !cache.recent_turns.is_empty() {
        rows.push("  Recent turns:".to_string());
        for rec in cache.recent_turns.iter().rev().take(10) {
            if rec.status == xai_grok_shell::session::CacheStatus::FirstTurn {
                // Cold start: no hit-rate percentage — the first request cannot
                // hit the cache, so a percentage here reads like a failure.
                rows.push(format!(
                    "    Turn #{} (loop {}) — cold start ({} in) · {}",
                    rec.turn_idx,
                    rec.loop_index,
                    group_thousands(rec.prompt_tokens as u64),
                    rec.diagnostic,
                ));
            } else {
                rows.push(format!(
                    "    Turn #{} (loop {}) — {:.1}% hit ({} in, {} cached) · {}",
                    rec.turn_idx,
                    rec.loop_index,
                    rec.cache_hit_rate_pct,
                    group_thousands(rec.prompt_tokens as u64),
                    group_thousands(rec.cached_prompt_tokens as u64),
                    rec.diagnostic,
                ));
            }
        }
    }

    join_header_rows(
        "Prompt Cache Telemetry & Diagnostics:".to_string(),
        rows,
    )
}

/// Cost cell. Ticks are 1e10 per USD; partial sums are scrubbed to absent.
fn format_cost(m: &xai_grok_shell::extensions::notification::PromptUsageModel) -> String {
    use xai_grok_shell::extensions::notification::ticks_to_usd;
    match m.cost_usd_ticks {
        Some(ticks) => format!("${:.4}", ticks_to_usd(ticks)),
        None if m.cost_is_partial => "not available (not reported for some calls)".to_string(),
        None => "not available (not reported)".to_string(),
    }
}

/// First non-empty, trimmed line of `text` (empty string if none). Collapses a
/// multi-line prompt/command to a single display line.
fn first_nonempty_line(text: &str) -> &str {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("")
}

/// Format one `/queue` row as `  #N  <first non-empty line>` with a
/// `(+K more lines)` suffix for multi-line prompts.
fn format_queue_row(pos: usize, text: &str) -> String {
    let first_line = first_nonempty_line(text);
    let extra = text.lines().count().saturating_sub(1);
    if extra > 0 {
        format!(
            "  #{pos}  {first_line}  (+{extra} more line{})",
            if extra == 1 { "" } else { "s" }
        )
    } else {
        format!("  #{pos}  {first_line}")
    }
}

/// Join a header line above its rows into a single block string.
fn join_header_rows(header: String, rows: Vec<String>) -> String {
    std::iter::once(header)
        .chain(rows)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_grok_shell::extensions::notification::{PromptUsage, PromptUsageModel};

    fn model_row(input: u64, output: u64, ticks: Option<i64>) -> PromptUsageModel {
        PromptUsageModel {
            input_tokens: input,
            output_tokens: output,
            total_tokens: input + output,
            cached_read_tokens: 0,
            cache_creation_tokens: 0,
            reasoning_tokens: 0,
            model_calls: 1,
            api_duration_ms: 1_000,
            cost_usd_ticks: ticks,
            cost_is_partial: false,
            cost_missing_calls: 0,
        }
    }

    #[test]
    fn session_usage_block_empty_ledger() {
        let usage = PromptUsage::default();
        assert_eq!(
            session_usage_block_text(&usage),
            "Session usage: no model calls yet in this session."
        );

        // Empty but incomplete must not read as a clean zero.
        let incomplete = PromptUsage {
            usage_is_incomplete: true,
            ..Default::default()
        };
        assert!(session_usage_block_text(&incomplete).contains("incomplete"));
    }

    #[test]
    fn session_usage_block_formats_tokens_and_cost() {
        let mut totals = model_row(1_234_567, 45_678, Some(12_345_000_000));
        totals.cached_read_tokens = 1_000_000;
        totals.reasoning_tokens = 12_000;
        totals.model_calls = 42;
        totals.api_duration_ms = 192_000;
        let usage = PromptUsage {
            totals,
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        // Snapshot pins content and column alignment together; single-model
        // sessions must skip the redundant by-model breakdown.
        insta::assert_snapshot!("session_usage_block_full", text);
    }

    #[test]
    fn session_usage_block_lists_models_when_multiple() {
        let mut usage = PromptUsage {
            totals: model_row(150, 15, None),
            ..Default::default()
        };
        usage
            .model_usage
            .insert("grok-build".into(), model_row(100, 10, None));
        usage
            .model_usage
            .insert("grok-4".into(), model_row(50, 5, None));
        let text = session_usage_block_text(&usage);
        assert!(text.contains("By model:"), "{text}");
        assert!(text.contains("grok-build — 100 in / 10 out"), "{text}");
        assert!(text.contains("grok-4 — 50 in / 5 out"), "{text}");
    }

    #[test]
    fn session_usage_block_absent_cost_is_unknown_not_free() {
        let usage = PromptUsage {
            totals: model_row(100, 10, None),
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        insta::assert_snapshot!("session_usage_block_absent_cost", text);
        // Unknown cost must never read as free.
        assert!(!text.contains("$0"), "{text}");
    }

    #[test]
    fn session_usage_block_flags_partial_and_incomplete() {
        let mut totals = model_row(100, 10, None);
        totals.cost_is_partial = true;
        let usage = PromptUsage {
            totals,
            usage_is_incomplete: true,
            ..Default::default()
        };
        let text = session_usage_block_text(&usage);
        assert!(text.contains("not reported for some calls"), "{text}");
        assert!(text.contains("usage is incomplete"), "{text}");
    }

    #[test]
    fn group_thousands_groups_digits() {
        assert_eq!(group_thousands(0), "0");
        assert_eq!(group_thousands(999), "999");
        assert_eq!(group_thousands(1_000), "1,000");
        assert_eq!(group_thousands(1_234_567), "1,234,567");
    }

    #[test]
    fn first_nonempty_line_skips_blank_leading_lines() {
        assert_eq!(first_nonempty_line("\n  \n  hello \nworld"), "hello");
        assert_eq!(first_nonempty_line("   "), "");
        assert_eq!(first_nonempty_line(""), "");
        assert_eq!(first_nonempty_line("only"), "only");
    }

    #[test]
    fn format_queue_row_single_line() {
        assert_eq!(format_queue_row(1, "fix the bug"), "  #1  fix the bug");
    }

    #[test]
    fn format_queue_row_multiline_reports_extra_lines() {
        assert_eq!(
            format_queue_row(2, "first\nsecond"),
            "  #2  first  (+1 more line)"
        );
        assert_eq!(
            format_queue_row(3, "first\nsecond\nthird"),
            "  #3  first  (+2 more lines)"
        );
    }

    #[test]
    fn session_cache_block_empty() {
        let resp = xai_grok_shell::extensions::cache::SessionCacheResponse {
            summary: Default::default(),
            recent_turns: vec![],
        };
        assert_eq!(
            session_cache_block_text(&resp),
            "Prompt cache telemetry: no turns recorded yet in this session."
        );
    }

    #[test]
    fn session_cache_block_formats_summary_and_turns() {
        let resp = xai_grok_shell::extensions::cache::SessionCacheResponse {
            summary: xai_grok_shell::session::CacheSummary {
                total_input_tokens: 10_000,
                total_cached_tokens: 6_575,
                steady_input_tokens: 7_500,
                steady_cached_tokens: 6_375,
                overall_hit_rate_pct: 85.0,
                total_turns: 4,
                hits: 3,
                partial_hits: 0,
                breaks: 1,
                last_break_diagnostic: Some("Cache break: 0% hit rate. Item #2 was pruned/trimmed".into()),
            },
            recent_turns: vec![
                xai_grok_shell::session::CacheTurnRecord {
                    turn_idx: "1".into(),
                    loop_index: 0,
                    prompt_tokens: 2500,
                    cached_prompt_tokens: 200,
                    completion_tokens: 150,
                    cache_hit_rate_pct: 8.0,
                    status: xai_grok_shell::session::CacheStatus::FirstTurn,
                    divergence: xai_grok_shell::session::PrefixDivergence::FirstTurn,
                    diagnostic: "First turn in session (cold cache).".into(),
                    timestamp_rfc3339: "2026-08-14T00:00:00Z".into(),
                },
                xai_grok_shell::session::CacheTurnRecord {
                    turn_idx: "2".into(),
                    loop_index: 0,
                    prompt_tokens: 2500,
                    cached_prompt_tokens: 2000,
                    completion_tokens: 150,
                    cache_hit_rate_pct: 80.0,
                    status: xai_grok_shell::session::CacheStatus::Hit,
                    divergence: xai_grok_shell::session::PrefixDivergence::PrefixIntact {
                        preserved_items: 4,
                        new_items: 1,
                    },
                    diagnostic: "Cache hit: 80.0%".into(),
                    timestamp_rfc3339: "2026-08-14T00:01:00Z".into(),
                },
            ],
        };
        let text = session_cache_block_text(&resp);
        assert!(text.contains("Cache hit rate: 85.0% (6,375 of 7,500 steady-state input tokens cached; cold start excluded)"), "{text}");
        assert!(text.contains("Turns tracked:  4 (3 hits · 0 partial · 1 breaks)"), "{text}");
        assert!(text.contains("Last break:     Cache break: 0% hit rate. Item #2 was pruned/trimmed"), "{text}");
        assert!(text.contains("Turn #1 (loop 0) — cold start (2,500 in) · First turn in session (cold cache)."), "{text}");
        assert!(text.contains("Turn #2 (loop 0) — 80.0% hit (2,500 in, 2,000 cached) · Cache hit: 80.0%"), "{text}");
    }

    #[test]
    fn session_cache_block_cold_start_only() {
        let resp = xai_grok_shell::extensions::cache::SessionCacheResponse {
            summary: xai_grok_shell::session::CacheSummary {
                total_input_tokens: 2_500,
                total_cached_tokens: 200,
                steady_input_tokens: 0,
                steady_cached_tokens: 0,
                overall_hit_rate_pct: 0.0,
                total_turns: 1,
                hits: 0,
                partial_hits: 0,
                breaks: 0,
                last_break_diagnostic: None,
            },
            recent_turns: vec![xai_grok_shell::session::CacheTurnRecord {
                turn_idx: "1".into(),
                loop_index: 0,
                prompt_tokens: 2500,
                cached_prompt_tokens: 200,
                completion_tokens: 150,
                cache_hit_rate_pct: 8.0,
                status: xai_grok_shell::session::CacheStatus::FirstTurn,
                divergence: xai_grok_shell::session::PrefixDivergence::FirstTurn,
                diagnostic: "First turn in session (cold cache).".into(),
                timestamp_rfc3339: "2026-08-14T00:00:00Z".into(),
            }],
        };
        let text = session_cache_block_text(&resp);
        assert!(text.contains("Cache hit rate: n/a (cold-start request only so far)"), "{text}");
        assert!(text.contains("Turn #1 (loop 0) — cold start (2,500 in)"), "{text}");
        assert!(!text.contains("% hit ("), "must not render a hit-rate percentage: {text}");
    }
}
