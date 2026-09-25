#[allow(dead_code)]
mod acp_harness;

use acp_harness::assert_probes_emitted;

// Fork: upstream also probes `session.spawn_and_register.plugin_refresh`,
// but the fork has no prefetch join — plugins resolve via the agent-level
// lazy registry, not in the session spawn path.
const SUBPHASE_TIMERS: [&str; 5] = [
    "session.spawn_and_register.session_env",
    "session.spawn_actor.permission_setup",
    "session.spawn_actor.agent_build",
    "session.new_session.git_discovery",
    "session.new_session.tool_overrides_echo",
];

#[test]
fn session_create_emits_a_timer_for_each_serial_subphase() {
    assert_probes_emitted("session-create-subphase-timers", &SUBPHASE_TIMERS);
}
