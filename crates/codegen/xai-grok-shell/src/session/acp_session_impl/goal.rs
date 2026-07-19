//! Goal-orchestration concern for `SessionActor`.

use super::*;

/// Per-role toolset capability requirement for the parent-side gate.
///
/// Each role needs a different minimum toolset to do its job; a configured
/// harness `agent_type` whose role toolset lacks the capability fails open to
/// the current model + session harness rather than spawning an unusable
/// verifier.
#[derive(Debug, Clone, Copy)]
pub(crate) enum RoleCapability {
    /// Skeptic: reads + greps code to corroborate diff hunks.
    Skeptic,
    /// Strategist: reads + greps + runs commands while investigating traces.
    Strategist,
}

impl RoleCapability {
    /// `true` when `summary`'s toolset satisfies this role's minimum
    /// capability, keyed on the `can_*` flags (`can_search` for grep,
    /// `can_execute` for terminal/bash).
    fn is_satisfied(
        self,
        summary: &xai_grok_tools::implementations::grok_build::task::types::SubagentTypeSummary,
    ) -> bool {
        match self {
            Self::Skeptic => summary.can_read && summary.can_search,
            Self::Strategist => summary.can_read && summary.can_search && summary.can_execute,
        }
    }
}

/// Panel-scoped memoization for `resolve_goal_role_override`:
/// at most one `describe_subagent_type` coordinator round-trip per distinct
/// `agent_type`, so a multi-index skeptic panel sharing one agent_type costs
/// one round-trip instead of N. A single planner/strategist resolve uses a
/// fresh (empty) cache — no cross-call sharing needed.
#[derive(Default)]
pub(crate) struct PanelResolveCache {
    /// harness `agent_type` → describe outcome (the coordinator round-trip
    /// result for the role's `general-purpose` toolset on that harness).
    describe: std::collections::HashMap<
        String,
        xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome,
    >,
}

/// Build a role's prompt tool names from its resolved spawn override (
/// option (a)). A committed explicit pair draws its names from the SAME
/// `describe_subagent_type` summary cached during the gate (no second
/// round-trip); every other case (inherit, fail-open, or a missing summary)
/// falls back to the parent-toolset `inherit` names so the prompt always
/// renders fully.
fn role_tool_names_from(
    override_: &crate::session::goal_planner::RoleSpawnOverride,
    cache: &PanelResolveCache,
    inherit: &crate::session::goal_role_tools::RoleToolNames,
) -> crate::session::goal_role_tools::RoleToolNames {
    use xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome;
    // `override_.agent_type` is the committed harness; the cache is keyed on it.
    match override_.agent_type.as_deref() {
        Some(harness) => match cache.describe.get(harness) {
            Some(SubagentDescribeOutcome::Ok(summary)) => {
                crate::session::goal_role_tools::RoleToolNames::from_summary(summary)
            }
            _ => inherit.clone(),
        },
        None => inherit.clone(),
    }
}

/// How [`SessionActor::record_verdict_on_orchestration`] updates the
/// orchestration's `last_classifier_gaps`. A real `NotAchieved` panel result
/// stamps fresh curated gaps (`Set`), a verdict that resolves them clears
/// (`Clear`), and a synthetic verdict that ran no panel leaves any stored
/// real gaps replaying into the continuation directive (`Preserve`).
pub(crate) enum GapsUpdate<'a> {
    Set(&'a str),
    Clear,
    Preserve,
}

impl SessionActor {
    /// Drain pending `UpdateGoalInput`s from the `update_goal` tool channel
    /// and apply them to `GoalTracker`. Emits `GoalUpdated` notifications.
    ///
    /// `current_tokens` is the parent session's total token count, used to
    /// flush accurate token accounting before status transitions (completion,
    /// pause) — without this the pager would show tokens_used=0.
    ///
    /// `purpose` distinguishes turn-end drains (which fire the staged
    /// verification gate for eligible `completed: true` commands when
    /// enabled) from mid-turn drains (which defer such commands into
    /// `pending_classifier_completions`
    /// so the verifier-skeptic subagents never run concurrently with
    /// model inference).
    ///
    /// An update with `blocked_reason: Some(_)` increments the blocked
    /// streak counter. Only after 3 consecutive blocked attempts does the
    /// goal actually pause with [`GoalPauseReason::Verification`].
    /// Subsequent updates drained in the same call are skipped after a
    /// successful block. The chat notification and `pause_message` stash
    /// are gated on the pause-transition's return value, so a block signal
    /// against a non-Active goal is silently dropped.
    pub(super) async fn drain_goal_updates(&self, current_tokens: i64, purpose: DrainPurpose) {
        self.drain_goal_updates_with_extra(current_tokens, purpose, Vec::new())
            .await;
    }

