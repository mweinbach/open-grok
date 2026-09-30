use super::*;

/// A startup sub-phase routed to its own slot, so a producer timer's field is chosen by the enum rather than a string match.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Subphase {
    SessionLoad,
    SessionReplay,
    SessionGitScan,
    SessionSpawn,
}

/// Per-subphase timings keyed by [`Subphase`], plus the gaps that are not subphases.
#[derive(Clone, Copy)]
pub(crate) struct SubphaseTimings {
    session_load_ms: Option<u64>,
    session_replay_ms: Option<u64>,
    session_git_scan_ms: Option<u64>,
    session_spawn_ms: Option<u64>,
    pub(crate) prefetch_wait_ms: Option<u64>,
    pub(crate) time_to_first_frame_ms: Option<u64>,
}

impl SubphaseTimings {
    const fn new() -> Self {
        Self {
            session_load_ms: None,
            session_replay_ms: None,
            session_git_scan_ms: None,
            session_spawn_ms: None,
            prefetch_wait_ms: None,
            time_to_first_frame_ms: None,
        }
    }

    pub(crate) fn get(&self, sp: Subphase) -> Option<u64> {
        match sp {
            Subphase::SessionLoad => self.session_load_ms,
            Subphase::SessionReplay => self.session_replay_ms,
            Subphase::SessionGitScan => self.session_git_scan_ms,
            Subphase::SessionSpawn => self.session_spawn_ms,
        }
    }

    fn set(&mut self, sp: Subphase, ms: u64) {
        let slot = match sp {
            Subphase::SessionLoad => &mut self.session_load_ms,
            Subphase::SessionReplay => &mut self.session_replay_ms,
            Subphase::SessionGitScan => &mut self.session_git_scan_ms,
            Subphase::SessionSpawn => &mut self.session_spawn_ms,
        };
        *slot = Some(ms);
    }

    /// Builds the wire event through the per-subphase accessor; `get`'s exhaustive
    /// match fails to compile until a new [`Subphase`] is routed to its field.
    pub(crate) fn startup_completed(
        &self,
        total_ms: u64,
        outcome: StartupOutcome,
        phases: String,
        auth_mode: AuthMode,
    ) -> crate::events::StartupCompleted {
        crate::events::StartupCompleted {
            total_ms,
            outcome,
            phases,
            auth_mode,
            prefetch_wait_ms: self.prefetch_wait_ms,
            session_load_ms: self.get(Subphase::SessionLoad),
            session_replay_ms: self.get(Subphase::SessionReplay),
            session_git_scan_ms: self.get(Subphase::SessionGitScan),
            session_spawn_ms: self.get(Subphase::SessionSpawn),
            time_to_first_frame_ms: self.time_to_first_frame_ms,
        }
    }
}

impl Default for SubphaseTimings {
    fn default() -> Self {
        Self::new()
    }
}

static SUBPHASES: Mutex<SubphaseTimings> = Mutex::new(SubphaseTimings::new());

pub(crate) fn subphases() -> std::sync::MutexGuard<'static, SubphaseTimings> {
    SUBPHASES.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn record_prefetch_wait(elapsed: Duration) {
    subphases().prefetch_wait_ms = Some(duration_ms(elapsed));
}

pub(crate) fn record_subphase(sp: Subphase, elapsed: Duration) {
    subphases().set(sp, duration_ms(elapsed));
}
