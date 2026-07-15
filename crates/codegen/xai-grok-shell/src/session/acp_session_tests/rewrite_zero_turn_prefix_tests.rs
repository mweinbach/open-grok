use super::SessionActor;
use super::support::create_test_actor;
use xai_grok_sampling_types::{ConversationItem, SyntheticReason};
#[test]
fn rewrites_prefix_at_index_one_without_dropping_reminder() {
    let mut conv = vec![
        ConversationItem::system("SP"),
        ConversationItem::user("OLD_PREFIX"),
        ConversationItem::system_reminder("<system-reminder>\nskills\n</system-reminder>"),
    ];
    SessionActor::rewrite_zero_turn_prefix(&mut conv, "NEW_PREFIX".into(), false);
    assert_eq!(
        conv.len(),
        3,
        "rebuild keeps the reminder when the drop flag is off"
    );
    assert_eq!(conv[1].text_content(), "NEW_PREFIX");
    assert!(matches!(& conv[1], ConversationItem::User(u) if u.synthetic_reason.is_none()));
    assert!(
        matches!(& conv[2], ConversationItem::User(u) if u.synthetic_reason ==
        Some(SyntheticReason::SystemReminder))
    );
}
#[test]
fn inserts_prefix_when_no_user_at_index_one() {
    let mut conv = vec![ConversationItem::system("SP")];
    SessionActor::rewrite_zero_turn_prefix(&mut conv, "NEW_PREFIX".into(), false);
    assert_eq!(conv.len(), 2, "prefix inserted at index 1");
    assert!(matches!(& conv[0], ConversationItem::System(s) if s.content.as_ref() == "SP"));
    assert_eq!(conv[1].text_content(), "NEW_PREFIX");
}
#[test]
fn skips_synthetic_reminder_at_index_one() {
    let mut conv = vec![
        ConversationItem::system("SP"),
        ConversationItem::system_reminder("<system-reminder>\nskills\n</system-reminder>"),
    ];
    SessionActor::rewrite_zero_turn_prefix(&mut conv, "NEW_PREFIX".into(), false);
    assert_eq!(conv.len(), 3, "prefix inserted, reminder preserved");
    assert!(matches!(& conv[0], ConversationItem::System(s) if s.content.as_ref() == "SP"));
    assert_eq!(conv[1].text_content(), "NEW_PREFIX");
    assert!(
        matches!(& conv[2], ConversationItem::User(u) if u.synthetic_reason ==
        Some(SyntheticReason::SystemReminder))
    );
}
/// A mid-session agent rebuild (e.g. a model that forces a different
/// template) builds a fresh, empty ToolBridge. The rebuild must
/// re-register `GoalUpdateHandle`, otherwise `update_goal` fails with
/// "GoalUpdateHandle not registered" and the goal can never complete.
/// Drives the real `handle_rebuild_agent_for_definition` path.
#[tokio::test(flavor = "current_thread")]
async fn rebuild_reinjects_goal_update_handle() {
    use xai_grok_tools::implementations::grok_build::update_goal::{
        GoalUpdateHandle, UpdateGoalInput, envelope_for_test,
    };
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gw_tx, persist_tx).await;
            actor
                .handle_rebuild_agent_for_definition(
                    xai_grok_agent::AgentDefinition::default_grok_build(),
                )
                .await
                .expect("zero-turn rebuild should succeed");
            let bridge = actor.agent.borrow().tool_bridge().clone();
            let resources = bridge.shared_resources().await;
            let sender = {
                let guard = resources.lock().await;
                guard
                    .get::<GoalUpdateHandle>()
                    .expect(
                        "rebuilt bridge must carry GoalUpdateHandle so update_goal works after an \
                         agent rebuild",
                    )
                    .0
                    .clone()
            };
            sender
                .send(envelope_for_test(UpdateGoalInput {
                    completed: Some(true),
                    message: None,
                    blocked_reason: None,
                }))
                .expect("send through re-injected handle");
            let mut rx = actor
                .goal_update_rx
                .borrow_mut()
                .take()
                .expect("actor retains goal_update_rx");
            assert!(
                rx.try_recv().is_ok(),
                "re-injected GoalUpdateHandle must deliver to the actor's goal channel",
            );
        })
        .await;
}
/// The seeded skill used by the rebuild skill-reminder tests. A non-plugin
/// Local skill is always listable, so it renders into the grok markdown skill
/// catalog when the pending baseline is drained for a different agent.
fn regression_skill() -> xai_grok_tools::implementations::skills::types::SkillInfo {
    xai_grok_tools::implementations::skills::types::SkillInfo {
        name: "regression-baseline-skill".to_owned(),
        description: "Seeded skill for the rebuild reminder regression test.".to_owned(),
        path: "/tmp/skills/regression-baseline-skill/SKILL.md".to_owned(),
        ..Default::default()
    }
}
/// Seed the actor's live ToolBridge `SkillManager` with one skill so a baseline
/// change is pending, mirroring the fresh, seeded bridge a zero-turn agent
/// rebuild produces (its `build_agent` re-runs skill discovery and calls the
/// same `seed_skill_discovery`).
async fn seed_pending_baseline(actor: &SessionActor) {
    let bridge = actor.agent.borrow().tool_bridge().clone();
    bridge
        .seed_skill_discovery(
            Some(std::path::PathBuf::from("/tmp")),
            None,
            vec![regression_skill()],
            None,
            Some(256_000),
            None,
            xai_grok_tools::types::compat::CompatConfig::default(),
        )
        .await;
}
/// Count of synthetic `SystemReminder` user items -- the shape both
/// `rewrite_zero_turn_prefix` and `inject_baseline_skill_reminder` use to
/// identify the baseline skill reminder.
fn skill_reminder_count(conversation: &[ConversationItem]) -> usize {
    conversation
        .iter()
        .filter(|item| {
            matches!(
                item, ConversationItem::User(u) if u.synthetic_reason ==
                Some(SyntheticReason::SystemReminder)
            )
        })
        .count()
}
/// An inherited baseline skill reminder from the source session, with DISTINCT
/// (stale) content so tests can prove it was replaced, not merely kept.
fn stale_source_reminder() -> ConversationItem {
    ConversationItem::system_reminder(
        "<system-reminder>\nThe following skills are available for use:\n\n\
         - stale-source-skill: from the source session.\n</system-reminder>",
    )
}
/// Regression: a zero-turn agent rebuild INTO a grok/Default agent
/// must re-inject the baseline skill `<system-reminder>`. `initialize()`
/// is otherwise the only place skills are surfaced for the grok agent, so
/// before the fix a switch into such an agent — whose rebuilt bridge holds
/// a pending `BaselineChange` — dropped the skill listing for a no-tool first
/// turn. Drives the real `inject_baseline_skill_reminder` seam that
/// `handle_rebuild_agent_for_definition` calls; deleting the drain/inject makes
/// this fail.
#[tokio::test(flavor = "current_thread")]
async fn rebuild_reinjects_baseline_skill_reminder_for_non_cursor() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gw_tx, persist_tx).await;
            seed_pending_baseline(&actor).await;
            let mut conversation = vec![
                ConversationItem::system("SP"),
                ConversationItem::user("PREFIX"),
            ];
            actor
                .inject_baseline_skill_reminder(&mut conversation)
                .await;
            assert_eq!(
                conversation.len(),
                3,
                "non-cursor rebuild must append the baseline skill reminder",
            );
            let reminder = conversation.last().expect("conversation is non-empty");
            assert!(
                matches!(reminder, ConversationItem::User(u) if u.synthetic_reason ==
                Some(SyntheticReason::SystemReminder)),
                "appended item must be a system-reminder user message",
            );
            let text = reminder.text_content();
            assert!(
                text.contains("The following skills are available for use:"),
                "reminder must carry the grok skill catalog header:\n{text}",
            );
            assert!(
                text.contains("regression-baseline-skill"),
                "reminder must list the seeded skill:\n{text}",
            );
        })
        .await;
}
/// Idempotency / no-duplication: a reminder-using -> reminder-using zero-turn rebuild
/// inherits the source session's baseline skill `<system-reminder>` (which
/// `rewrite_zero_turn_prefix` keeps for a reminder-using target). The helper must
/// strip that stale reminder and inject exactly one fresh listing -- not append
/// a second catalog. Pins the double-listing bug: without the strip the count
/// is 2; without the inject the surviving reminder is the stale one.
#[tokio::test(flavor = "current_thread")]
async fn rebuild_injects_exactly_one_reminder_when_source_reminder_present() {
    let local = tokio::task::LocalSet::new();
    local
        .run_until(async {
            let (gw_tx, _gw_rx) = tokio::sync::mpsc::unbounded_channel();
            let (persist_tx, _persist_rx) = tokio::sync::mpsc::unbounded_channel();
            let actor = create_test_actor(0, 256_000, 85, gw_tx, persist_tx).await;
            seed_pending_baseline(&actor).await;
            let mut conversation = vec![
                ConversationItem::system("SP"),
                ConversationItem::user("PREFIX"),
                stale_source_reminder(),
            ];
            actor
                .inject_baseline_skill_reminder(&mut conversation)
                .await;
            assert_eq!(
                skill_reminder_count(&conversation),
                1,
                "exactly one baseline skill reminder must remain (no double-listing)",
            );
            let text = conversation
                .iter()
                .rev()
                .find_map(|item| match item {
                    ConversationItem::User(u)
                        if u.synthetic_reason == Some(SyntheticReason::SystemReminder) =>
                    {
                        Some(item.text_content())
                    }
                    _ => None,
                })
                .expect("a skill reminder remains");
            assert!(
                text.contains("regression-baseline-skill"),
                "the surviving reminder must be the freshly injected listing:\n{text}",
            );
            assert!(
                !text.contains("stale-source-skill"),
                "the inherited stale reminder must have been stripped:\n{text}",
            );
        })
        .await;
}