    /// Body of [`drain_goal_updates`] accepting pre-popped envelopes
    /// from the drainer task. Test callers pass an empty `extra` and
    /// rely on `goal_update_rx` for the channel half.
    pub(super) async fn drain_goal_updates_with_extra(
        &self,
        current_tokens: i64,
        purpose: DrainPurpose,
        extra: Vec<xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalEnvelope>,
    ) {
        use xai_grok_tools::implementations::grok_build::update_goal::{
            RejectReason, UpdateGoalAck,
        };
        // The `update_goal` tool and its `GoalUpdateHandle` are always
        // registered (see `spawn_session_actor`), so a model can call
        // `update_goal` in a session that never entered goal mode — e.g. any
        // plain eval/coding rollout. When the harness is disabled there is no
        // orchestration to update, but every envelope must still be answered:
        // dropping the ack oneshot here surfaces to the tool as the misleading
        // `harness_no_ack` ("Goal-update harness dropped the response channel")
        // error. Reject cleanly instead, draining both the drainer-supplied
        // `extra` envelopes and any channel-buffered ones so no ack receiver is
        // left hanging.
        if !self.goal_harness_enabled() {
            let reject = || UpdateGoalAck::Rejected {
                reason: RejectReason::HarnessDisabled,
                detail: "Goal mode is not active for this session (no /goal run in \
                         progress); update_goal has no effect."
                    .to_string(),
            };
            for (_input, ack_tx) in extra {
                send_ack(ack_tx, reject());
            }
            if let Some(rx) = self.goal_update_rx.borrow_mut().as_mut() {
                while let Ok((_input, ack_tx)) = rx.try_recv() {
                    send_ack(ack_tx, reject());
                }
            }
            return;
        }
        // FIFO over three sources:
        //   1. `pending_classifier_completions` — inputs deferred by
        //      an earlier MidTurn drain; their acks are already resolved.
        //   2. `extra` — envelopes from the drainer task; ack live.
        //   3. The mpsc channel — used by tests that drive this
        //      function directly.
        let mut cmds: Vec<DrainSource> = match purpose {
            DrainPurpose::TurnEnd => self
                .pending_classifier_completions
                .lock()
                .drain(..)
                .map(DrainSource::Pending)
                .collect(),
            DrainPurpose::MidTurn => Vec::new(),
        };
        cmds.extend(extra.into_iter().map(DrainSource::Channel));
        {
            let mut rx_slot = self.goal_update_rx.borrow_mut();
            if let Some(rx) = rx_slot.as_mut() {
                cmds.extend(std::iter::from_fn(|| rx.try_recv().ok()).map(DrainSource::Channel));
            }
        }
        let notify = self.goal_notify_sender();
        let mut block_seen = false;
        // When an auto-pause (cap, stall/no_progress, or blocked) fires
        // mid-drain, remaining entries are collapsed into a single
        // `GoalClassifierPendingQueueCleared` event instead of per-entry
        // `dropped_after_pause` / `fail_open` noise.
        let mut cap_dropped: u32 = 0;
        let mut auto_paused_mid_drain = false;
        for source in cmds {
            let (cmd, ack_tx) = source.into_parts();
            if auto_paused_mid_drain {
                // Strictly-later drop after the goal auto-paused. Counted
                // for the summary event after the loop; ack resolved as
                // Rejected so the tool reply is not left hanging.
                cap_dropped = cap_dropped.saturating_add(1);
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::DroppedAfterPauseInDrain,
                        detail: "Goal auto-paused during verification; this completion was \
                                 dropped without further verification"
                            .to_string(),
                    },
                );
                drop(cmd);
                continue;
            }
            if block_seen {
                // A prior cmd transitioned to Blocked; reject the
                // rest so a follow-up `completed: true` cannot race
                // past the just-fired pause.
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::BlockSeenInDrain,
                        detail: "Goal just transitioned to Blocked by a prior update in this drain"
                            .to_string(),
                    },
                );
                continue;
            }
            if let Some(reason) = cmd.blocked_reason {
                let streak = self
                    .goal_blocked_streak
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                    + 1;
                if streak < 3 {
                    tracing::info!(
                        streak,
                        "blocked attempt rejected — model must try {}/3 before blocking",
                        streak,
                    );
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Accepted {
                            summary: format!(
                                "Blocked attempt {streak}/3 recorded. The harness needs \
                                 3 consecutive blocked attempts before pausing the goal — \
                                 continue retrying or refining the approach."
                            ),
                        },
                    );
                    continue;
                }
                let detail = cmd.message;
                let chat_text = format_blocked_chat_notification(&reason, detail.as_deref());
                // Format the success summary from `&reason` BEFORE
                // consuming `reason` into `pause_msg` — saves a clone.
                let success_summary = format!("Goal blocked: {reason}.");
                let pause_msg = match detail {
                    Some(d) => format!("{reason}\n{d}"),
                    None => reason,
                };
                let applied = self
                    .auto_pause_goal_if_active_with_message(
                        crate::session::goal_tracker::GoalPauseReason::Verification,
                        pause_msg,
                    )
                    .await;
                if applied {
                    self.send_slash_command_output(&chat_text).await;
                    block_seen = true;
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Accepted {
                            summary: success_summary,
                        },
                    );
                } else {
                    // Block signal against a non-Active goal is
                    // silently dropped today — surface the drop as an
                    // explicit Rejected ack so the tool reply is
                    // accurate.
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::BlockedAgainstNonActive,
                            detail: "Goal is not Active; blocked_reason ignored".to_string(),
                        },
                    );
                }
                continue;
            }
            // Verdict files are harness-owned (skeptic panel, per-goal
            // scratch root); this drain never gates `completed: true` on
            // verdict-file state.
            if cmd.completed != Some(true) {
                // Message-only updates emit a refreshed `GoalUpdated`
                // without status mutation — preserves today's behaviour
                // for tool calls that carry neither `completed` nor
                // `blocked_reason`.
                let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                let mut tracker = self.goal_tracker.lock();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                drop(tracker);
                let summary = cmd
                    .message
                    .clone()
                    .map(|m| format!("{m}."))
                    .unwrap_or_else(|| "Goal updated.".to_string());
                try_send_ack(ack_tx, UpdateGoalAck::Accepted { summary });
                continue;
            }

            // Reset before the verification-stage guards: the disabled
            // fast-path and the NonActive guard both early-return.
            self.goal_blocked_streak
                .store(0, std::sync::atomic::Ordering::Relaxed);

            let policy = self.resolve_goal_classifier_policy();

            // Classifier disabled: straight to `tracker.complete()`.
            if !policy.enabled {
                let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
                self.prune_subagent_records_for_active_goal();
                // An earlier blocked-path command in this drain awaits, so a
                // concurrent mid-turn drain can refill the queue before a
                // later `completed: true` lands here.
                self.clear_pending_classifier_completions();
                let mut tracker = self.goal_tracker.lock();
                tracker.complete();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                drop(tracker);
                try_send_ack(ack_tx, UpdateGoalAck::CompletedWithoutClassifier);
                continue;
            }

            // Guard 1: only fire when the goal is Active. Non-Active
            // splits into "post-cap drop" (emit
            // `GoalClassifierDroppedAfterCap` with the real
            // attempts_seen) and "other non-Active" (emit
            // `FailOpen { GoalNotActiveAtResolve }`).
            //
            // Single-lock fast-path returning `None` on Active (no
            // field reads needed); `Some((attempts_seen, post_cap))`
            // on non-Active. An absent snapshot maps to
            // `Some((0, false))` so the FailOpen branch fires.
            let non_active_meta: Option<(u32, bool)> = {
                let tracker = self.goal_tracker.lock();
                match tracker.snapshot() {
                    Some(o) if o.status == crate::session::goal_tracker::GoalStatus::Active => None,
                    Some(o) => Some((
                        o.classifier_runs_attempted,
                        o.status == crate::session::goal_tracker::GoalStatus::BackOffPaused
                            && o.classifier_max_runs
                                .is_some_and(|cap| o.classifier_runs_attempted >= cap),
                    )),
                    None => Some((0, false)),
                }
            };
            if let Some((attempts_seen, post_cap)) = non_active_meta {
                tracing::debug!(
                    "completed: true against non-Active goal; dropping classifier fire"
                );
                if post_cap {
                    self.events.emit(
                        crate::session::events::Event::GoalClassifierDroppedAfterCap {
                            attempts_seen,
                        },
                    );
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::PostCap,
                            detail: format!(
                                "Classifier cap already reached ({attempts_seen} attempts); goal \
                                 is BackOff-paused. Do not call update_goal again until the user \
                                 resumes."
                            ),
                        },
                    );
                } else {
                    self.events.emit(
                        crate::session::events::Event::GoalClassifierFailOpen {
                            reason: crate::session::events::GoalClassifierFailOpenReason::GoalNotActiveAtResolve
                                .as_const_str(),
                            attempt: attempts_seen,
                            latency_ms: 0,
                        },
                    );
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::NonActive,
                            detail: "Goal is not Active; cannot mark complete".to_string(),
                        },
                    );
                }
                continue;
            }

            // Guard 2: defer classifier-eligible completions if this
            // drain is mid-turn. The deferred command will be processed
            // at the next `DrainPurpose::TurnEnd` drain ahead of the
            // regular channel, preserving FIFO order. Cap the queue at
            // `GOAL_CLASSIFIER_PENDING_QUEUE_CAP`; on overflow drop the
            // OLDEST entry (the new entry reflects the model's current
            // intent) and emit `FailClosed { PendingQueueFull }`.
            if matches!(purpose, DrainPurpose::MidTurn) {
                // Cap the queue at `GOAL_CLASSIFIER_PENDING_QUEUE_CAP`;
                // on overflow drop the OLDEST entry (the new entry
                // reflects the model's current intent). The dropped
                // entry's ack was ALREADY resolved at its own MidTurn
                // defer time, so we just emit the eviction telemetry
                // and let the dropped input fall out of scope —
                // there's no parked ack to update.
                let pending_depth = {
                    let mut q = self.pending_classifier_completions.lock();
                    if q.len() >= GOAL_CLASSIFIER_PENDING_QUEUE_CAP {
                        q.pop_front();
                        self.events.emit(
                            crate::session::events::Event::GoalClassifierFailClosed {
                                reason:
                                    crate::session::events::GoalClassifierFailClosedReason::PendingQueueFull
                                        .as_const_str(),
                                attempt: 0,
                            },
                        );
                    }
                    q.push_back(cmd);
                    u32::try_from(q.len()).unwrap_or(u32::MAX)
                };
                self.events.emit(
                    crate::session::events::Event::GoalClassifierMidTurnDeferred { pending_depth },
                );
                // Ack immediately to avoid deadlocking the actor on
                // the tool's `await`.
                try_send_ack(ack_tx, UpdateGoalAck::DeferredToTurnEnd { pending_depth });
                continue;
            }

            // Guard 3: re-entry guard. A second `completed: true`
            // racing in while a classifier is already running is
            // accounted as a synthetic NotAchieved so retry spam is
            // bounded by `classifier_max_runs`. `SeqCst` pairs with
            // the `InFlightGuard` drop for panic-safe acquire/release.
            if self
                .goal_classifier_in_flight
                .compare_exchange(
                    false,
                    true,
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                )
                .is_err()
            {
                self.events
                    .emit(crate::session::events::Event::GoalClassifierFailClosed {
                    reason:
                        crate::session::events::GoalClassifierFailClosedReason::ConcurrentInFlight
                            .as_const_str(),
                    attempt: 0,
                });
                let synthetic_info = self
                    .account_not_achieved_without_sampler(
                        &policy,
                        NotAchievedSyntheticReason::ConcurrentInFlight,
                    )
                    .await;
                // Resolve the second tool call's ack with the synthetic
                // verdict so the model gets immediate feedback instead
                // of discovering the rejection via a nudge.
                if let Some((attempt, details_path, cap_reached)) = synthetic_info {
                    if cap_reached {
                        // Source the flag from the tracker via
                        // `cap_just_reached` to keep the two emit
                        // sites in lock-step.
                        if self.cap_just_reached() {
                            auto_paused_mid_drain = true;
                        }
                        try_send_ack(
                            ack_tx,
                            UpdateGoalAck::ClassifierCapReached {
                                details_path,
                                attempt,
                            },
                        );
                    } else {
                        try_send_ack(
                            ack_tx,
                            UpdateGoalAck::ClassifierConcurrentInFlight {
                                details_path,
                                attempt,
                                max_runs: policy.max_runs,
                            },
                        );
                    }
                } else {
                    // Orchestration vanished mid-flight — distinct
                    // from "classifier still verifying".
                    try_send_ack(
                        ack_tx,
                        UpdateGoalAck::Rejected {
                            reason: RejectReason::InFlightOrchestrationVanished,
                            detail: "Goal orchestration vanished during in-flight short-circuit"
                                .to_string(),
                        },
                    );
                }
                continue;
            }
            // Scope-guard the in-flight flag for panic safety.
            let _in_flight_guard = InFlightGuard {
                flag: &self.goal_classifier_in_flight,
            };
            // Capture fire-start time so post-fire branches emit real
            // wall-clock elapsed, not `latency_ms: 0`.
            let fire_started = std::time::Instant::now();

            // Reserve an attempt slot under the tracker mutex (no
            // `.await` held). Defense-in-depth: orchestration vanishing
            // between Guard 1 and slot reservation requires a non-async
            // mutation by another path (no `.await` between them under
            // the LocalSet); the bail-out emits a fail-open event so
            // any production occurrence is observable.
            let Some(attempt) = self.reserve_classifier_attempt_slot(&policy) else {
                self.events
                    .emit(crate::session::events::Event::GoalClassifierFailOpen {
                    reason:
                        crate::session::events::GoalClassifierFailOpenReason::GoalNotActiveAtResolve
                            .as_const_str(),
                    attempt: 0,
                    latency_ms: fire_started.elapsed().as_millis() as u64,
                });
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::OrchestrationVanished,
                        detail: "Goal orchestration vanished before classifier could start"
                            .to_string(),
                    },
                );
                continue;
            };

            self.emit_goal_verifying(current_tokens);

            // Refund the reserved slot if this future is dropped (turn cancel /
            // user pause) before it resolves; disarmed before
            // `apply_classifier_outcome`, which owns its own rollback. The
            // skip-list below states which paused states keep the slot.
            let mut slot_guard = TrackerDropGuard::new(&self.goal_tracker, |t| {
                use crate::session::goal_tracker::GoalStatus;
                // `Blocked` is defensive (its only setter disarms first);
                // kept so the skip-list states the full pause policy.
                if !matches!(
                    t.status(),
                    Some(
                        GoalStatus::BackOffPaused
                            | GoalStatus::NoProgressPaused
                            | GoalStatus::Blocked
                    ),
                ) {
                    t.rollback_classifier_attempt();
                }
            });

            // Scoped so the "Verifying…" latch clears on normal exit AND on
            // a turn cancel dropping this future — later `GoalUpdated`s
            // carry the cleared flag, and a cancelled turn never latches it.
            let outcome = {
                let _verifying_latch = TrackerDropGuard::new(&self.goal_tracker, |t| {
                    if let Some(o) = t.snapshot_mut() {
                        o.verifying_in_flight = false;
                    }
                });
                self.run_verification_stage_for_drain(attempt, policy.max_runs)
                    .await
            };

            // Re-check status: the user may have paused mid-classifier.
            // Emit fail-open with real elapsed (not `0`) so dashboards
            // can filter by attempt > 0. The slot refund rides the
            // still-armed `slot_guard` drop at the end of this iteration.
            let status_changed = self.goal_tracker.lock().status()
                != Some(crate::session::goal_tracker::GoalStatus::Active);
            if status_changed {
                self.events
                    .emit(crate::session::events::Event::GoalClassifierFailOpen {
                    reason:
                        crate::session::events::GoalClassifierFailOpenReason::GoalNotActiveAtResolve
                            .as_const_str(),
                    attempt,
                    latency_ms: fire_started.elapsed().as_millis() as u64,
                });
                try_send_ack(
                    ack_tx,
                    UpdateGoalAck::Rejected {
                        reason: RejectReason::StatusChangedDuringClassifier,
                        detail: "Goal transitioned to non-Active during classifier run".to_string(),
                    },
                );
                continue;
            }
            slot_guard.disarm();

            let ack = self
                .apply_classifier_outcome(&policy, attempt, outcome, &notify)
                .await;
            // Any auto-pause this outcome triggered (cap, stall, or
            // blocked) leaves the goal paused; consolidate the rest of
            // the drain instead of letting each fall through Guard 1 as
            // per-entry FailOpen noise. A completion (Complete) does not
            // pause, so it keeps its existing per-entry handling.
            if !auto_paused_mid_drain && self.goal_is_paused() {
                auto_paused_mid_drain = true;
            }
            try_send_ack(ack_tx, ack);
        }
        if cap_dropped > 0 {
            self.events.emit(
                crate::session::events::Event::GoalClassifierPendingQueueCleared {
                    dropped: cap_dropped,
                },
            );
        }
    }

    /// Reserve the next classifier attempt slot. `None` if the
    /// orchestration vanished between Guard 1 and reservation.
    pub(super) fn reserve_classifier_attempt_slot(
        &self,
        policy: &GoalClassifierPolicy,
    ) -> Option<u32> {
        let mut tracker = self.goal_tracker.lock();
        let o = tracker.snapshot_mut()?;
        o.classifier_runs_attempted = o.classifier_runs_attempted.saturating_add(1);
        o.classifier_max_runs = Some(policy.max_runs);
        // Verification is firing — restart the re-verify round counter.
        o.rounds_since_verify = 0;
        Some(o.classifier_runs_attempted)
    }

    /// `true` iff the orchestration is BackOff-paused with the
    /// classifier-run counter at or past its cap (cap just fired,
    /// remaining drain entries are stale).
    fn cap_just_reached(&self) -> bool {
        let tracker = self.goal_tracker.lock();
        tracker.snapshot().is_some_and(|o| {
            o.status == crate::session::goal_tracker::GoalStatus::BackOffPaused
                && o.classifier_max_runs
                    .is_some_and(|cap| o.classifier_runs_attempted >= cap)
        })
    }

    /// `true` iff the goal is in any paused state — used after applying
    /// a classifier outcome to detect a cap / stall / blocked auto-pause
    /// and consolidate the rest of the mid-drain queue.
    fn goal_is_paused(&self) -> bool {
        self.goal_tracker
            .lock()
            .status()
            .is_some_and(|s| s.is_paused())
    }

    /// Apply a `GoalClassifierOutcome` to the tracker and queue any
    /// resulting side-effects (nudge push, cap-reached pause).
    async fn apply_classifier_outcome(
        &self,
        policy: &GoalClassifierPolicy,
        attempt: u32,
        outcome: crate::session::goal_classifier::GoalClassifierOutcome,
        notify: &crate::session::goal_orchestrator::GoalNotifySender,
    ) -> xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck {
        use crate::session::goal_classifier::GoalClassifierOutcome;
        use crate::session::goal_tracker::GoalClassifierVerdict;
        use xai_grok_tools::implementations::grok_build::update_goal::UpdateGoalAck;

        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        match outcome {
            GoalClassifierOutcome::Achieved { details_path } => {
                self.prune_subagent_records_for_active_goal();
                // Entries deferred by a concurrent mid-turn drain while the
                // panel ran claim against the now-achieved goal.
                self.clear_pending_classifier_completions();
                let details_path = {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::Achieved,
                        Some(details_path.as_str()),
                        GapsUpdate::Clear,
                    );
                    // Goal solved: the NotAchieved streak is broken and any
                    // stale strategist note must stop replaying.
                    tracker.reset_strategist_state();
                    tracker.complete();
                    // Ack the location `complete()` rescued the details file
                    // to, so the "See <path>" pointer stays readable.
                    let details_path = tracker
                        .snapshot()
                        .and_then(|o| o.last_classifier_details_path.clone())
                        .unwrap_or(details_path);
                    notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                    details_path
                };
                // Real, verified achievement (NOT the FailOpenAchieved arm
                // below): run the ONE closing summarizer. Fail-OPEN — never
                // blocks the completion that already happened above.
                self.maybe_run_goal_summarizer(attempt).await;
                UpdateGoalAck::ClassifierAchieved { details_path }
            }
            GoalClassifierOutcome::NotAchieved {
                details_path,
                gaps_summary,
                pause_summary,
                gap_fingerprint,
            } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::NotAchieved,
                        Some(details_path.as_str()),
                        GapsUpdate::Set(gaps_summary.as_str()),
                    );
                    // Bump the strategist streak under the same lock; the
                    // trigger below (after the cap/stall guards, so a paused
                    // round never also fires) reads the persisted state.
                    tracker.record_not_achieved_streak();
                }
                // Cap takes precedence over the stall check: an at-cap
                // rejection acks CapReached even if its fingerprint also
                // repeated (matters at max_runs == 1).
                if attempt >= policy.max_runs {
                    self.auto_pause_for_classifier_cap(attempt, &details_path, &pause_summary)
                        .await;
                    return UpdateGoalAck::ClassifierCapReached {
                        details_path,
                        attempt,
                    };
                }
                // Stall early-exit: an unchanged gap fingerprint across
                // consecutive attempts means the model addressed nothing,
                // so pause now rather than spend the rest of the cap. An
                // empty fingerprint carries no stable content, so it never
                // counts as a repeat.
                let stalled = !gap_fingerprint.is_empty()
                    && self
                        .goal_tracker
                        .lock()
                        .record_classifier_stall(&gap_fingerprint);
                if stalled {
                    self.auto_pause_for_classifier_stall(&details_path, &pause_summary)
                        .await;
                    return UpdateGoalAck::ClassifierStalled {
                        details_path,
                        attempt,
                    };
                }
                // Stall-triggered strategist: fire only when neither the cap
                // nor the stall paused this round (both returned above). The
                // trigger is SKIP-ROBUST (`>= last_fired + N`) so a synthetic
                // streak bump that jumps past a multiple of N still fires.
                // Claim the fire atomically (check + record under one lock) so
                // re-entrancy can't double-fire, then release the lock before
                // the await. The strategist is best-effort / fail-open — never
                // pauses the goal, never changes the ack.
                let every = self.goal_strategist_every;
                let claimed =
                    self.goal_tracker
                        .lock()
                        .claim_strategist_fire(|consecutive, last_fired| {
                            crate::session::goal_strategist::strategist_should_fire(
                                consecutive,
                                last_fired,
                                every,
                            )
                        });
                if let Some(consecutive_not_achieved) = claimed {
                    self.maybe_run_goal_strategist(attempt, consecutive_not_achieved)
                        .await;
                }
                // Feedback is delivered by the in-turn continuation directive
                // (`prepare_goal_continuation` inlines the persisted gaps), so
                // no separate nudge turn is queued.
                notify.emit_goal_updated(
                    &mut self.goal_tracker.lock(),
                    tokens_used,
                    finished_marginal,
                );
                UpdateGoalAck::ClassifierNotAchieved {
                    details_path,
                    attempt,
                    max_runs: policy.max_runs,
                }
            }
            GoalClassifierOutcome::Blocked {
                details_path,
                pause_summary,
            } => {
                {
                    let mut tracker = self.goal_tracker.lock();
                    Self::record_verdict_on_orchestration(
                        &mut tracker,
                        GoalClassifierVerdict::NotAchieved,
                        Some(details_path.as_str()),
                        GapsUpdate::Clear,
                    );
                    // A blocked rejection is not an ordinary retry — give
                    // the reserved slot back and clear the stall streak +
                    // ALL strategist state (streak, last-fired, stale note)
                    // so a resumed goal starts clean (symmetric to the slot
                    // rollback).
                    tracker.rollback_classifier_attempt();
                    tracker.reset_classifier_stall();
                    tracker.reset_strategist_state();
                }
                self.auto_pause_for_classifier_blocked(&details_path, &pause_summary)
                    .await;
                UpdateGoalAck::ClassifierBlocked { details_path }
            }
            GoalClassifierOutcome::FailOpenAchieved {
                reason,
                details_path,
            } => {
                tracing::warn!(
                    ?reason,
                    "goal verification fail-open (infra-class) → Achieved",
                );
                self.prune_subagent_records_for_active_goal();
                self.clear_pending_classifier_completions();
                let mut tracker = self.goal_tracker.lock();
                Self::record_verdict_on_orchestration(
                    &mut tracker,
                    GoalClassifierVerdict::Achieved,
                    (!details_path.is_empty()).then_some(details_path.as_str()),
                    GapsUpdate::Clear,
                );
                // Fail-open is treated as Achieved: break the streak and drop
                // any stale strategist note, symmetric with the real Achieved.
                tracker.reset_strategist_state();
                tracker.complete();
                notify.emit_goal_updated(&mut tracker, tokens_used, finished_marginal);
                UpdateGoalAck::ClassifierFailOpenAchieved {
                    reason: reason.as_const_str(),
                }
            }
        }
    }

    /// Stamp `last_classifier_verdict` / `last_classifier_details_path` /
    /// `last_classifier_gaps` / `last_classifier_at` on the live
    /// orchestration. `details_path` is borrowed and cloned into the
    /// snapshot only when present. `gaps` selects how
    /// `last_classifier_gaps` is updated — see [`GapsUpdate`].
    pub(super) fn record_verdict_on_orchestration(
        tracker: &mut crate::session::goal_tracker::GoalTracker,
        verdict: crate::session::goal_tracker::GoalClassifierVerdict,
        details_path: Option<&str>,
        gaps: GapsUpdate<'_>,
    ) {
        if let Some(o) = tracker.snapshot_mut() {
            o.last_classifier_verdict = Some(verdict);
            if let Some(p) = details_path {
                o.last_classifier_details_path = Some(p.to_string());
            }
            match gaps {
                GapsUpdate::Set(g) => o.last_classifier_gaps = Some(g.to_string()),
                GapsUpdate::Clear => o.last_classifier_gaps = None,
                GapsUpdate::Preserve => {}
            }
            o.last_classifier_at = Some(chrono::Utc::now().to_rfc3339());
        }
    }

    /// Active classifier policy: kill-switch + base cap plus any strategist
    /// bonus granted this goal.
    pub(super) fn resolve_goal_classifier_policy(&self) -> GoalClassifierPolicy {
        let bonus = self
            .goal_tracker
            .lock()
            .snapshot()
            .map_or(0, |o| o.strategist_cap_bonus);
        GoalClassifierPolicy {
            enabled: self.goal_classifier_enabled,
            max_runs: self.goal_classifier_max_runs.saturating_add(bonus),
        }
    }

    /// Latch `verifying_in_flight` (see its field doc) and emit a `GoalUpdated`
    /// so the pager shows the "Verifying…" badge while the skeptic panel runs;
    /// the caller's `TrackerDropGuard` clears the latch when the verification
    /// scope exits (including a turn cancel dropping it). The classifier
    /// counters are read straight off the orchestration (the caller bumped
    /// them via `reserve_classifier_attempt_slot`).
    ///
    /// Routed through [`GoalNotifySender::send_update`] (like
    /// `emit_goal_planning`) so it does NOT close the rewind window.
    fn emit_goal_verifying(&self, current_tokens: i64) {
        if !self.goal_harness_enabled() {
            return;
        }
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let update = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let Some(o) = tracker.snapshot_mut() else {
                return;
            };
            o.verifying_in_flight = true;
            crate::session::goal_orchestrator::build_goal_updated(o, tokens_used, finished_marginal)
        };
        self.goal_notify_sender().send_update(update);
    }

    /// Latch `planning_in_flight` and emit a `GoalUpdated` so the pager
    /// surfaces the "planning…" badge while the planner subagent runs.
    /// The latch (see its field doc) keeps the badge across mid-run
    /// updates; `maybe_run_goal_planner` resets it on every exit path.
    ///
    /// Routed through [`GoalNotifySender::send_update`] (like
    /// `emit_goal_verifying`) so it does NOT close the rewind window.
    pub(super) fn emit_goal_planning(&self, current_tokens: i64) {
        if !self.goal_harness_enabled() {
            return;
        }
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let update = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let Some(o) = tracker.snapshot_mut() else {
                return;
            };
            o.planning_in_flight = true;
            crate::session::goal_orchestrator::build_goal_updated(o, tokens_used, finished_marginal)
        };
        self.goal_notify_sender().send_update(update);
    }

    /// Apply NotAchieved accounting without invoking the sampler;
    /// used when Guard 3's in-flight short-circuit fires. Mirrors the
    /// `NotAchieved` arm (counter, persisted verdict, cap pause) modulo
    /// the synthetic details file. Does NOT touch
    /// `goal_classifier_in_flight` — the in-flight owner clears it.
    async fn account_not_achieved_without_sampler(
        &self,
        policy: &GoalClassifierPolicy,
        reason: NotAchievedSyntheticReason,
    ) -> Option<(u32, String, bool)> {
        use crate::session::goal_classifier::format_details_path;
        use crate::session::goal_tracker::GoalClassifierVerdict;

        let (attempt, verifier_id) = {
            let mut tracker = self.goal_tracker.lock();
            let o = tracker.snapshot_mut()?;
            o.classifier_runs_attempted = o.classifier_runs_attempted.saturating_add(1);
            o.classifier_max_runs = Some(policy.max_runs);
            (o.classifier_runs_attempted, o.verifier_id.clone())
        };
        let details_path = format_details_path(&verifier_id, attempt);
        let body = reason.details_body(attempt, policy.max_runs);
        // Best-effort write, gated like every classifier artifact: never
        // through an unverified (possibly squatted) root.
        let wrote_details =
            if crate::session::goal_tracker::ensure_goal_scratch_root(&verifier_id).is_ok() {
                tokio::fs::write(&details_path, body).await.is_ok()
            } else {
                false
            };
        // SECURITY: only surface the scratch-rooted path if WE actually wrote
        // the synthetic details. If ensure (or the write) failed — e.g. a local
        // attacker squatted the predictable scratch root — pointing the tracker,
        // tool acks, or the "See …" pause pointer there would show attacker-
        // planted content instead of harness output. When absent we record /
        // return no path and the ack & pause renderers omit the pointer.
        let details_ptr: Option<&str> = wrote_details.then_some(details_path.as_str());

        let gaps_summary = reason.gaps_summary();
        {
            let mut tracker = self.goal_tracker.lock();
            Self::record_verdict_on_orchestration(
                &mut tracker,
                GoalClassifierVerdict::NotAchieved,
                details_ptr,
                // Preserve: this synthetic verdict ran no panel — generic
                // text must not clobber actionable gaps from a real one.
                GapsUpdate::Preserve,
            );
            // Keep the strategist streak accurate on the synthetic
            // (concurrent-in-flight) rejection path, but do NOT fire the
            // strategist here — the trigger lives only in
            // `apply_classifier_outcome`'s real NotAchieved branch.
            tracker.record_not_achieved_streak();
        }

        let cap_reached = attempt >= policy.max_runs;
        if cap_reached {
            let pause_summary = format!(
                "{}:\n{gaps_summary}",
                crate::session::goal_classifier::PAUSE_GROUP_FIXABLE,
            );
            self.auto_pause_for_classifier_cap(attempt, details_ptr.unwrap_or(""), &pause_summary)
                .await;
        } else {
            // Feedback is delivered by the in-turn continuation directive, not
            // a separate nudge turn.
            let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
            let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }
        // Empty when the synthetic details were not written (see `details_ptr`);
        // the caller's ack renderer omits the "See …" pointer in that case.
        Some((attempt, details_ptr.unwrap_or("").to_owned(), cap_reached))
    }

    /// Emit the cap-reached event + auto-pause with `BackOff`, carrying
    /// a pause message built from `pause_summary` (grouped blockers) and
    /// the details pointer. Extracted so both the real-classifier
    /// `NotAchieved` path and the synthetic
    /// `account_not_achieved_without_sampler` path share one builder.
    ///
    /// Does NOT touch `pending_classifier_completions`. The queue is
    /// drained into the local `cmds` Vec at the top of
    /// `drain_goal_updates`; remaining post-cap entries are
    /// consolidated into one `GoalClassifierPendingQueueCleared`
    /// event by the drain loop.
    async fn auto_pause_for_classifier_cap(
        &self,
        attempt: u32,
        details_path: &str,
        pause_summary: &str,
    ) {
        self.events
            .emit(crate::session::events::Event::GoalClassifierCapReached { attempt });
        let msg = format_goal_pause_message(
            &format!("Goal classifier rejected completion {attempt} times — goal auto-paused."),
            pause_summary,
            details_path,
        );
        self.auto_pause_goal_if_active_with_message(
            crate::session::goal_tracker::GoalPauseReason::BackOff,
            msg,
        )
        .await;
    }

    /// Auto-pause early (before the run cap) when the verifier flagged the
    /// same gaps across consecutive attempts with no progress. Distinguished
    /// from the cap path by the `no_progress` telemetry reason (no `CapReached`
    /// event); the grouped blockers are surfaced in the pause message.
    async fn auto_pause_for_classifier_stall(&self, details_path: &str, pause_summary: &str) {
        let msg = format_goal_pause_message(
            "Goal verification flagged the same gaps with no progress across \
             consecutive attempts — goal auto-paused.",
            pause_summary,
            details_path,
        );
        self.auto_pause_goal_if_active_with_message(
            crate::session::goal_tracker::GoalPauseReason::NoProgress,
            msg,
        )
        .await;
    }

    /// Auto-pause as `Blocked` (needs-user) when every refuter is a
    /// non-model-fixable blocker — iterating cannot help, so the model
    /// must not retry. The grouped contradictions / unverifiable items
    /// are surfaced in the pause message.
    async fn auto_pause_for_classifier_blocked(&self, details_path: &str, pause_summary: &str) {
        let msg = format_goal_pause_message(
            "Goal verification found no model-fixable path — paused for your decision.",
            pause_summary,
            details_path,
        );
        self.auto_pause_goal_if_active_with_message(
            crate::session::goal_tracker::GoalPauseReason::Verification,
            msg,
        )
        .await;
    }

    /// Construct the verification-stage inputs from the live session
    /// state and invoke
    /// [`crate::session::goal_classifier::run_verification_stage`] with
    /// a [`ChannelSpawner`] backed by the subagent coordinator channel.
    /// Returns the stage outcome verbatim; the caller applies the
    /// lifecycle effects.
    pub(super) async fn run_verification_stage_for_drain(
        &self,
        attempt: u32,
        max_runs: u32,
    ) -> crate::session::goal_classifier::GoalClassifierOutcome {
        use crate::session::events::GoalClassifierFailOpenReason;
        use crate::session::goal_classifier::{
            ChannelSpawner, GoalClassifierOutcome, VerificationStageInputs, run_verification_stage,
        };

        let (
            objective,
            verifier_id,
            baseline_commit,
            plan_file,
            plan_baseline_file,
            goal_created_at,
            prior_skeptic0,
            prior_gaps,
            first_final_response,
            scratch_dir_ready,
        ) = {
            let tracker = self.goal_tracker.lock();
            let Some(o) = tracker.snapshot() else {
                return GoalClassifierOutcome::FailOpenAchieved {
                    reason: GoalClassifierFailOpenReason::GoalNotActiveAtResolve,
                    details_path: String::new(),
                };
            };
            (
                o.objective.clone(),
                o.verifier_id.clone(),
                o.changes_baseline_commit.clone(),
                o.plan_file.clone(),
                o.plan_baseline_file.clone(),
                crate::session::goal_classifier::evidence::parse_created_at_to_unix(&o.created_at),
                o.skeptic0_session_id.clone(),
                o.last_classifier_gaps.clone(),
                o.first_final_response.clone(),
                o.scratch_dir_ready,
            )
        };

        // Replay round 1's full deliverable summary as the breadth anchor on
        // re-verification rounds; a cold panel would otherwise see only this
        // round's narrow fix note. The anchor is frozen post-panel (below) so
        // a cancelled first verification never anchors a round with no verdict.
        let (final_response, anchor_to_persist) = {
            let current = {
                let items = self.chat_state_handle.get_conversation().await;
                crate::session::goal_classifier::evidence::extract_final_response(&items)
                    .unwrap_or_default()
            };
            let composed =
                crate::session::goal_classifier::evidence::compose_verifier_final_response(
                    first_final_response.as_deref(),
                    current,
                );
            (composed.to_send, composed.to_persist)
        };

        let model_id = self
            .chat_state_handle
            .get_sampling_config()
            .await
            .map(|c| c.model)
            .unwrap_or_default();

        let Some(event_tx) = self.tool_context.subagent_event_tx.clone() else {
            tracing::warn!("verification stage: no subagent coordinator channel; failing open");
            return GoalClassifierOutcome::FailOpenAchieved {
                reason: GoalClassifierFailOpenReason::SamplerError,
                details_path: String::new(),
            };
        };
        // Tag the skeptics with the live turn's prompt id so a turn-cancel
        // (`cancel_running_turn_subagents` → `cancel_by_parent_prompt_id`)
        // matches and TERMINATES them. Verification runs inside the turn's
        // `handle_prompt`, so `current_prompt_id` is still set here; it is
        // only cleared once the turn completes in `handle_completion`.
        let parent_prompt_id = self
            .current_prompt_id
            .lock()
            .expect("current_prompt_id mutex poisoned")
            .clone();
        // Record each skeptic as a synthetic `task` call into the harness trace
        // phase (never the live model context); the panel is sealed into its own
        // sibling trace turn below so the trace viewer can discover the skeptic subagents.
        let task_tool_name = self.resolve_goal_tool_names().await.task;

        // Resolve the per-index skeptic runtime overrides. `n` is the CLAMPED
        // skeptic count — identical to the fan-out site — so the assignment
        // never desyncs from the spawned indices.
        let n = self.goal_verifier_skeptic_count.clamp(
            crate::session::goal_classifier::GOAL_VERIFIER_SKEPTIC_MIN,
            crate::session::goal_classifier::GOAL_VERIFIER_SKEPTIC_MAX,
        );
        // Parent-toolset names for the inherit / fail-open render path,
        // resolved once for the panel (all-inherit indices share them).
        let inherit_tool_names = self.resolve_inherit_role_tool_names().await;
        let (skeptic_overrides, skeptic_tool_names): (
            Vec<crate::session::goal_planner::RoleSpawnOverride>,
            Vec<crate::session::goal_role_tools::RoleToolNames>,
        ) = if self.goal_use_current_model_only {
            // Kill-switch: every skeptic inherits, overriding any persisted
            // frozen `skeptic_model_assignment`. The frozen assignment is left
            // intact (not cleared) so turning the switch back off restores the
            // per-index models with their continuity.
            (
                vec![crate::session::goal_planner::RoleSpawnOverride::default(); n as usize],
                vec![inherit_tool_names.clone(); n as usize],
            )
        } else {
            // Freeze / extend the assignment under the lock, DROP the guard,
            // then resolve each index (the async `describe_subagent_type`
            // round-trip must not hold the goal_tracker lock).
            let assignment = {
                let mut tracker = self.goal_tracker.lock();
                match tracker.snapshot_mut() {
                    Some(o) => {
                        let expanded = crate::session::goal_classifier::expand_skeptic_assignment(
                            &o.skeptic_model_assignment,
                            &self.goal_role_models.skeptic_pool,
                            n as usize,
                        );
                        o.skeptic_model_assignment = expanded.clone();
                        expanded
                    }
                    None => Vec::new(),
                }
            };
            // Snapshot the model catalog ONCE for the panel (not per index)
            // and memoize the describe round-trip by agent_type across indices
            // (KD#14).
            let available_models = self.models_manager.models();
            let mut cache = PanelResolveCache::default();
            let mut overrides = Vec::with_capacity(n as usize);
            for idx in 0..n {
                let ov = match assignment.get(idx as usize) {
                    Some(pair) => {
                        self.resolve_goal_role_override(
                            "skeptic",
                            Some(idx),
                            pair,
                            RoleCapability::Skeptic,
                            &event_tx,
                            &available_models,
                            &mut cache,
                        )
                        .await
                    }
                    None => crate::session::goal_planner::RoleSpawnOverride::default(),
                };
                overrides.push(ov);
            }
            // Build the per-index tool names from the SAME cached describe
            // summaries (no second round-trip): an explicit committed index
            // renders its own toolset's names, every other index inherits.
            let tool_names = overrides
                .iter()
                .map(|ov| role_tool_names_from(ov, &cache, &inherit_tool_names))
                .collect();
            (overrides, tool_names)
        };

        let spawner: std::sync::Arc<dyn crate::session::goal_classifier::GoalClassifierSpawner> =
            std::sync::Arc::new(ChannelSpawner {
                event_tx,
                parent_session_id: self.session_id_string(),
                parent_prompt_id,
                cwd: Some(self.tool_context.cwd.as_str().to_owned()),
                trace_sink: Some((self.chat_state_handle.clone(), task_tool_name)),
                skeptic_overrides,
                events: Some(self.events.writer()),
            });

        // Goal-wide implementer scratch dir, threaded into every skeptic prompt.
        let implementer_scratch =
            crate::session::goal_tracker::implementer_scratch_dir(&verifier_id);

        let inputs = VerificationStageInputs {
            objective: &objective,
            final_response: &final_response,
            baseline_commit: baseline_commit.as_deref(),
            workspace_root: self.tool_context.cwd.as_path(),
            verifier_id: &verifier_id,
            attempt,
            model_id: &model_id,
            goal_created_at,
            plan_file: plan_file.as_deref(),
            plan_baseline_file: plan_baseline_file.as_deref(),
            implementer_scratch_dir: implementer_scratch.as_path(),
            scratch_dir_ready,
            skeptic_count: self.goal_verifier_skeptic_count,
            max_runs,
            prior_skeptic0_session_id: prior_skeptic0.as_deref(),
            prior_gaps: prior_gaps.as_deref(),
            tool_names: &skeptic_tool_names,
            inherit_tool_names: &inherit_tool_names,
        };

        let result = run_verification_stage(spawner, inputs, &|e| self.events.emit(e)).await;
        // Seal this panel's recorded skeptic `task` pairs into one harness trace
        // turn (one verifier turn per verification round). No-op on the
        // fail-open early-exit paths that spawn no skeptics.
        self.chat_state_handle.flush_harness_trace_turn();
        // Persist skeptic 0's child id only when the panel actually ran:
        // a fail-open early-exit must not sever the gatekeeper resume
        // chain. N == 1 runs clear it (id `None`); `GoalTracker::complete`
        // clears it on achieve.
        if result.panel_ran
            && let Some(o) = self.goal_tracker.lock().snapshot_mut()
        {
            o.skeptic0_session_id = result.skeptic0_session_id;
            // Freeze the breadth anchor only after the panel ran, so a
            // cancelled first verification leaves it uncaptured and the next
            // real round still claims it.
            if let Some(anchor) = anchor_to_persist {
                o.first_final_response = Some(anchor);
            }
        }
        result.outcome
    }

    /// Resolve a single-pair role (planner / strategist) from its
    /// `GoalRoleModelChoice`, returning both the spawn override and the
    /// prompt tool names for the role. `InheritCurrent` ⇒ the default (inherit)
    /// override + the parent-toolset names (no work, no telemetry); `Explicit`
    /// snapshots the model catalog once and delegates to
    /// [`Self::resolve_goal_role_override`] with a fresh per-call cache, then
    /// builds the tool names from the SAME cached `describe` summary (no second
    /// round-trip). Takes `choice` by reference (no clone).
    pub(crate) async fn resolve_goal_single_role_override(
        &self,
        role: &'static str,
        choice: &crate::agent::config::GoalRoleModelChoice,
        capability: RoleCapability,
        event_tx: &tokio::sync::mpsc::UnboundedSender<
            xai_grok_tools::implementations::grok_build::task::types::SubagentEvent,
        >,
    ) -> (
        crate::session::goal_planner::RoleSpawnOverride,
        crate::session::goal_role_tools::RoleToolNames,
        crate::session::goal_role_tools::RoleToolNames,
    ) {
        let inherit = self.resolve_inherit_role_tool_names().await;
        let crate::agent::config::GoalRoleModelChoice::Explicit(pair) = choice else {
            // Inherit path: the resolved names ARE the parent names. Clone once
            // so the caller gets both the prompt's `tool_names` and the
            // (unused-on-this-path) fail-open `inherit_tool_names`.
            return (
                crate::session::goal_planner::RoleSpawnOverride::default(),
                inherit.clone(),
                inherit,
            );
        };
        let available_models = self.models_manager.models();
        let mut cache = PanelResolveCache::default();
        let override_ = self
            .resolve_goal_role_override(
                role,
                None,
                pair,
                capability,
                event_tx,
                &available_models,
                &mut cache,
            )
            .await;
        let tool_names = role_tool_names_from(&override_, &cache, &inherit);
        (override_, tool_names, inherit)
    }

    /// Resolve one `/goal` role's runtime override from its configured
    /// `{model, agent_type}` `pair`, with the parent-side gates +
    /// per-role/per-index fail-open. ONE code path for every role and every
    /// skeptic index (no per-role copy-paste). The caller short-circuits the
    /// `InheritCurrent` case (no pair ⇒ no work / no telemetry).
    ///
    /// The pair is committed only when:
    /// 1. **Catalog membership** — the model is in the session's catalog
    ///    (`available_models`, passed by reference so the caller snapshots it
    ///    once per panel). Miss ⇒ `ModelUnknown`.
    /// 2. **Entitlement** — the session's `allowed_models` permits the model
    ///    (`ModelInfo.user_selectable`; a restricted session marks out-of-list
    ///    entries `false`, an unrestricted one marks all `true`). This is the
    ///    session-local entitlement boundary — no disk read, no `api_key`
    ///    probe (which would false-fail session-token auth). Miss ⇒
    ///    `ModelUnauthorized`.
    /// 3. **Harness resolves, allowed + capable** — `describe_subagent_type`,
    ///    given the role's fixed `general-purpose` subagent_type and the
    ///    configured `agent_type` as the HARNESS override, reports an allowed
    ///    toolset that satisfies the role's `capability` gate. An `agent_type`
    ///    that doesn't resolve to a harness ⇒ `ToolsetUnknown`; a resolved
    ///    harness whose role toolset can't satisfy the role ⇒
    ///    `ToolsetIncapable`.
    ///
    /// Any failure emits `GoalRoleModelFailOpen` with the matching reason and
    /// returns the inherit override (current model + session harness); success
    /// emits `GoalRoleModelResolved`. `cache` memoizes the describe round-trip
    /// per `agent_type` (harness) across a panel build.
    ///
    /// The `describe_subagent_type` round-trip is awaited here with NO
    /// `goal_tracker` lock held — callers snapshot/freeze any state first.
    pub(crate) async fn resolve_goal_role_override(
        &self,
        role: &'static str,
        skeptic_idx: Option<u32>,
        pair: &crate::util::config::GoalRoleModel,
        capability: RoleCapability,
        event_tx: &tokio::sync::mpsc::UnboundedSender<
            xai_grok_tools::implementations::grok_build::task::types::SubagentEvent,
        >,
        available_models: &indexmap::IndexMap<String, crate::agent::config::ModelEntry>,
        cache: &mut PanelResolveCache,
    ) -> crate::session::goal_planner::RoleSpawnOverride {
        use crate::session::events::{Event, GoalRoleModelFailOpenReason as Reason};
        use crate::session::goal_planner::RoleSpawnOverride;
        use xai_grok_tools::implementations::grok_build::task::backend::{
            ChannelBackend, SubagentBackend,
        };
        use xai_grok_tools::implementations::grok_build::task::types::SubagentDescribeOutcome;

        let fail_open = |reason: Reason| {
            self.emit_event(Event::GoalRoleModelFailOpen {
                role,
                skeptic_idx,
                reason: reason.as_const_str(),
            });
            RoleSpawnOverride::default()
        };

        // 1. Model present in the session's catalog (the provisioned set).
        let Some(entry) = crate::agent::config::find_model_by_id(available_models, &pair.model)
        else {
            return fail_open(Reason::ModelUnknown);
        };
        // 2. Entitlement: the session's `allowed_models` permits the model. A
        //    restricted session marks out-of-list catalog entries
        //    `user_selectable == false`; an unrestricted session marks all
        //    `true`. This is the session-local entitlement boundary (no disk
        //    read, no `api_key` probe that would false-fail session-token auth).
        if !entry.info.user_selectable {
            return fail_open(Reason::ModelUnauthorized);
        }
        // 2b. Reject a STRICT harness whose flavor isn't representable (e.g.
        //    `codex`): it resolves, but `resolve_subagent_toolset` would
        //    silently run grok-build flavor. Non-strict names (grok-build
        //    family) run the default flavor and pass; unresolvable names fall
        //    through to the describe `Unknown` arm below.
        if xai_grok_agent::config::is_strict_harness_agent_type(&pair.agent_type)
            && !crate::agent::subagent::subagent_harness_flavor_is_representable(&pair.agent_type)
        {
            return fail_open(Reason::HarnessFlavorUnsupported);
        }
        // 3. Harness resolves + toolset allowed + capable (coordinator
        //    round-trip; no lock held). The role spawns the fixed
        //    `general-purpose` subagent_type on the configured `agent_type`
        //    HARNESS, so we describe THAT pairing (not `agent_type` as a
        //    subagent type) — an unresolvable harness ⇒ `Unknown`. Memoized per
        //    agent_type (harness) for the panel build.
        if !cache.describe.contains_key(&pair.agent_type) {
            let backend = ChannelBackend::new(event_tx.clone());
            let parent_session_id = self.session_id_string();
            let outcome = backend
                .describe_subagent_type(
                    crate::session::goal_planner::GOAL_ROLE_SUBAGENT_TYPE,
                    Some(&pair.agent_type),
                    &parent_session_id,
                )
                .await;
            // Only memoize *definitive* outcomes. `Unavailable` is transient
            // (e.g. a parent-teardown race inside the describe round-trip);
            // caching it would let a single first-index failure poison every
            // later same-`agent_type` index, so we leave it uncached and fail
            // open for this index only — the next index re-describes.
            if matches!(outcome, SubagentDescribeOutcome::Unavailable) {
                return fail_open(Reason::ToolsetUnavailable);
            }
            cache.describe.insert(pair.agent_type.clone(), outcome);
        }
        let summary = match cache.describe.get(&pair.agent_type) {
            Some(SubagentDescribeOutcome::Ok(summary)) => summary,
            Some(SubagentDescribeOutcome::Unknown { .. }) => {
                return fail_open(Reason::ToolsetUnknown);
            }
            Some(SubagentDescribeOutcome::NotAllowed { .. }) => {
                return fail_open(Reason::ToolsetNotAllowed);
            }
            Some(SubagentDescribeOutcome::Disabled) => return fail_open(Reason::ToolsetDisabled),
            Some(SubagentDescribeOutcome::Unavailable) => {
                return fail_open(Reason::ToolsetUnavailable);
            }
            // `SubagentDescribeOutcome` is `#[non_exhaustive]`; a future
            // variant (or an impossible cache miss) fails open conservatively.
            _ => return fail_open(Reason::ToolsetUnavailable),
        };
        if !capability.is_satisfied(summary) {
            return fail_open(Reason::ToolsetIncapable);
        }

        // Commit the configured pair.
        self.emit_event(Event::GoalRoleModelResolved {
            role,
            skeptic_idx,
            model_id: pair.model.clone(),
            agent_type: pair.agent_type.clone(),
            source: "remote",
        });
        RoleSpawnOverride {
            model: Some(pair.model.clone()),
            agent_type: Some(pair.agent_type.clone()),
        }
    }

    /// Resolve tool names for goal-mode prompts via `tool_for_kind()`.
    ///
    /// Centralises the lookup so `setup_goal`, `maybe_queue_goal_continuation`,
    /// and `resume_goal` don't duplicate the same sequence of async calls.
    pub(super) async fn resolve_goal_tool_names(&self) -> GoalToolNames {
        use xai_grok_tools::types::tool::ToolKind;
        let bridge = self.agent.borrow().tool_bridge().clone();
        GoalToolNames {
            goal: bridge
                .tool_for_kind(ToolKind::GoalUpdate)
                .await
                .unwrap_or_else(|| "update_goal".into()),
            task: bridge
                .tool_for_kind(ToolKind::Task)
                .await
                .unwrap_or_else(|| "spawn_subagent".into()),
            // `ToolKind::Plan` is the todo-list tool kind in every namespace
            // (tool names can differ by active template), so this
            // names the tool the active harness exposes. Mirrors
            // `TodoNudgeReminder`'s resolution.
            todo: bridge
                .tool_for_kind(ToolKind::Plan)
                .await
                .unwrap_or_else(|| "todo_write".into()),
        }
    }

    /// Resolve the PARENT toolset's client-facing names for the role-prompt
    /// placeholders (read/list/search/write/execute/web-search/web-fetch),
    /// mirroring [`Self::resolve_goal_tool_names`] but for the kinds a role
    /// prompt names.
    /// Used on the inherit / fail-open path, where the role runs on the parent
    /// toolset; [`RoleToolNames::from_parent`] applies the literal fallback for
    /// any kind the bridge lacks, and resolves `{WRITE_TOOL}` from the parent
    /// `Edit` tool when the bridge has no `Write` (so the inherit / retry render
    /// agrees with `from_summary` — `search_replace` on the default grok-build
    /// host, not the literal `write`). The `{TOOLSET_TOOLS}` block is empty on
    /// this path (no per-role toolset enumeration); on the default grok-build
    /// host the bridge resolves the parent's real tool ids (`read_file`, …).
    pub(crate) async fn resolve_inherit_role_tool_names(
        &self,
    ) -> crate::session::goal_role_tools::RoleToolNames {
        use xai_grok_tools::types::tool::ToolKind;
        let bridge = self.agent.borrow().tool_bridge().clone();
        crate::session::goal_role_tools::RoleToolNames::from_parent(
            bridge.tool_for_kind(ToolKind::Read).await,
            bridge.tool_for_kind(ToolKind::ListDir).await,
            bridge.tool_for_kind(ToolKind::Search).await,
            bridge.tool_for_kind(ToolKind::Write).await,
            bridge.tool_for_kind(ToolKind::Edit).await,
            bridge.tool_for_kind(ToolKind::Execute).await,
            bridge.tool_for_kind(ToolKind::WebSearch).await,
            bridge.tool_for_kind(ToolKind::WebFetch).await,
        )
    }

    /// Set up goal state and return the orchestrator system reminder.
    ///
    /// Called from `handle_prompt` when `/goal <objective>` is resolved. Unlike
    /// other built-in slash commands this does NOT end the turn — the prompt
    /// continues to model inference so the user sees one seamless turn instead
    /// of "Worked for 0.0s" followed by a continuation.
    ///
    /// Returns the system-reminder text so the caller can prepend it to the
    /// prompt blocks, keeping the reminder and user objective in a single
    /// user message (matching the `/loop` re-entrant pattern).
    pub(super) async fn setup_goal(&self, objective: &str, token_budget: Option<i64>) -> String {
        let goal_id = uuid::Uuid::new_v4().to_string();
        let created_at = chrono::Utc::now().to_rfc3339();
        // Record the current session token total as baseline
        // so we can compute goal-specific usage later.
        let token_baseline = self.chat_state_handle.get_total_tokens().await as i64;
        let baseline_commit =
            crate::session::goal_classifier::capture_git_baseline(self.tool_context.cwd.as_path())
                .await;
        self.goal_tracker.lock().create_goal(
            goal_id,
            objective.to_owned(),
            token_budget,
            token_baseline,
            created_at,
            baseline_commit,
        );
        // Fresh goal — drop any goal-turn-origin task ids carried over from a
        // previous goal so the new goal's drain starts clean.
        self.goal_turn_task_ids.lock().clear();
        self.clear_pending_classifier_completions();
        // Streaks are per-goal; stale counts must not leak into a fresh one.
        self.goal_continuation_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.goal_blocked_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);

        // Emit GoalUpdated so the pager picks up the new goal.
        {
            let (tokens_used, finished_marginal) = self.goal_tokens(token_baseline);
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }

        // Planner fires before the orchestrator reminder; fail-closed
        // (failure pauses the goal).
        self.maybe_run_goal_planner(objective).await;

        let names = self.resolve_goal_tool_names().await;
        // Hold the lock across the render so the plan path is borrowed
        // from the snapshot without cloning.
        let planner_enabled = self.goal_planner_enabled;
        let body = {
            let tracker = self.goal_tracker.lock();
            let o = tracker
                .snapshot()
                .expect("create_goal must populate the orchestration snapshot");
            let plan_path = goal_reminder_plan_path(planner_enabled, o);
            let scratch_dir = crate::session::goal_tracker::implementer_scratch_dir(&o.verifier_id);
            render_goal_rules(
                objective,
                &names,
                "",
                "",
                plan_path,
                &scratch_dir.to_string_lossy(),
                o.scratch_dir_ready,
            )
        };
        format!("<system-reminder>\n{body}\nStart now.\n</system-reminder>\n\n")
    }

    /// Apply the `/goal resume` transition and build the continuation
    /// reminder.
    ///
    /// Called from `handle_prompt` when `/goal resume` is resolved. Like
    /// `setup_goal`, a successful resume/nudge does NOT end the turn:
    /// [`GoalResumeOutcome::Inference`] carries the system-reminder so the
    /// caller seeds it as the turn's user content and flows through to model
    /// inference, giving resume the same data collection and harness traces
    /// as the initial `/goal`. Terminal/no-op cases return
    /// [`GoalResumeOutcome::Message`] so the caller prints it and ends the
    /// turn.
    pub(super) async fn resume_goal(&self) -> GoalResumeOutcome {
        use crate::session::goal_tracker::GoalStatus;
        // Read status and apply the `*Paused -> Active` transition under a
        // single mutex acquisition. The Resumed arm captures `pause_message`
        // BEFORE calling `resume()` (which clears it) so the continuation
        // reminder can surface the prior block reason to the model.
        enum ResumeTransition {
            Resumed {
                previous_status: GoalStatus,
                previous_pause_message: Option<String>,
            },
            Nudge,
            AlreadyComplete,
            BudgetLimited,
            NoGoal,
        }
        let transition = {
            let mut tracker = self.goal_tracker.lock();
            match tracker.status() {
                Some(
                    GoalStatus::UserPaused
                    | GoalStatus::BackOffPaused
                    | GoalStatus::NoProgressPaused
                    | GoalStatus::InfraPaused
                    | GoalStatus::Blocked,
                ) => {
                    // Capture-before-clear: read status and pause_message
                    // first, then resume() drops them.
                    let previous_status = tracker.status().unwrap();
                    let previous_pause_message =
                        tracker.snapshot().and_then(|o| o.pause_message.clone());
                    tracker.resume();
                    ResumeTransition::Resumed {
                        previous_status,
                        previous_pause_message,
                    }
                }
                Some(GoalStatus::Active) => ResumeTransition::Nudge,
                Some(GoalStatus::Complete) => ResumeTransition::AlreadyComplete,
                Some(GoalStatus::BudgetLimited) => ResumeTransition::BudgetLimited,
                None => ResumeTransition::NoGoal,
            }
        };
        let was_resumed = matches!(transition, ResumeTransition::Resumed { .. });
        let (user_msg, previous_status, previous_pause_message) = match transition {
            ResumeTransition::Resumed {
                previous_status,
                previous_pause_message,
            } => ("Goal resumed.", previous_status, previous_pause_message),
            // Idempotent nudge — goal already active, just re-inject
            // the continuation reminder.
            ResumeTransition::Nudge => (
                "Goal nudged — refreshing context.",
                GoalStatus::Active,
                None,
            ),
            ResumeTransition::AlreadyComplete => {
                return GoalResumeOutcome::Message(
                    "Goal is already complete. Use /goal <objective> to start a new one."
                        .to_string(),
                );
            }
            ResumeTransition::BudgetLimited => {
                return GoalResumeOutcome::Message(
                    "Goal is budget-limited. Use /goal clear, then /goal <objective>.".to_string(),
                );
            }
            ResumeTransition::NoGoal => {
                return GoalResumeOutcome::Message(
                    "No goal set. Use /goal <objective> to start one.".to_string(),
                );
            }
        };

        // Reset back-off streak and blocked streak — explicit user
        // re-arming clears both.
        self.goal_continuation_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);
        self.goal_blocked_streak
            .store(0, std::sync::atomic::Ordering::Relaxed);

        // Retry-on-resume: re-fire the planner when a just-resumed goal
        // still has no plan, so "resume to retry" holds. On re-failure
        // the goal re-pauses with the canonical message.
        if was_resumed {
            let needs_retry = {
                let tracker = self.goal_tracker.lock();
                tracker
                    .snapshot()
                    .map(|o| o.status == GoalStatus::Active && o.plan_file.is_none())
                    .unwrap_or(false)
            };
            if needs_retry {
                let objective = self
                    .goal_tracker
                    .lock()
                    .snapshot()
                    .map(|o| o.objective.clone());
                if let Some(objective) = objective {
                    self.maybe_run_goal_planner(&objective).await;
                    // Planner re-failed and re-paused the goal: end the turn
                    // instead of flowing through to inference, which would run
                    // the model against a paused goal with a stale
                    // `Status: Active` reminder (the reminder below assumes Active).
                    if self.goal_tracker.lock().status() != Some(GoalStatus::Active) {
                        return GoalResumeOutcome::Message(
                            "Planning failed again; goal paused.".to_string(),
                        );
                    }
                }
            }
        }

        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);

        // Emit notification so the pager sees the resume / nudge.
        {
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }

        // Inject the full rules (same as setup_goal) since the model
        // may have lost context during the pause.
        let names = self.resolve_goal_tool_names().await;

        let block_recap = match (previous_status, previous_pause_message.as_deref()) {
            (GoalStatus::Blocked, Some(msg)) => format!(
                "Previous state: Blocked.\n\
                 Previous block reason: {msg}\n\
                 Re-evaluate whether the blocker has been addressed. If yes, continue \
                 working. If no, you may block again with an updated reason.\n\
                 \n"
            ),
            (GoalStatus::InfraPaused, Some(msg)) => format!(
                "Previous state: Paused (infrastructure error).\n\
                 Previous error: {msg}\n\
                 The prior turn failed due to infrastructure. Retry when the error \
                 condition is resolved.\n\
                 \n"
            ),
            (GoalStatus::InfraPaused, None) => "Previous state: Paused (infrastructure error).\n\
                 The prior turn failed due to infrastructure. Retry when the error \
                 condition is resolved.\n\
                 \n"
            .to_string(),
            _ => String::new(),
        };

        let planner_enabled = self.goal_planner_enabled;
        let reminder = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            tracker.snapshot().map(|o| {
                let elapsed = crate::session::goal_orchestrator::format_elapsed(o.elapsed_ms);
                let goal_state = format!(
                    "<goal-state>\nStatus: Active\nTokens: {tokens} | Elapsed: \
                     {elapsed}\n</goal-state>\n\n",
                    tokens = tokens_used,
                );
                let plan_path = goal_reminder_plan_path(planner_enabled, o);
                let scratch_dir =
                    crate::session::goal_tracker::implementer_scratch_dir(&o.verifier_id);
                let body = render_goal_rules(
                    &o.objective,
                    &names,
                    &block_recap,
                    &goal_state,
                    plan_path,
                    &scratch_dir.to_string_lossy(),
                    o.scratch_dir_ready,
                );
                format!("<system-reminder>\n{body}\nContinue working now.\n</system-reminder>")
            })
        };
        let Some(reminder) = reminder else {
            return GoalResumeOutcome::Message("Goal state is missing.".to_string());
        };
        GoalResumeOutcome::Inference {
            reminder,
            user_msg: user_msg.to_string(),
        }
    }

    /// If a goal is active, inject a continuation nudge so the model keeps
    /// working on it after the current turn completes.
    ///
    /// After the completion drain and the goal-active gate, the
    /// turn-final assistant text is matched against the
    /// [`goal_stop_detector`](super::super::goal_stop_detector) panel. On a
    /// hit (with pending todos) the nudge renders the bail-specific
    /// flavor and `Event::GoalPrematureStopDetected` is emitted on the
    /// queue path; otherwise the generic flavor renders. A
    /// verified-complete goal returns at the gate before detection, so
    /// it never receives a bail nudge.
    ///
    /// Idempotent on the push side: if `state.pending_inputs` already
    /// contains an `InputItem` whose origin is
    /// [`PromptOrigin::GoalSummary`](crate::session::PromptOrigin::GoalSummary),
    /// the function skips the push and returns without enqueuing a
    /// duplicate. The token-update and `GoalUpdated` notification still
    /// run on every call that doesn't early-return on an inactive goal
    /// — they reflect current accounting, not reminder identity, so
    /// running them ahead of the dedup gate is safe. Two call sites
    /// rely on this: the pre-`emit_turn_ended` queue in
    /// `handle_completion`'s `TurnOutcome::Completed` arm (which
    /// guarantees the reminder is part of pending state by the time the
    /// user-visible turn-ended event fires) and the post-completion
    /// safety net inside [`Self::handle_turn_end`].
    ///
    /// A pending `GoalClassifierNudge` also satisfies the idempotency
    /// gate: when a completion claim is classifier-rejected during the
    /// drain (goal stays Active, a classifier nudge queued), that nudge
    /// pre-empts the bail nudge — the early-return fires before the
    /// `GoalPrematureStopDetected` emit, so no bail event is recorded on
    /// that turn. The classifier nudge already forces continuation with
    /// the gap inlined, so this precedence is intentional.
    /// Remove every prior goal-continuation directive from the persisted
    /// conversation, keeping only the copy about to be pushed. The
    /// directive re-embeds the full objective each turn, so without this
    /// a copy accumulates per turn and bloats context until compaction.
    /// No-op (no replace) when none are present.
    ///
    /// Matches only the harness's own synthetic injection (a `User` item
    /// tagged [`SyntheticReason::GoalSummary`] carrying
    /// [`GOAL_CONTINUATION_SENTINEL`]) — substring-anywhere matching
    /// would let model-authored text quoting the sentinel erase
    /// arbitrary history items. Untagged legacy copies are not matched
    /// and persist until compaction (safe direction of failure).
    pub(super) async fn prune_prior_goal_continuation_directives(&self) {
        use xai_grok_sampling_types::conversation::SyntheticReason;

        fn is_goal_continuation_directive(item: &ConversationItem) -> bool {
            matches!(
                item,
                ConversationItem::User(u)
                    if u.synthetic_reason == Some(SyntheticReason::GoalSummary)
            ) && item.text_content().contains(GOAL_CONTINUATION_SENTINEL)
        }
        let conv = self.chat_state_handle.get_conversation().await;
        if !conv.iter().any(is_goal_continuation_directive) {
            return;
        }
        let kept: Vec<ConversationItem> = conv
            .into_iter()
            .filter(|item| !is_goal_continuation_directive(item))
            .collect();
        self.chat_state_handle.replace_conversation(kept);
    }

    /// Enforce the goal token budget at a turn end. If the ratcheted goal
    /// spend (parent + subagents) has reached the configured `--budget`,
    /// record the `BudgetExceeded` history entry, trip the goal to
    /// `BudgetLimited`, emit the `GoalUpdated`, and notify the user; returns
    /// `true` so the caller stops continuation. Returns `false` when no
    /// budget is set or spend is still under it.
    ///
    /// Must run on EVERY goal turn end — success (via
    /// [`Self::prepare_goal_continuation`]) AND failure / cancel (via
    /// [`Self::handle_turn_end`]). Otherwise spend that crossed the cap during
    /// a failed or cancelled turn would leave the goal Active until a later
    /// successful turn end. `goal_tokens` is idempotent for a fixed
    /// `current_tokens`, so the success-path double call (here + the caller's
    /// own read) does not double-count.
    pub(super) async fn enforce_goal_token_budget(&self, current_tokens: i64) -> bool {
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let Some(budget) = self.goal_tracker.lock().token_budget() else {
            return false;
        };
        if tokens_used < budget {
            return false;
        }
        // `budget_limit` records the `BudgetExceeded` history entry itself
        // (via the tracker chokepoint), so we don't append it here.
        self.goal_tracker.lock().budget_limit();
        // Deferred completions must not fire against a now-budget-limited goal.
        self.clear_pending_classifier_completions();
        let notify = self.goal_notify_sender();
        notify.emit_goal_updated(
            &mut self.goal_tracker.lock(),
            tokens_used,
            finished_marginal,
        );
        self.send_slash_command_output(&format!(
            "Goal token budget reached ({tokens_used} of {budget} tokens) — goal \
             stopped. Use /goal clear, then /goal <objective> to start a new one."
        ))
        .await;
        true
    }

    /// Run the turn-end goal drain (verification) and, when the goal is still
    /// Active afterwards, build the continuation directive. Does the drain +
    /// post-verdict `GoalUpdated` notification + directive rendering; does NOT
    /// emit the premature-stop signal (the caller emits it when it actually
    /// continues, so a no-op re-check can't double-fire). Returns `None` when
    /// the goal resolved (achieved / paused / blocked) or this isn't a goal
    /// turn. Shared by the in-turn loop ([`Self::run_goal_round_end`]) and the
    /// legacy queue path ([`Self::maybe_queue_goal_continuation`]).
    async fn prepare_goal_continuation(&self, current_tokens: i64) -> Option<GoalContinuationPlan> {
        // Turn-end drain: classifier-eligible completions fire the verifier
        // here (deferred completions from the prior mid-turn drain first).
        self.drain_goal_updates(current_tokens, DrainPurpose::TurnEnd)
            .await;

        let goal_active = laziness_injection_active(
            self.goal_harness_enabled(),
            self.goal_tracker.lock().status(),
        );
        if !goal_active {
            return None;
        }

        // Stop-detection seam (post-drain, post-gate). Short-circuit: the
        // cheap chat-state read runs first, so the tool-bridge todo round-trip
        // is only paid when the text bails.
        let stop_pattern = match self.assistant_text_signals_premature_stop().await {
            Some(pattern) if self.has_pending_goal_todos().await => Some(pattern),
            _ => None,
        };

        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);

        // Budget enforcement is factored into `enforce_goal_token_budget` so
        // the failed / cancelled turn-end path (`handle_turn_end`) trips the
        // same cap. Idempotent re-read of `goal_tokens` (see its doc).
        if self.enforce_goal_token_budget(current_tokens).await {
            return None;
        }

        // Emit a GoalUpdated notification so the pager reflects the new state.
        {
            let notify = self.goal_notify_sender();
            notify.emit_goal_updated(
                &mut self.goal_tracker.lock(),
                tokens_used,
                finished_marginal,
            );
        }

        let names = self.resolve_goal_tool_names().await;
        let goal_tool = &names.goal;
        let todo_tool = &names.todo;

        let planner_enabled = self.goal_planner_enabled;
        let (
            objective,
            elapsed,
            plan_pointer,
            verifier_gaps,
            strategist_note,
            strategy_rec,
            plan_path,
            scratch_dir,
            scratch_ready,
            rounds_since_verify,
            refuted,
        ) = {
            let mut tracker = self.goal_tracker.lock();
            tracker.account_elapsed();
            let o = tracker.snapshot_mut()?;
            // Count this worker round for the re-verify escalation.
            o.rounds_since_verify = o.rounds_since_verify.saturating_add(1);
            let rounds_since_verify = o.rounds_since_verify;
            let refuted = o.consecutive_not_achieved >= 1;
            let elapsed = crate::session::goal_orchestrator::format_elapsed(o.elapsed_ms);
            let scratch_dir = crate::session::goal_tracker::implementer_scratch_dir(&o.verifier_id)
                .to_string_lossy()
                .into_owned();
            // Resolve the plan path once and derive both the rendered
            // pointer line and the owned PathBuf from the same value.
            let plan_path = goal_reminder_plan_path(planner_enabled, o).map(Path::to_path_buf);
            let plan_pointer = plan_path
                .as_deref()
                .map(|p| format!("Plan: {}\n\n", p.display()))
                .unwrap_or_default();
            // Replay the freshest verifier gaps every round (persisted on
            // the orchestration, not a one-shot) so the model keeps the
            // skeptic panel's findings until a later verdict overwrites them.
            let verifier_gaps = o
                .last_classifier_gaps
                .as_deref()
                .map(|gaps| render_verifier_gaps_block(gaps, goal_tool))
                .unwrap_or_default();
            // One-shot, unlike verifier_gaps: cloned out so the caller can
            // compare-and-clear (`consume_strategist_note`) only once a
            // directive embedding it is committed for delivery — a
            // gate-dropped directive retries the advice next round.
            let strategy_rec = o.last_strategy_recommendation.clone();
            let strategist_note = strategy_rec
                .as_deref()
                .map(|rec| {
                    render_strategist_note(
                        rec,
                        plan_path.as_deref(),
                        o.last_strategy_path.as_deref(),
                    )
                })
                .unwrap_or_default();
            (
                o.objective.clone(),
                elapsed,
                plan_pointer,
                verifier_gaps,
                strategist_note,
                strategy_rec,
                plan_path,
                scratch_dir,
                o.scratch_dir_ready,
                rounds_since_verify,
                refuted,
            )
        };
        let next_step = resolve_goal_next_step(plan_path.as_deref())
            .unwrap_or_else(|| format!("Check your `{todo_tool}` list for next steps."));
        let tokens = u64::try_from(tokens_used).unwrap_or(0);
        let bail_preface = if stop_pattern.is_some() {
            GOAL_CONTINUATION_BAIL_PREFACE
        } else {
            ""
        };
        // Empty until a refuted goal has churned past the threshold.
        let reverify_block = render_goal_reverify_block(
            rounds_since_verify,
            refuted,
            self.goal_reverify_after,
            goal_tool,
        );
        let directive = render_goal_continuation_directive(
            &objective,
            tokens,
            &elapsed,
            bail_preface,
            &plan_pointer,
            &verifier_gaps,
            &strategist_note,
            &reverify_block,
            &next_step,
            todo_tool,
            goal_tool,
            &scratch_dir,
            scratch_ready,
        );
        Some(GoalContinuationPlan {
            directive,
            stop_pattern,
            strategy_rec,
        })
    }

    /// Record the premature-stop in goal history (so it reaches the pager's
    /// Recent History via `last_event`) AND emit the telemetry event. Shared
    /// by the in-turn loop and the legacy queue path so both always fire
    /// together.
    fn record_and_emit_premature_stop(&self, pattern: &'static str) {
        self.goal_tracker.lock().append_history(
            crate::session::goal_tracker::GoalHistoryEntry::now(
                crate::session::goal_tracker::GoalEvent::PrematureStopDetected,
                Some(pattern.to_owned()),
            ),
        );
        self.emit_event(crate::session::events::Event::GoalPrematureStopDetected { pattern });
    }

    /// Compare-and-clear the one-shot strategist note once a directive
    /// embedding it is committed for delivery: a newer recommendation
    /// written between render and commit is never-delivered advice and
    /// must survive for the next round.
    pub(super) fn consume_strategist_note(&self, delivered_rec: &str) {
        if let Some(o) = self.goal_tracker.lock().snapshot_mut()
            && o.last_strategy_recommendation.as_deref() == Some(delivered_rec)
        {
            o.last_strategy_recommendation = None;
            o.last_strategy_path = None;
        }
    }

    /// In-turn goal loop step: run verification for the round just completed
    /// and decide whether to continue the loop in-turn (with the continuation
    /// directive) or end the turn. The premature-stop signal, if any, is
    /// emitted here — once per continued round.
    pub(super) async fn run_goal_round_end(&self) -> GoalRoundDecision {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let Some(plan) = self.prepare_goal_continuation(current_tokens).await else {
            return GoalRoundDecision::EndTurn;
        };
        // A Continue directive is unconditionally injected — returning it
        // commits the embedded strategist note for delivery.
        if let Some(rec) = plan.strategy_rec.as_deref() {
            self.consume_strategist_note(rec);
        }
        if let Some(pattern) = plan.stop_pattern {
            self.record_and_emit_premature_stop(pattern);
        }
        GoalRoundDecision::Continue(plan.directive)
    }

    /// Inject the continuation directive into the conversation as the next
    /// user turn for the in-turn goal loop. Prunes prior directives so only
    /// the latest copy stays in history (the directive re-embeds the full
    /// objective each round).
    pub(super) async fn inject_goal_continuation_message(&self, directive: String) {
        self.prune_prior_goal_continuation_directives().await;
        self.chat_state_handle
            .push_user_message(ConversationItem::goal_summary(directive));
    }

    /// Legacy multi-turn path: queue the continuation directive as a fresh
    /// `GoalSummary` turn. Retained for the event-loop `handle_turn_end` safety
    /// net (which no longer reaches it once the in-turn loop owns continuation)
    /// and the goal test-suite. The in-turn loop uses
    /// [`Self::run_goal_round_end`] + [`Self::inject_goal_continuation_message`]
    /// instead.
    pub(super) async fn maybe_queue_goal_continuation(&self) {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        let Some(plan) = self.prepare_goal_continuation(current_tokens).await else {
            return;
        };
        // Peek the idempotency guard before the (awaiting) prune so a
        // skipped no-op turn does no work; the push block below re-checks
        // under the lock as the authoritative guard.
        {
            let state = self.state.lock().await;
            if state.pending_inputs.iter().any(|i| {
                matches!(
                    i.origin,
                    super::super::PromptOrigin::GoalSummary
                        | super::super::PromptOrigin::GoalClassifierNudge,
                )
            }) {
                return;
            }
        }
        // Keep only the latest continuation directive in history.
        self.prune_prior_goal_continuation_directives().await;

        let prompt_id = format!("goal-summary-{}", uuid::Uuid::now_v7());
        // Receiver intentionally dropped — goal continuation turns have no
        // caller awaiting the result.
        let (respond_to, _) = tokio::sync::oneshot::channel();
        {
            let mut state = self.state.lock().await;
            // Idempotency: if a continuation reminder is already queued for
            // this goal, skip the push.
            if state.pending_inputs.iter().any(|i| {
                matches!(
                    i.origin,
                    super::super::PromptOrigin::GoalSummary
                        | super::super::PromptOrigin::GoalClassifierNudge,
                )
            }) {
                tracing::debug!("continuation reminder already pending; skipping duplicate");
                return;
            }
            state.pending_inputs.push_back(InputItem {
                prompt_id,
                prompt_blocks: vec![acp::ContentBlock::Text(acp::TextContent::new(
                    plan.directive,
                ))],
                prompt_mode: crate::session::plan_mode::PromptMode::Agent,
                trace_gcs_config: None,
                artifact_tracker: None,
                client_identifier: None,
                screen_mode: None,
                verbatim: true,
                json_schema: None,
                origin: super::super::PromptOrigin::GoalSummary,
                task_wake_fallback: None,
                respond_to,
                persist_ack: None,
                parsed_prompt_tx: None,
                queue_meta: None,
                send_now: false,
            });
        }
        // Past the authoritative gate the queued directive will be delivered;
        // a gate-dropped one leaves the note for the next round.
        if let Some(rec) = plan.strategy_rec.as_deref() {
            self.consume_strategist_note(rec);
        }
        // Emit after the idempotency push so the signal fires once per turn,
        // never on the already-pending no-op.
        if let Some(pattern) = plan.stop_pattern {
            self.record_and_emit_premature_stop(pattern);
        }
    }

    /// Drop all deferred mid-turn completions. Called on every transition
    /// that ends or pauses the goal's active run (and on fresh-goal setup)
    /// so a stale `completed: true` claim can't replay the verifier — and
    /// burn a cap attempt — against work it no longer describes. Safe to
    /// drop outright: each entry's ack was already resolved at defer time.
    pub(crate) fn clear_pending_classifier_completions(&self) {
        self.pending_classifier_completions.lock().clear();
    }

    /// Auto-pause an `Active` goal with the supplied reason. No-op if the
    /// goal is not currently `Active` (already paused, complete, or absent).
    ///
    /// Called from four sites:
    /// - `SessionCommand::Cancel` on Ctrl+C — `GoalPauseReason::User`
    /// - The event-loop completion handler on doom-loop terminate —
    ///   `GoalPauseReason::User`
    /// - The event-loop completion handler on infra-classified turn `Err` —
    ///   `GoalPauseReason::Infra`
    /// - `handle_turn_end` when the consecutive-failed-turn streak hits the
    ///   back-off threshold — `GoalPauseReason::BackOff`
    ///
    /// Snapshots `tokens_used` before transitioning so the pager retains
    /// an accurate token count.
    pub(crate) async fn auto_pause_goal_if_active(
        &self,
        reason: crate::session::goal_tracker::GoalPauseReason,
    ) {
        let _ = self.auto_pause_goal_if_active_inner(reason, None).await;
    }

    /// Variant of [`Self::auto_pause_goal_if_active`] that additionally
    /// stashes a human-readable `message` on the orchestration's
    /// `pause_message` field. Used by [`Self::drain_goal_updates`] when
    /// the model calls `update_goal(blocked_reason: ...)` so the message
    /// survives until the next status transition.
    ///
    /// Returns `true` iff the transition was applied — `false` when the
    /// goal was not `Active` (already paused, complete, budget-limited,
    /// or absent). Callers that emit user-visible side effects (chat
    /// notification, telemetry) MUST gate on this return value so a
    /// stray block signal against a non-Active goal can't surface as a
    /// "Goal paused" message with no actual state change behind it.
    pub(crate) async fn auto_pause_goal_if_active_with_message(
        &self,
        reason: crate::session::goal_tracker::GoalPauseReason,
        message: String,
    ) -> bool {
        self.auto_pause_goal_if_active_inner(reason, Some(message))
            .await
    }

    /// Shared body of the auto-pause helpers. Releases the tracker lock
    /// before any subsequent `.await` (the notification re-acquires the
    /// lock inside `emit_goal_updated`). Returns `true` iff the pause
    /// transition was applied (the goal was `Active`).
    async fn auto_pause_goal_if_active_inner(
        &self,
        reason: crate::session::goal_tracker::GoalPauseReason,
        message: Option<String>,
    ) -> bool {
        let current_tokens = self.chat_state_handle.get_total_tokens().await as i64;
        {
            let mut tracker = self.goal_tracker.lock();
            if tracker.status() != Some(crate::session::goal_tracker::GoalStatus::Active) {
                return false;
            }
            // The early-return above guarantees `Active`, so the pause
            // transition always succeeds here.
            match message {
                Some(msg) => tracker.pause_with_message(reason, msg),
                None => tracker.pause(reason),
            };
        }
        self.clear_pending_classifier_completions();
        let (tokens_used, finished_marginal) = self.goal_tokens(current_tokens);
        let notify = self.goal_notify_sender();
        notify.emit_goal_updated(
            &mut self.goal_tracker.lock(),
            tokens_used,
            finished_marginal,
        );
        self.emit_event(crate::session::events::Event::GoalAutoPaused {
            reason: reason.into(),
        });
        true
    }

    /// Match the last assistant message text (via
    /// `chat_state_handle.get_last_assistant_text`) against the
    /// stop/bail/hand-off regexes in
    /// [`goal_stop_detector`](super::super::goal_stop_detector). Returns
    /// the matched pattern label, or `None` when the text does not
    /// bail — including the dominant production case of ordinary
    /// progress narration, plus the rarer no-assistant-text and
    /// unreachable-actor paths.
    async fn assistant_text_signals_premature_stop(&self) -> Option<&'static str> {
        let text = self.chat_state_handle.get_last_assistant_text().await?;
        crate::session::goal_stop_detector::matched_stop_pattern(&text)
    }

    /// True iff the live todo tool surface has at least one Pending or
    /// InProgress entry. Gates the stop-detector inside
    /// `maybe_queue_goal_continuation`: bail text only selects the
    /// bail-specific nudge flavor when the model still has declared
    /// work outstanding.
    ///
    /// Fails OPEN: when the tool bridge cannot read the `TodoState`
    /// resource (resource not yet plumbed, bridge transiently
    /// unavailable, etc.) this returns `true` and emits a
    /// `tracing::warn!`, so a momentarily-unreadable bridge cannot
    /// suppress the bail flavor. The safety-net intent of the stop-
    /// detector wins over a precise pending-todo count.
    async fn has_pending_goal_todos(&self) -> bool {
        use crate::tools::todo::{TodoState, TodoStatus};
        use xai_grok_tools::types::resources::State;
        let bridge = self.tool_bridge_handle();
        match bridge.read_resource::<State<TodoState>>().await {
            Some(state) => state.0.todo_items_with_ids().any(|(_id, item)| {
                matches!(item.status, TodoStatus::Pending | TodoStatus::InProgress)
            }),
            None => {
                tracing::warn!(
                    "goal stop-detector: TodoState resource missing; failing OPEN \
                     so the bail-text safety net still queues the continuation",
                );
                true
            }
        }
    }
}

