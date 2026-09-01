use super::{AgentView, PromptInputMode, PromptMode};
use crate::app::app_view::InputOutcome;
use crate::views::prompt_widget::StashedPrompt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StashCause {
    Chord,
    ClearedDraft,
}

#[derive(Debug)]
pub struct PromptStashEntry {
    pub prompt: StashedPrompt,
    pub input_mode: PromptInputMode,
    pub cause: StashCause,
}

impl PromptStashEntry {
    pub(in crate::app) fn history_text(&self) -> String {
        prompt_history_text(&self.prompt.text, self.input_mode)
    }
}

pub(in crate::app) fn prompt_history_text(text: &str, input_mode: PromptInputMode) -> String {
    let text = text.trim();
    match input_mode {
        PromptInputMode::Bash if !text.is_empty() => format!("! {text}"),
        _ => text.to_owned(),
    }
}

const STASH_CAPTION: &str = "Stashed";

impl AgentView {
    pub(super) fn prompt_caption(&self) -> Option<String> {
        let title = self
            .display_name
            .as_deref()
            .map(|text| crate::views::session_title::sanitize_display_text(text).into_owned());

        match (self.prompt_stash.is_some(), title) {
            (true, Some(title)) => Some(format!("{STASH_CAPTION} · {title}")),
            (true, None) => Some(STASH_CAPTION.to_owned()),
            (false, title) => title,
        }
    }

    pub(in crate::app) fn stash_prompt_draft(&mut self, cause: StashCause) {
        if self.prompt.text().is_empty() && self.prompt.images.is_empty() {
            return;
        }

        let entry = PromptStashEntry {
            prompt: self.prompt.stash(),
            input_mode: self.prompt_input_mode,
            cause,
        };
        self.prompt.set_text("");
        self.prompt.clear_history();

        self.prompt_stash = Some(entry);

        self.note_stash_change_in_minimal(
            "Draft stashed. Press the stash key again to restore it.",
        );
    }

    pub(super) fn stash_draft_from_chord(&mut self) {
        if !matches!(self.prompt_mode, PromptMode::Normal) {
            return;
        }

        self.stash_prompt_draft(StashCause::Chord);
        self.prompt_input_mode = PromptInputMode::Normal;
    }

    pub(in crate::app) fn note_draft_consumed(&mut self) {
        self.draft_consumed = true;
    }

    pub(in crate::app) fn take_draft_consumed(&mut self) -> bool {
        std::mem::take(&mut self.draft_consumed)
    }

    pub(in crate::app) fn auto_restore_stash_after_send(&mut self) {
        if !self.composer_is_idle_and_empty() {
            return;
        }
        let Some(entry) = self
            .prompt_stash
            .take_if(|error| error.cause == StashCause::Chord)
        else {
            return;
        };

        self.restore_stash_entry(entry);
        self.note_stash_change_in_minimal("Stashed draft restored.");
    }

    pub(super) fn reclaim_stash_recalled_into_composer(&mut self) {
        let composer = prompt_history_text(self.prompt.text(), self.prompt_input_mode);
        let Some(entry) = self
            .prompt_stash
            .take_if(|error| error.history_text() == composer)
        else {
            return;
        };

        let cursor = self.prompt.cursor();
        self.prompt.restore(entry.prompt);
        self.prompt.set_cursor(cursor);
    }

    pub(in crate::app) fn record_prompt_in_history(&mut self, text: &str) {
        crate::app::agent::remember_prompt(&mut self.session.prompt_history, text);
    }

    pub(super) fn handle_stash_prompt_key(&mut self) -> InputOutcome {
        if !matches!(self.prompt_mode, PromptMode::Normal) {
            return InputOutcome::Unchanged;
        }

        if self.paste_probe_in_flight > 0 {
            self.deferred_send = Some(super::AgentDeferredSend::Stash);
            return InputOutcome::Changed;
        }

        if !self.prompt.text().is_empty() || !self.prompt.images.is_empty() {
            self.stash_draft_from_chord();
            return InputOutcome::Changed;
        }

        let Some(entry) = self.prompt_stash.take() else {
            return InputOutcome::Unchanged;
        };

        self.restore_stash_entry(entry);
        self.note_stash_change_in_minimal("Stashed draft restored.");
        InputOutcome::Changed
    }

