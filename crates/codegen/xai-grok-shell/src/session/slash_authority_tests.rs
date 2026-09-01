use super::*;
use crate::session::slash_commands::BUILTIN_COMMANDS;

fn text_block(text: &str) -> acp::ContentBlock {
    acp::ContentBlock::Text(acp::TextContent::new(text))
}

#[test]
fn agent_and_session_bus_origins_cannot_gain_human_command_authority() {
    for origin in [
        crate::session::PromptOrigin::ParentAgentMessage {
            message_id: "parent-message".into(),
            sender_session_id: "parent-session".into(),
        },
        crate::session::PromptOrigin::AgentMessage {
            message_id: "team-message".into(),
        },
        crate::session::PromptOrigin::PeerSessionMessage {
            message_id: "peer-message".into(),
        },
    ] {
        assert_eq!(
            origin.policy().authority,
            InputAuthority::ModelAuthoredUntrusted
        );
        for command in ["/always-approve on", "/yolo", "/swarm", "/hooks-trust"] {
            assert!(matches!(
                resolve(
                    origin.policy().authority,
                    &[text_block(command)],
                    BUILTIN_COMMANDS
                ),
                AuthorityResolution::ModelAuthoredSkillCandidate { .. }
            ));
        }
    }
}

#[test]
fn eligible_builtin_requires_an_always_on_gate() {
    let gated = BuiltinCommand {
        name: "compact",
        description: "gated test compact",
        argument_hint: None,
        aliases: &[],
        gate: super::super::slash_commands::BuiltinGate::Memory,
        model_authored_eligibility: ModelAuthoredEligibility::ExactCanonical,
        resolve: |_| BuiltinAction::Compact { user_context: None },
    };
    assert!(matches!(
        resolve(
            InputAuthority::ModelAuthoredUntrusted,
            &[text_block("/compact")],
            &[gated]
        ),
        AuthorityResolution::ModelAuthoredSkillCandidate { .. }
    ));
}

#[test]
fn model_authored_resolution_uses_exact_canonical_metadata() {
    assert!(matches!(
        resolve(
            InputAuthority::ModelAuthoredUntrusted,
            &[text_block("/compact preserve auth")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::StaticBuiltin(BuiltinAction::Compact {
            user_context: Some(context),
        }) if context == "preserve auth"
    ));

    for text in ["/Compact", "/COMPACT", "/yolo", "/context", "/feedback"] {
        assert!(matches!(
            resolve(
                InputAuthority::ModelAuthoredUntrusted,
                &[text_block(text)],
                BUILTIN_COMMANDS,
            ),
            AuthorityResolution::ModelAuthoredSkillCandidate { .. }
        ));
    }
}

#[test]
fn runtime_control_is_inert_and_human_intent_continues_to_dynamic_resolution() {
    assert!(matches!(
        resolve(
            InputAuthority::RuntimeControl,
            &[text_block("/compact")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::Inert
    ));
    assert!(matches!(
        resolve(
            InputAuthority::RuntimeControl,
            &[text_block("/available-skill")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::Inert
    ));
    assert!(matches!(
        resolve(
            InputAuthority::HumanIntent,
            &[text_block("/Compact keep aliases")],
            BUILTIN_COMMANDS,
        ),
        AuthorityResolution::HumanIntent {
            command_name: "Compact",
            args: "keep aliases",
        }
    ));
}