#[cfg(test)]
mod role_capability_tests {
    use super::RoleCapability;
    use xai_grok_tools::implementations::grok_build::task::types::SubagentTypeSummary;

    fn summary(can_read: bool, can_search: bool, can_execute: bool) -> SubagentTypeSummary {
        SubagentTypeSummary {
            can_read,
            can_search,
            can_execute,
            ..Default::default()
        }
    }

    #[test]
    fn skeptic_requires_read_and_search() {
        assert!(RoleCapability::Skeptic.is_satisfied(&summary(true, true, false)));
        assert!(!RoleCapability::Skeptic.is_satisfied(&summary(true, false, false)));
        assert!(!RoleCapability::Skeptic.is_satisfied(&summary(false, true, false)));
    }

    #[test]
    fn strategist_requires_read_search_execute() {
        assert!(RoleCapability::Strategist.is_satisfied(&summary(true, true, true)));
        assert!(!RoleCapability::Strategist.is_satisfied(&summary(true, true, false)));
        assert!(!RoleCapability::Strategist.is_satisfied(&summary(true, false, true)));
    }
}

#[cfg(test)]
mod role_tool_names_tests {
    use super::{PanelResolveCache, role_tool_names_from};
    use crate::session::goal_planner::RoleSpawnOverride;
    use crate::session::goal_role_tools::RoleToolNames;
    use xai_grok_tools::implementations::grok_build::task::types::{
        SubagentDescribeOutcome, SubagentTypeSummary,
    };
    use xai_grok_tools::types::tool::ToolKind;