    fn note_stash_change_in_minimal(&mut self, text: &str) {
        if self.is_minimal_mode() {
            self.scrollback
                .push_block(crate::scrollback::block::RenderBlock::system(text));
        }
    }

    fn composer_is_idle_and_empty(&self) -> bool {
        matches!(self.prompt_mode, PromptMode::Normal)
            && self.prompt_input_mode == PromptInputMode::Normal
            && self.prompt.text().is_empty()
            && self.prompt.images.is_empty()
    }

    fn restore_stash_entry(&mut self, entry: PromptStashEntry) {
        self.prompt_input_mode = entry.input_mode;
        self.prompt.restore(entry.prompt);
        self.prompt.refresh_slash(&self.session.models);
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_fixtures;
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    fn await_history_results(agent: &mut AgentView) {
        for _ in 0..500 {
            agent.prompt.history_search.poll();
            if agent.prompt.history_search.selected_text().is_some() {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        panic!("the history matcher never delivered a selection");
    }

    fn chords() -> [KeyEvent; 2] {
        [
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL),
            KeyEvent::new(KeyCode::Char('s'), KeyModifiers::ALT),
        ]
    }

    #[test]
    fn committing_a_recalled_shell_command_keeps_shell_mode() {
        for commit in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        ] {
            let mut agent = test_fixtures::make_agent();
            agent.session.prompt_history = vec!["! git status".to_owned()];
            agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            await_history_results(&mut agent);

            agent.handle_prompt_key_for_test(&commit);

            assert_eq!(
                agent.prompt_input_mode,
                PromptInputMode::Bash,
                "{commit:?} dropped shell mode"
            );
            assert!(
                agent.prompt.text().starts_with("git status"),
                "{commit:?} left {:?}",
                agent.prompt.text()
            );
        }
    }

    #[test]
    fn chord_round_trips_the_draft_on_both_bindings() {
        for chord in chords() {
            let mut agent = test_fixtures::make_agent();
            agent.prompt.set_text("half-typed thought");
            agent.prompt.set_cursor(4);

            let stashed = agent.handle_prompt_key_for_test(&chord);

            assert!(matches!(stashed, InputOutcome::Changed), "got {stashed:?}");
            assert_eq!(agent.prompt.text(), "");
            assert!(agent.prompt_stash.is_some());

            let restored = agent.handle_prompt_key_for_test(&chord);

            assert!(
                matches!(restored, InputOutcome::Changed),
                "got {restored:?}"
            );
            assert_eq!(agent.prompt.text(), "half-typed thought");
            assert!(agent.prompt_stash.is_none());
            assert_eq!(agent.prompt.cursor(), 4, "cursor returns to where it was");
        }
    }

    #[test]
    fn a_replaced_draft_is_discarded_and_stays_out_of_the_history() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt_input_mode = PromptInputMode::Bash;
        agent.prompt.set_text("git status");
        agent.stash_prompt_draft(StashCause::Chord);

        agent.prompt_input_mode = PromptInputMode::Normal;
        agent.prompt.set_text("second");
        agent.stash_prompt_draft(StashCause::Chord);

        let stash = agent
            .prompt_stash
            .as_ref()
            .expect("slot holds the newer draft");
        assert_eq!(stash.prompt.text, "second");

        agent.session.prompt_history = vec!["fetched from the shell".to_owned()];

        let history = agent.combined_prompt_history();
        let texts: Vec<&str> = history.iter().map(|error| error.text.as_str()).collect();

        assert_eq!(texts, ["fetched from the shell"]);
    }

    #[test]
    fn the_border_caption_reports_the_stash_and_the_rename_title() {
        let mut agent = test_fixtures::make_agent();
        assert_eq!(agent.prompt_caption(), None);

        agent.display_name = Some("payment retries".to_owned());
        assert_eq!(agent.prompt_caption().as_deref(), Some("payment retries"));

        agent.prompt.set_text("draft");
        agent.stash_prompt_draft(StashCause::Chord);
        assert_eq!(
            agent.prompt_caption().as_deref(),
            Some("Stashed · payment retries")
        );

        agent.display_name = None;
        assert_eq!(agent.prompt_caption().as_deref(), Some("Stashed"));
    }

    #[test]
    fn a_stash_during_an_image_paste_waits_for_the_image() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("draft with an image on the way");
        agent.paste_probe_in_flight = 1;

