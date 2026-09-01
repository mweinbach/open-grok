use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use xai_chat_state::UsageLedger;
use xai_grok_sampling_types::reported_cost_ticks;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionUsageFile {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub session_id: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub updated_at: String,
    #[serde(default)]
    pub session: UsageSummary,
    #[serde(default)]
    pub turns: Vec<TurnUsage>,
    #[serde(skip)]
    last_incoming_turn: Option<u32>,
    #[serde(skip)]
    last_written_turn: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageSummary {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub cached_read_tokens: u64,
    #[serde(default)]
    pub cache_creation_tokens: u64,
    #[serde(default)]
    pub reasoning_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
    #[serde(default)]
    pub model_calls: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd_ticks: Option<i64>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub cost_is_partial: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub usage_is_incomplete: bool,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub turn_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub primary_model_id: Option<String>,
    #[serde(default, skip_serializing_if = "IndexMap::is_empty")]
    pub model_usage: IndexMap<String, UsageSummary>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TurnUsage {
    pub turn_number: u32,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub ended_at: String,
    #[serde(flatten)]
    pub usage: UsageSummary,
}

impl UsageSummary {
    pub fn from_ledger(ledger: &UsageLedger) -> Self {
        let mut model_usage = IndexMap::new();
        for (model, totals) in &ledger.by_model {
            model_usage.insert(model.clone(), Self::from_totals(totals, ledger.incomplete));
        }
        let mut summary = Self::from_totals(&ledger.totals, ledger.incomplete);
        summary.primary_model_id = primary_model(&model_usage);
        summary.model_usage = model_usage;
        summary
    }

    fn from_totals(totals: &xai_chat_state::UsageTotals, incomplete: bool) -> Self {
        Self {
            input_tokens: totals.input_tokens,
            output_tokens: totals.output_tokens,
            cached_read_tokens: totals.cached_read_tokens,
            cache_creation_tokens: totals.cache_creation_tokens,
            reasoning_tokens: totals.reasoning_tokens,
            total_tokens: totals.total_tokens(),
            model_calls: totals.model_calls,
            cost_usd_ticks: totals.cost_usd_ticks,
            cost_is_partial: totals.cost_is_partial(),
            usage_is_incomplete: incomplete,
            turn_count: 0,
            primary_model_id: None,
            model_usage: IndexMap::new(),
        }
    }

    pub fn covers(&self, previous: &Self) -> bool {
        self.input_tokens >= previous.input_tokens
            && self.output_tokens >= previous.output_tokens
            && self.model_calls >= previous.model_calls
    }

    pub fn saturating_add(&self, other: &Self) -> Self {
        let mut model_usage = self.model_usage.clone();
        for (model, row) in &other.model_usage {
            let entry = model_usage.entry(model.clone()).or_default();
            *entry = entry.saturating_add_row(row);
        }
        let mut out = self.saturating_add_row(other);
        out.primary_model_id = primary_model(&model_usage);
        out.model_usage = model_usage;
        out.turn_count = self.turn_count.saturating_add(other.turn_count);
        out
    }

    fn saturating_add_row(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_add(other.input_tokens),
            output_tokens: self.output_tokens.saturating_add(other.output_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_add(other.cached_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_add(other.cache_creation_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_add(other.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_add(other.total_tokens),
            model_calls: self.model_calls.saturating_add(other.model_calls),
            cost_usd_ticks: merge_cost_ticks(self.cost_usd_ticks, other.cost_usd_ticks),
            cost_is_partial: self.cost_is_partial || other.cost_is_partial,
            usage_is_incomplete: self.usage_is_incomplete || other.usage_is_incomplete,
            turn_count: 0,
            primary_model_id: None,
            model_usage: IndexMap::new(),
        }
    }

    pub fn saturating_sub(&self, other: &Self) -> Self {
        let mut model_usage = IndexMap::new();
        for (model, row) in &self.model_usage {
            let prev = other.model_usage.get(model).cloned().unwrap_or_default();
            let delta = row.saturating_sub_row(&prev);
            if !delta.is_zero() {
                model_usage.insert(model.clone(), delta);
            }
        }
        let mut out = self.saturating_sub_row(other);
        out.primary_model_id = primary_model(&model_usage);
        out.model_usage = model_usage;
        out
    }

    fn saturating_sub_row(&self, other: &Self) -> Self {
        Self {
            input_tokens: self.input_tokens.saturating_sub(other.input_tokens),
            output_tokens: self.output_tokens.saturating_sub(other.output_tokens),
            cached_read_tokens: self
                .cached_read_tokens
                .saturating_sub(other.cached_read_tokens),
            cache_creation_tokens: self
                .cache_creation_tokens
                .saturating_sub(other.cache_creation_tokens),
            reasoning_tokens: self.reasoning_tokens.saturating_sub(other.reasoning_tokens),
            total_tokens: self.total_tokens.saturating_sub(other.total_tokens),
            model_calls: self.model_calls.saturating_sub(other.model_calls),
            cost_usd_ticks: sub_cost_ticks(self.cost_usd_ticks, other.cost_usd_ticks),
            cost_is_partial: self.cost_is_partial || other.cost_is_partial,
            usage_is_incomplete: self.usage_is_incomplete,
            turn_count: 0,
            primary_model_id: None,
            model_usage: IndexMap::new(),
        }
    }

    fn is_zero(&self) -> bool {
        self.input_tokens == 0
            && self.output_tokens == 0
            && self.cached_read_tokens == 0
            && self.cache_creation_tokens == 0
            && self.reasoning_tokens == 0
            && self.model_calls == 0
            && self.cost_usd_ticks.is_none()
    }
}

pub enum UsageLoad {
    SessionNotFound,
    NoUsage,
    Ready(Box<SessionUsageFile>),
}

impl SessionUsageFile {
    pub fn new(session_id: impl Into<String>) -> Self {
        Self {
            session_id: session_id.into(),
            ..Self::default()
        }
    }

    pub fn load_for_session(session_id: &str) -> std::io::Result<UsageLoad> {
        load_for_session_in_root(
            session_id,
            &crate::util::grok_home::grok_home().join("sessions"),
        )
    }

    pub fn turn(&self, turn_number: u32) -> Option<&TurnUsage> {
        self.turns
            .iter()
            .find(|turn| turn.turn_number == turn_number)
    }

    pub fn retain_turns_through(&mut self, max_turn: u32) {
        self.turns.retain(|turn| turn.turn_number <= max_turn);
        let mut session = UsageSummary::default();
        for turn in &self.turns {
            session = session.saturating_add(&turn.usage);
        }
        session.turn_count = self.turns.len() as u64;
        session.primary_model_id = primary_model(&session.model_usage);
        self.session = session;
    }

    pub fn apply_turn(
        &mut self,
        turn_number: u32,
        ended_at: impl Into<String>,
        live: &UsageSummary,
        prev_live: Option<&UsageSummary>,
    ) -> u32 {
        let fold_into = (self.last_incoming_turn == Some(turn_number))
            .then_some(self.last_written_turn)
            .flatten();
        let written = self.apply_turn_internal(turn_number, ended_at, live, prev_live, fold_into);
        self.last_incoming_turn = Some(turn_number);
        self.last_written_turn = Some(written);
        written
    }

    pub(crate) fn restore_apply_cursor(
        &mut self,
        last_incoming_turn: Option<u32>,
        last_written_turn: Option<u32>,
    ) {
        self.last_incoming_turn = last_incoming_turn;
        self.last_written_turn = last_written_turn;
    }

    pub(crate) fn apply_cursor(&self) -> (Option<u32>, Option<u32>) {
        (self.last_incoming_turn, self.last_written_turn)
    }

    fn apply_turn_internal(
        &mut self,
        mut turn_number: u32,
        ended_at: impl Into<String>,
        live: &UsageSummary,
        prev_live: Option<&UsageSummary>,
        fold_into: Option<u32>,
    ) -> u32 {
        let ended_at = ended_at.into();
        self.updated_at = ended_at.clone();

        let mut turn_usage = match prev_live {
            Some(prev) if live.covers(prev) => live.saturating_sub(prev),
            _ => live.clone(),
        };

        if let Some(fold_n) = fold_into
            && let Some(existing) = self
                .turns
                .iter_mut()
                .find(|turn| turn.turn_number == fold_n)
        {
            if turn_usage.is_zero() {
                return fold_n;
            }
            existing.ended_at = ended_at;
            existing.usage = existing.usage.saturating_add(&turn_usage);
            existing.usage.turn_count = 1;
            existing.usage.primary_model_id = primary_model(&existing.usage.model_usage);
            self.session = self.session.saturating_add(&turn_usage);
            self.session.turn_count = self.turns.len() as u64;
            return fold_n;
        }

        if self
            .turns
            .iter()
            .any(|turn| turn.turn_number == turn_number)
        {
            turn_number = self
                .turns
                .iter()
                .map(|turn| turn.turn_number)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
        }

        turn_usage.turn_count = 1;
        turn_usage.primary_model_id = primary_model(&turn_usage.model_usage);

        self.turns.push(TurnUsage {
            turn_number,
            ended_at,
            usage: turn_usage.clone(),
        });

        self.session = self.session.saturating_add(&turn_usage);
        self.session.turn_count = self.turns.len() as u64;
        self.session.primary_model_id = primary_model(&self.session.model_usage);
        turn_number
    }
}

fn load_for_session_in_root(
    session_id: &str,
    sessions_root: &std::path::Path,
) -> std::io::Result<UsageLoad> {
    let Some(dir) = crate::session::persistence::find_persisted_session_dir_by_id_in_root_result(
        session_id,
        sessions_root,
    )?
    else {
        return Ok(UsageLoad::SessionNotFound);
    };
    let path = dir.join(crate::session::storage::USAGE_FILE);
    if !path.is_file() {
        return Ok(UsageLoad::NoUsage);
    }
    let data = std::fs::read(&path)?;
    if data.iter().all(u8::is_ascii_whitespace) {
        return Ok(UsageLoad::NoUsage);
    }
    match serde_json::from_slice(&data) {
        Ok(file) => Ok(UsageLoad::Ready(Box::new(file))),
        Err(error) => Err(std::io::Error::new(std::io::ErrorKind::InvalidData, error)),
    }
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn primary_model(model_usage: &IndexMap<String, UsageSummary>) -> Option<String> {
    model_usage
        .iter()
        .max_by_key(|(_, row)| (row.model_calls, row.total_tokens))
        .map(|(name, _)| name.clone())
}

fn merge_cost_ticks(first: Option<i64>, second: Option<i64>) -> Option<i64> {
    match (first, second) {
        (None, None) => None,
        (first, second) => {
            reported_cost_ticks(Some(first.unwrap_or(0).saturating_add(second.unwrap_or(0))))
        }
    }
}

fn sub_cost_ticks(live: Option<i64>, previous: Option<i64>) -> Option<i64> {
    match (live, previous) {
        (None, _) => None,
        (Some(live), None) => reported_cost_ticks(Some(live)),
        (Some(live), Some(previous)) => reported_cost_ticks(Some(live.saturating_sub(previous))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use xai_chat_state::UsageLedger;
    use xai_grok_sampling_types::TokenUsage;

    fn tu(prompt: u32, completion: u32) -> TokenUsage {
        TokenUsage {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: prompt + completion,
            reasoning_tokens: 0,
            cached_prompt_tokens: 0,
            cache_creation_prompt_tokens: 0,
        }
    }

    fn live(calls: &[(&str, u32, u32, Option<i64>)]) -> UsageSummary {
        let mut ledger = UsageLedger::default();
        for (model, prompt, completion, cost) in calls {
            ledger.record_main_loop_call(model, &tu(*prompt, *completion), Some(10), *cost);
        }
        UsageSummary::from_ledger(&ledger)
    }

    #[test]
    fn first_turn_writes_session_and_one_turn() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "2026-08-26T00:00:00Z", &first, None);

        assert_eq!(file.turns.len(), 1);
        assert_eq!(file.turns[0].turn_number, 1);
        assert_eq!(file.turns[0].usage.input_tokens, 100);
        assert_eq!(file.turns[0].usage.output_tokens, 20);
        assert_eq!(file.turns[0].usage.cost_usd_ticks, Some(50));
        assert_eq!(file.session.input_tokens, 100);
        assert_eq!(file.session.output_tokens, 20);
        assert_eq!(file.session.turn_count, 1);
        assert_eq!(file.session.cost_usd_ticks, Some(50));
        assert_eq!(file.session.primary_model_id.as_deref(), Some("grok-4"));
        assert_eq!(file.updated_at, "2026-08-26T00:00:00Z");
    }

    #[test]
    fn session_primary_model_is_the_most_used_not_the_last_turn() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 80, 10, Some(40))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(
            2,
            "t2",
            &live(&[
                ("grok-4", 100, 20, Some(50)),
                ("grok-4", 80, 10, Some(40)),
                ("grok-fast", 10, 2, Some(1)),
            ]),
            Some(&first),
        );

        assert_eq!(
            file.turns[1].usage.primary_model_id.as_deref(),
            Some("grok-fast")
        );
        assert_eq!(file.session.primary_model_id.as_deref(), Some("grok-4"));
    }

    #[test]
    fn second_turn_appends_and_session_becomes_latest_ledger() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(
            2,
            "t2",
            &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
            Some(&first),
        );

        assert_eq!(file.turns.len(), 2);
        assert_eq!(file.turns[1].turn_number, 2);
        assert_eq!(file.turns[1].usage.input_tokens, 40);
        assert_eq!(file.turns[1].usage.output_tokens, 10);
        assert_eq!(file.turns[1].usage.cost_usd_ticks, Some(20));
        assert_eq!(file.session.input_tokens, 140);
        assert_eq!(file.session.output_tokens, 30);
        assert_eq!(file.session.turn_count, 2);
        assert_eq!(file.session.cost_usd_ticks, Some(70));
    }

    #[test]
    fn inherited_turn_number_without_fold_appends() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.restore_apply_cursor(None, None);
        let resumed = live(&[("grok-4", 10, 2, Some(5))]);
        file.apply_turn(1, "t-resume", &resumed, None);

        assert_eq!(file.turns.len(), 2);
        assert_eq!(file.turns[0].turn_number, 1);
        assert_eq!(file.turns[0].usage.input_tokens, 100);
        assert_eq!(file.turns[1].turn_number, 2);
        assert_eq!(file.turns[1].usage.input_tokens, 10);
        assert_eq!(file.session.input_tokens, 110);
        assert_eq!(file.session.turn_count, 2);
    }

    #[test]
    fn duplicate_turn_number_zero_delta_does_not_mutate_turns() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(1, "t1-again", &first, Some(&first));

        assert_eq!(file.turns.len(), 1);
        assert_eq!(file.turns[0].ended_at, "t1");
        assert_eq!(file.session.turn_count, 1);
        assert_eq!(file.updated_at, "t1-again");
    }

    #[test]
    fn duplicate_turn_number_folds_extra_live_usage() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        let continued = live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]);
        file.apply_turn(1, "t1-late", &continued, Some(&first));

        assert_eq!(file.turns.len(), 1);
        assert_eq!(file.turns[0].ended_at, "t1-late");
        assert_eq!(file.turns[0].usage.input_tokens, 140);
        assert_eq!(file.turns[0].usage.output_tokens, 30);
        assert_eq!(file.turns[0].usage.cost_usd_ticks, Some(70));
        assert_eq!(file.session.input_tokens, 140);
        assert_eq!(file.session.output_tokens, 30);
        assert_eq!(file.session.turn_count, 1);
        assert_eq!(file.session.cost_usd_ticks, Some(70));
    }

    #[test]
    fn resume_folds_new_process_ledger_onto_persisted_session() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(
            2,
            "t2",
            &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
            Some(&first),
        );

        let post_resume_1 = live(&[("grok-4", 25, 5, Some(8))]);
        file.apply_turn(3, "t3", &post_resume_1, None);

        assert_eq!(file.turns.len(), 3);
        assert_eq!(file.turns[2].turn_number, 3);
        assert_eq!(file.turns[2].usage.input_tokens, 25);
        assert_eq!(file.turns[2].usage.output_tokens, 5);
        assert_eq!(file.session.input_tokens, 165);
        assert_eq!(file.session.output_tokens, 35);
        assert_eq!(file.session.turn_count, 3);
        assert_eq!(file.session.cost_usd_ticks, Some(78));
    }

    #[test]
    fn resume_later_turns_use_process_local_delta() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(
            2,
            "t2",
            &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
            Some(&first),
        );

        let post_resume_1 = live(&[("grok-4", 25, 5, Some(8))]);
        file.apply_turn(3, "t3", &post_resume_1, None);
        file.apply_turn(
            4,
            "t4",
            &live(&[("grok-4", 25, 5, Some(8)), ("grok-4", 30, 6, Some(9))]),
            Some(&post_resume_1),
        );

        assert_eq!(file.turns.len(), 4);
        assert_eq!(file.turns[3].usage.input_tokens, 30);
        assert_eq!(file.turns[3].usage.output_tokens, 6);
        assert_eq!(file.session.input_tokens, 195);
        assert_eq!(file.session.output_tokens, 41);
        assert_eq!(file.session.turn_count, 4);
        assert_eq!(file.session.cost_usd_ticks, Some(87));
    }

    #[test]
    fn retain_turns_through_drops_later_turns_and_rebuilds_session() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(
            2,
            "t2",
            &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
            Some(&first),
        );
        file.retain_turns_through(1);

        assert_eq!(file.turns.len(), 1);
        assert_eq!(file.turns[0].turn_number, 1);
        assert_eq!(file.session.input_tokens, 100);
        assert_eq!(file.session.turn_count, 1);
        assert_eq!(file.session.cost_usd_ticks, Some(50));
    }

    #[test]
    fn turn_lookup_returns_matching_row() {
        let mut file = SessionUsageFile::new("sess-1");
        let first = live(&[("grok-4", 100, 20, Some(50))]);
        file.apply_turn(1, "t1", &first, None);
        file.apply_turn(
            2,
            "t2",
            &live(&[("grok-4", 100, 20, Some(50)), ("grok-4", 40, 10, Some(20))]),
            Some(&first),
        );

        assert_eq!(file.turn(2).unwrap().usage.input_tokens, 40);
        assert!(file.turn(3).is_none());
    }

    #[test]
    fn covers_detects_same_process_vs_reset_ledger() {
        let bigger = live(&[("m", 10, 1, None), ("m", 5, 1, None)]);
        let smaller = live(&[("m", 5, 1, None)]);
        assert!(bigger.covers(&smaller));
        assert!(!smaller.covers(&bigger));
        assert!(smaller.covers(&UsageSummary::default()));
    }
}