    /// A named summary (distinct from the inherit/default names) so the
    /// from_summary-vs-inherit dispatch is unambiguous.
    fn cursor_summary() -> SubagentTypeSummary {
        let mut s = SubagentTypeSummary {
            can_read: true,
            can_search: true,
            ..Default::default()
        };
        s.tool_names.insert(ToolKind::Read, "cursor_read".into());
        s.tool_names.insert(ToolKind::Search, "cursor_grep".into());
        s
    }

    /// A distinguishable parent-inherit names value (not the literal defaults).
    fn inherit_names() -> RoleToolNames {
        RoleToolNames::from_parent(
            Some("parent_read".into()),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
        )
    }

    #[test]
    fn committed_explicit_draws_from_the_cached_describe_summary() {
        let mut cache = PanelResolveCache::default();
        cache.describe.insert(
            "cursor-type".into(),
            SubagentDescribeOutcome::Ok(cursor_summary()),
        );
        let ov = RoleSpawnOverride {
            model: Some("m".into()),
            agent_type: Some("cursor-type".into()),
        };
        let tn = role_tool_names_from(&ov, &cache, &inherit_names());
        assert_eq!(tn.read, "cursor_read", "explicit pair uses its own toolset");
        assert_eq!(tn.search, "cursor_grep");
        assert!(
            tn.toolset_tools.contains("cursor_read"),
            "explicit path enumerates {{TOOLSET_TOOLS}}",
        );
    }