        let outcome = agent.handle_prompt_key_for_test(&chords()[0]);

        assert!(matches!(outcome, InputOutcome::Changed), "got {outcome:?}");
        assert!(
            agent.prompt_stash.is_none(),
            "stash must wait for the probe"
        );
        assert_eq!(agent.prompt.text(), "draft with an image on the way");
        assert!(matches!(
            agent.deferred_send,
            Some(super::super::AgentDeferredSend::Stash)
        ));

        agent.paste_probe_in_flight = 0;

        let kind = agent
            .take_deferred_send_after_paste()
            .expect("probe finished, so the stash drains");
        assert!(agent.resume_deferred_send(kind).is_none());

        assert!(agent.prompt_stash.is_some(), "the draft lands in the slot");
        assert_eq!(agent.prompt.text(), "");
    }

    #[test]
    fn the_up_browse_skips_the_stash_and_keeps_the_slot() {
        for commit in [
            KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
            KeyEvent::new(KeyCode::Char('!'), KeyModifiers::NONE),
        ] {
            let mut agent = test_fixtures::make_agent();
            agent.session.prompt_history = vec!["sent prompt".to_owned()];
            agent.prompt.set_text("fix the retry loop");
            agent.stash_prompt_draft(StashCause::Chord);

            agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
            assert_eq!(
                agent.prompt.text(),
                "sent prompt",
                "{commit:?}: Up browses the sent prompts, not the stash"
            );

            await_history_results(&mut agent);

            agent.handle_prompt_key_for_test(&commit);

            assert!(
                agent.prompt_stash.is_some(),
                "{commit:?} committed a history entry, so the draft stays stashed"
            );
            assert!(agent.prompt.text().starts_with("sent prompt"));
        }
    }

    #[test]
    fn escaping_the_up_browse_leaves_the_stash_alone() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("fix the retry loop");
        agent.stash_prompt_draft(StashCause::Chord);

        agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        agent.handle_prompt_key_for_test(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));

        assert_eq!(
            agent.prompt.text(),
            "",
            "Esc restores the pre-open composer"
        );
        assert!(agent.prompt_stash.is_some());
    }

    #[test]
    fn undo_cannot_resurrect_a_stashed_draft() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("a draft worth keeping");
        agent.stash_prompt_draft(StashCause::Chord);

        let undone = agent.prompt.textarea.undo();

        assert!(!undone, "the stash left an undo step behind");
        assert_eq!(agent.prompt.text(), "");
        assert!(
            agent.prompt_stash.is_some(),
            "the slot still owns the draft"
        );
    }

    #[test]
    fn a_deferred_stash_refuses_a_queued_row_edit() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("draft with an image on the way");
        agent.paste_probe_in_flight = 1;
        agent.handle_prompt_key_for_test(&chords()[0]);

        agent.prompt_mode = PromptMode::EditingQueued {
            id: 1,
            original: "queued text".to_owned(),
            server_id: None,
            kind: crate::app::agent::QueueEntryKind::Prompt,
        };
        agent.prompt.set_text("queued text being edited");
        agent.paste_probe_in_flight = 0;

        let kind = agent
            .take_deferred_send_after_paste()
            .expect("the probe finished");
        assert!(agent.resume_deferred_send(kind).is_none());

        assert!(
            agent.prompt_stash.is_none(),
            "the queued row must not land in the stash"
        );
        assert_eq!(agent.prompt.text(), "queued text being edited");
        assert!(matches!(
            agent.prompt_mode,
            PromptMode::EditingQueued { .. }
        ));
    }

    #[test]
    fn a_pop_during_an_image_paste_waits_for_the_image() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("parked draft");
        agent.stash_prompt_draft(StashCause::Chord);
        agent.paste_probe_in_flight = 1;

        let outcome = agent.handle_prompt_key_for_test(&chords()[0]);

        assert!(matches!(outcome, InputOutcome::Changed), "got {outcome:?}");
        assert_eq!(agent.prompt.text(), "", "the pop must wait for the probe");
        assert!(
            agent.prompt_stash.is_some(),
            "the slot still holds the draft"
        );
        assert!(matches!(
            agent.deferred_send,
            Some(super::super::AgentDeferredSend::Stash)
        ));

        agent.paste_probe_in_flight = 0;
        let kind = agent
            .take_deferred_send_after_paste()
            .expect("the probe finished");
        assert!(agent.resume_deferred_send(kind).is_none());

        assert_eq!(agent.prompt.text(), "parked draft", "the deferred pop ran");
        assert!(
            agent.prompt_stash.is_none(),
            "the slot handed the draft back"
        );
    }

    #[test]
    fn accepting_a_recalled_stash_empties_the_slot_on_either_input() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("fix the retry loop");
        agent.stash_prompt_draft(StashCause::Chord);

        agent.accept_history_entry("fix the retry loop");

        assert!(agent.prompt_stash.is_none(), "the accept is a pop");
        assert_eq!(agent.prompt.text(), "fix the retry loop");
    }

    #[test]
    fn a_deferred_interject_reports_the_draft_it_consumed() {
        let mut agent = test_fixtures::make_agent();
        agent.prompt.set_text("stashed thought");
        agent.stash_prompt_draft(StashCause::Chord);
        agent.session.start_turn(&mut agent.scrollback);
        agent.prompt.set_text("one more thing");

        let action = agent.resume_deferred_send(super::super::AgentDeferredSend::Interject);

        assert!(action.is_some(), "the interject reissues after the probe");
        assert_eq!(agent.prompt.text(), "", "the interject took the draft");
        assert!(
            agent.take_draft_consumed(),
            "the router restores only what a handler reports"
        );
    }

    #[test]
    fn ctrl_s_never_opens_the_session_picker() {
        let registry = crate::actions::ActionRegistry::defaults();

        for draft in ["", "a draft"] {
            let mut agent = test_fixtures::make_agent();
            agent.prompt.set_text(draft);

            let outcome = agent.handle_input(&crossterm::event::Event::Key(chords()[0]), &registry);

            assert!(
                !matches!(
                    outcome,
                    InputOutcome::Action(crate::app::actions::Action::FetchSessionList)
                ),
                "draft {draft:?} must not fetch the session list, got {outcome:?}"
            );
            assert!(
                agent.active_modal.is_none(),
                "draft {draft:?} opened a modal"
            );
        }
    }

    #[test]
    fn f3_opens_the_session_picker() {
        let registry = crate::actions::ActionRegistry::defaults();
        let mut agent = test_fixtures::make_agent();
        let f3 = KeyEvent::new(KeyCode::F(3), KeyModifiers::NONE);

        let outcome = agent.handle_input(&crossterm::event::Event::Key(f3), &registry);

        assert!(
            matches!(
                outcome,
                InputOutcome::Action(crate::app::actions::Action::FetchSessionList)
            ),
            "got {outcome:?}"
        );
        assert!(matches!(
            agent.active_modal,
            Some(crate::app::agent_view::ActiveModal::SessionPicker { .. })
        ));
    }

    #[test]
    fn minimal_mode_reports_the_stash_in_the_scrollback() {
        let mut agent = test_fixtures::make_agent();
        agent
            .prompt
            .set_screen_mode(crate::app::ScreenMode::Minimal);
        agent.prompt.set_text("draft");

        agent.handle_prompt_key_for_test(&chords()[0]);
        agent.handle_prompt_key_for_test(&chords()[0]);

        let blocks: Vec<String> = (0..agent.scrollback.len())
            .filter_map(
                |index| match agent.scrollback.entry(index).map(|error| &error.block) {
                    Some(crate::scrollback::block::RenderBlock::System(block)) => {
                        Some(block.text.clone())
                    }
                    _ => None,
                },
            )
            .collect();

        assert!(
            blocks.iter().any(|block| block.contains("Draft stashed")),
            "stash must be announced: {blocks:?}"
        );
        assert!(
            blocks
                .iter()
                .any(|block| block.contains("Stashed draft restored")),
            "restore must be announced: {blocks:?}"
        );
    }

    #[test]
    fn a_stashed_draft_stays_out_of_the_history() {
        let mut agent = test_fixtures::make_agent();
        agent.session.prompt_history = vec!["sent prompt".to_owned()];
        agent.prompt_input_mode = PromptInputMode::Bash;
        agent.prompt.set_text("git status");
        agent.stash_prompt_draft(StashCause::ClearedDraft);

        let history = agent.combined_prompt_history();
        let texts: Vec<&str> = history.iter().map(|error| error.text.as_str()).collect();

        assert_eq!(texts, ["sent prompt"]);
    }
}
