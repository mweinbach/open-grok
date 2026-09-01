use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use xai_grok_status_line::{ResolvedStatusLine, StatusLineConfig, StatusLineItem, StatusLineType};

use super::draws_a_row;

pub(crate) fn global() -> &'static StatusLineMetrics {
    static METRICS: StatusLineMetrics = StatusLineMetrics::new();
    &METRICS
}

#[derive(Debug)]
pub(crate) struct StatusLineMetrics {
    kind: OnceLock<&'static str>,
    draws_a_row: AtomicBool,
    had_content: AtomicBool,
    reported: AtomicBool,
    ok: AtomicU64,
    failed: AtomicU64,
    timed_out: AtomicU64,
    abandoned: AtomicU64,
    slowest_ms: AtomicU64,
}

impl StatusLineMetrics {
    const fn new() -> Self {
        Self {
            kind: OnceLock::new(),
            draws_a_row: AtomicBool::new(false),
            had_content: AtomicBool::new(false),
            reported: AtomicBool::new(false),
            ok: AtomicU64::new(0),
            failed: AtomicU64::new(0),
            timed_out: AtomicU64::new(0),
            abandoned: AtomicU64::new(0),
            slowest_ms: AtomicU64::new(0),
        }
    }

    pub(crate) fn note_content(&self) {
        self.had_content.store(true, Ordering::Relaxed);
    }

    pub(crate) fn report_config(&self, cfg: &StatusLineConfig) {
        let kind = cfg.declared_kind().map_or("unset", StatusLineType::as_str);
        if self.kind.set(kind).is_err() {
            return;
        }
        self.draws_a_row.store(draws_a_row(cfg), Ordering::Relaxed);
        xai_grok_telemetry::session_ctx::log_event(
            xai_grok_telemetry::events::StatusLineConfigured {
                kind,
                row_shows_a_problem: cfg.problem_to_paint().is_some(),
                items: items_label(cfg),
                custom_items: cfg.has_custom_items(),
            },
        );
    }

    pub(crate) fn record_ok(&self, duration_ms: u64) {
        self.record(&self.ok, duration_ms);
    }

    pub(crate) fn record_failed(&self, duration_ms: u64) {
        self.record(&self.failed, duration_ms);
    }

    pub(crate) fn record_timed_out(&self, duration_ms: u64) {
        self.record(&self.timed_out, duration_ms);
    }

    pub(crate) fn record_abandoned(&self) {
        self.abandoned.fetch_add(1, Ordering::Relaxed);
    }

    fn record(&self, counter: &AtomicU64, duration_ms: u64) {
        counter.fetch_add(1, Ordering::Relaxed);
        self.slowest_ms.fetch_max(duration_ms, Ordering::Relaxed);
    }

    pub(crate) fn report_health(&self) {
        if let Some(event) = self.health_event() {
            xai_grok_telemetry::session_ctx::log_event(event);
        }
    }

    fn health_event(&self) -> Option<xai_grok_telemetry::events::StatusLineHealth> {
        if !self.draws_a_row.load(Ordering::Relaxed) {
            return None;
        }
        let kind = self.kind.get()?;
        if self.reported.swap(true, Ordering::Relaxed) {
            return None;
        }
        Some(xai_grok_telemetry::events::StatusLineHealth {
            kind,
            had_content: self.had_content.load(Ordering::Relaxed),
            runs_ok: self.ok.load(Ordering::Relaxed),
            runs_failed: self.failed.load(Ordering::Relaxed),
            runs_timed_out: self.timed_out.load(Ordering::Relaxed),
            runs_abandoned: self.abandoned.load(Ordering::Relaxed),
            slowest_ms: self.slowest_ms.load(Ordering::Relaxed),
        })
    }
}

fn items_label(cfg: &StatusLineConfig) -> String {
    match cfg.resolve() {
        Some(ResolvedStatusLine::Builtin { items }) => items
            .iter()
            .copied()
            .map(StatusLineItem::as_str)
            .collect::<Vec<_>>()
            .join(","),
        Some(ResolvedStatusLine::Command { .. }) | None => String::new(),
    }
}

#[cfg(test)]
#[path = "metrics_tests.rs"]
mod tests;
