use xai_grok_pager::scrollback::block::RenderBlock;
use xai_grok_pager::scrollback::entry::ScrollbackEntry;
use xai_grok_pager::scrollback::state::ScrollbackState;
use xai_grok_pager_minimal::commit::is_committable;

#[test]
fn interleaved_thinking_does_not_close_a_streaming_agent_message() {
    let mut state = ScrollbackState::new();
    let message = state.push(ScrollbackEntry::running(RenderBlock::agent_message(
        "The user",
    )));
    state.push(ScrollbackEntry::running(RenderBlock::thinking_streaming()));

    assert!(!is_committable(&state, 0, true));
    assert!(state.push_chunk_to_agent(message, " asked me to wait."));
    assert!(!is_committable(&state, 0, true));

    state.push(ScrollbackEntry::running(RenderBlock::execute("true")));
    assert!(is_committable(&state, 0, true));
}