    #[test]
    fn inherit_override_uses_parent_names() {
        let tn = role_tool_names_from(
            &RoleSpawnOverride::default(),
            &PanelResolveCache::default(),
            &inherit_names(),
        );
        assert_eq!(tn.read, "parent_read", "inherit uses the parent toolset");
        assert_eq!(tn.toolset_tools, "", "inherit carries no toolset block");
    }

    #[test]
    fn explicit_but_summary_absent_or_unavailable_falls_back_to_inherit() {
        // (a) agent_type set but no cache entry (never described).
        let ov = RoleSpawnOverride {
            model: Some("m".into()),
            agent_type: Some("missing".into()),
        };
        let tn = role_tool_names_from(&ov, &PanelResolveCache::default(), &inherit_names());
        assert_eq!(tn.read, "parent_read", "absent summary ⇒ inherit");

        // (b) agent_type set but the describe round-trip failed open
        // (`Unavailable`) ⇒ inherit, never a partial/broken summary.
        let mut cache = PanelResolveCache::default();
        cache
            .describe
            .insert("x".into(), SubagentDescribeOutcome::Unavailable);
        let ov = RoleSpawnOverride {
            model: Some("m".into()),
            agent_type: Some("x".into()),
        };
        let tn = role_tool_names_from(&ov, &cache, &inherit_names());
        assert_eq!(tn.read, "parent_read", "fail-open describe ⇒ inherit");
    }
}
