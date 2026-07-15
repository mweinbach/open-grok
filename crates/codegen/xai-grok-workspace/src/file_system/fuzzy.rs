use std::{
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc::{RecvError, RecvTimeoutError, SyncSender, sync_channel},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use ignore::{DirEntry, WalkBuilder, WalkState, overrides::OverrideBuilder};
use nucleo::{
    Match, Matcher, Nucleo, Snapshot, Utf32String,
    pattern::{CaseMatching, MultiPattern, Normalization, Pattern},
};

const NUM_NUCLEO_THREADS: usize = 2;
const NUM_IGNORE_THREADS: usize = 8;

#[derive(Debug, Clone, Default)]
pub struct FuzzyMatchResult {
    // Path of the matched entry.
    pub path: Utf32String,
    /// Matcher score, higher is better.
    pub score: u32,
    /// Matched indices of characters.
    pub indices: Vec<u32>,
    /// Is it a directory.
    pub is_dir: bool,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct FuzzyMatcherStatus {
    pub changed: bool,
    pub done: bool,
}

#[derive(Debug, Clone)]
struct MatchEntry {
    pub is_dir: bool,
}

/// A very fast fuzzy matcher that does ignore-walking. Both happen in background threads.
pub struct FuzzyFileMatcher {
    root: PathBuf,
    query: String,
    nucleo: Nucleo<MatchEntry>,
    matcher: Matcher,
    walk_handle: Option<JoinHandle<()>>,
    cancel: Arc<AtomicBool>,
    top_entries: Vec<FuzzyMatchResult>,
    dirs: bool,
}

impl FuzzyFileMatcher {
    /// Create a new matcher with default config focused on matching paths.
    pub fn new(root: &Path) -> Self {
        let matcher_config = nucleo::Config::DEFAULT.match_paths();
        // matcher_config.prefer_prefix = true; // yes or no? nucleo docs lean towards no

        let mut nucleo = Nucleo::new(
            matcher_config.clone(),
            Arc::new(move || ()),
            Some(NUM_NUCLEO_THREADS),
            1,
        );
        nucleo.pattern = MultiPattern::new(1);

        Self {
            root: root.to_owned(),
            nucleo,
            matcher: Matcher::new(matcher_config),
            walk_handle: None,
            cancel: Arc::new(AtomicBool::new(false)),
            query: String::new(),
            top_entries: Vec::new(),
            dirs: false,
        }
    }

    pub fn query(&self) -> &str {
        &self.query
    }

    /// Start a new walk and restart nucleo matcher.
    pub fn restart_walk_custom(
        &mut self,
        make_walker: impl FnOnce(&mut WalkBuilder) -> &mut WalkBuilder,
    ) {
        // first, wait for previous walker to finish if it's up
        self.cancel.store(true, Ordering::Relaxed);
        if let Some(walk_handle) = self.walk_handle.take() {
            walk_handle.join().unwrap();
        }

        // disconnect all injectors and clear snapshots and streams
        self.nucleo.restart(true);

        // we're back in business
        self.cancel.store(false, Ordering::Relaxed);

        // build the walker(s)
        let walker_builder = make_walker(
            WalkBuilder::new(&self.root)
                .threads(NUM_IGNORE_THREADS)
                .follow_links(false)
                .git_ignore(true)
                .git_global(true)
                .git_exclude(true)
                .ignore(true)
                .hidden(true)
                .require_git(false)
                .overrides(
                    OverrideBuilder::new(&self.root)
                        .add("!.git")
                        .unwrap()
                        .build()
                        .unwrap(),
                ),
        )
        .clone();

        fn check_entry<'a>(entry: &'a DirEntry, root: &Path) -> Option<(&'a str, bool)> {
            let path = entry.path();
            if path != root
                && let Some(file_type) = entry.file_type()
                && (file_type.is_file() || file_type.is_dir())
                && let Ok(path) = path.strip_prefix(root)
                && let Some(path) = path.as_os_str().to_str()
                && !path.is_empty()
            {
                Some((path, file_type.is_dir()))
            } else {
                None
            }
        }

        // we'll just do it in a blocking way here assuming it's super fast anyway
        let top_walker = walker_builder
            .clone()
            .max_depth(Some(1))
            .sort_by_file_name(|a, b| a.cmp(b))
            .build();
        let top_entries = top_walker
            .into_iter()
            .filter_map(|entry| {
                let entry = entry.ok()?;
                let (path, is_dir) = check_entry(&entry, &self.root)?;
                Some(FuzzyMatchResult {
                    path: path.into(),
                    score: 0,
                    indices: Vec::new(),
                    is_dir,
                })
            })
            .collect::<Vec<_>>();

        let injector = self.nucleo.injector();
        let root = self.root.clone();
        let cancel = self.cancel.clone();

        // link walker threads with injectors and start it up
        let walker = walker_builder.build_parallel();
        let walk_handle = thread::spawn(move || {
            walker.run(|| {
                let injector = injector.clone();
                let root = root.clone();
                let cancel = cancel.clone();
                Box::new(move |entry| {
                    if cancel.load(Ordering::Relaxed) {
                        return WalkState::Quit;
                    } else if let Ok(entry) = entry
                        && let Some((path, is_dir)) = check_entry(&entry, &root)
                    {
                        injector.push(MatchEntry { is_dir }, |_entry, columns| {
                            columns[0] = path.into();
                        });
                    }
                    WalkState::Continue
                })
            });
        });
        self.walk_handle = Some(walk_handle);
        self.top_entries = top_entries;

        self.nucleo.tick(0);
    }

    /// Restart the walk with default walker parameters.
    pub fn restart_walk(&mut self) {
        self.restart_walk_custom(|w| w);
    }

    /// Set the query to a given string and trigger reparse.
    ///
    /// It will be faster if the current query is a strict prefix of the new query.
    pub fn set_query(&mut self, mut query: &str, dirs: bool) {
        self.dirs = dirs;
        if dirs && query.ends_with('/') {
            query = &query[..query.len() - 1];
        }
        if query == self.query {
            return;
        }
        // see this re: backslash etc: https://github.com/helix-editor/nucleo/pull/87
        let append = query.as_bytes().starts_with(self.query.as_bytes())
            && !query.ends_with('\\')
            && !query
                .as_bytes()
                .last()
                .is_some_and(|ch| ch.is_ascii_whitespace());
        self.nucleo
            .pattern
            .reparse(0, query, CaseMatching::Smart, Normalization::Smart, append);
        self.nucleo.tick(0);
        self.query = query.to_owned();
    }

    /// Sends a tick to nucleo matcher. Can be safely called at any frequency.
    pub fn tick(&mut self, tick_timeout_ms: u64) -> FuzzyMatcherStatus {
        if self.query.is_empty() {
            return FuzzyMatcherStatus {
                done: true,
                changed: false,
            };
        }
        let status = self.nucleo.tick(tick_timeout_ms);
        let done = self.nucleo.active_injectors() == 0 && !status.running;
        FuzzyMatcherStatus {
            done,
            changed: status.changed,
        }
    }

    /// Total number of currently matched items in the snapshot.
    pub fn num_items(&self) -> usize {
        if self.query.is_empty() {
            self.top_entries.len()
        } else {
            self.nucleo.snapshot().item_count() as _
        }
    }

    /// Get top `k` items from the snapshot and sort them by score, path length and path.
    pub fn get_top_k(&mut self, k: usize) -> Vec<FuzzyMatchResult> {
        // note: &mut only because we access self.matcher which has internal allocations

        // rust is a bit dumb at times, we'll need this for sorting without cloning
        fn sort_by_key_hrtb<T, F, K, Q>(slice: &mut [T], f: F)
        where
            F: for<'a> Fn(&'a T) -> (Q, &'a K),
            K: Ord,
            Q: Ord,
        {
            slice.sort_by(|a, b| f(a).cmp(&f(b)))
        }

        // special case: if query is empty, return top items only
        if self.query.is_empty() {
            return self
                .top_entries
                .iter()
                // dirs_only=true means only directories; dirs_only=false means both files and directories
                .filter(|e| !self.dirs || e.is_dir)
                .take(k)
                .cloned()
                .collect(); // should be already sorted
        }

        // https://github.com/helix-editor/helix/blob/d79cce4e4bfc24dd204f1b294c899ed73f7e9453/helix-term/src/ui/completion.rs#L369
        // suggested min score = 7 * len + 14
        let len = self.query.chars().count() as u32;
        let min_score = 7 + len * 14;

        let mut items = Vec::with_capacity(k);
        let pattern = self.nucleo.pattern.column_pattern(0);
        let snapshot = self.nucleo.snapshot();
        let mut iter = snapshot.matches().iter().peekable();

        while items.len() < k
            && let Some(m) = iter.next()
            // for empty queries, return everything; otherwise, apply heuristic min-score limit
            && (self.query.is_empty() ||  m.score >= min_score)
        {
            fn extract_match(
                m: &Match,
                snapshot: &Snapshot<MatchEntry>,
                pattern: &Pattern,
                matcher: &mut Matcher,
                dirs_only: bool,
            ) -> Option<FuzzyMatchResult> {
                let item = unsafe { snapshot.get_item_unchecked(m.idx) };
                // dirs_only=true means only directories; dirs_only=false means both files and directories
                if dirs_only && !item.data.is_dir {
                    return None;
                }
                let path = item.matcher_columns[0].clone();
                let mut indices = Vec::new();
                if !pattern.atoms.is_empty() {
                    pattern.indices(path.slice(..), matcher, &mut indices);
                }
                Some(FuzzyMatchResult {
                    path,
                    score: m.score,
                    indices,
                    is_dir: item.data.is_dir,
                })
            }

            if !pattern.atoms.is_empty() {
                let start = items.len();
                items.extend(extract_match(
                    m,
                    snapshot,
                    pattern,
                    &mut self.matcher,
                    self.dirs,
                ));
                while iter.peek().is_some_and(|p| p.score == m.score) {
                    let m = iter.next().unwrap();
                    items.extend(extract_match(
                        m,
                        snapshot,
                        pattern,
                        &mut self.matcher,
                        self.dirs,
                    ));
                }
                sort_by_key_hrtb(&mut items[start..], |m| (m.path.len(), &m.path));
            } else {
                items.extend(extract_match(
                    m,
                    snapshot,
                    pattern,
                    &mut self.matcher,
                    self.dirs,
                ));
            }
        }

        if items.len() > k {
            items.truncate(k);
        }

        if pattern.atoms.is_empty() {
            sort_by_key_hrtb(&mut items, |m| (true, &m.path));
        }

        items
    }
}

impl Drop for FuzzyFileMatcher {
    fn drop(&mut self) {
        // note: walker threads *may* get detached for a little while but hopefully not for too long
        self.cancel.store(true, Ordering::Relaxed);
    }
}

#[derive(Debug, Clone, Default)]
pub struct FuzzyMatcherDaemonResults {
    pub topk: Arc<[FuzzyMatchResult]>,
    pub num_items: usize,
    pub status: FuzzyMatcherStatus,
    pub generation: usize,
}

impl AsRef<[FuzzyMatchResult]> for FuzzyMatcherDaemonResults {
    fn as_ref(&self) -> &[FuzzyMatchResult] {
        self.topk.as_ref()
    }
}

#[derive(Debug, Clone)]
enum FuzzyMatcherDaemonMessage {
    RestartWalk { hidden: bool },
    SetQuery { query: String, dirs: bool },
    Stop,
}

pub struct FuzzyFileMatcherDaemon {
    results: Arc<Mutex<FuzzyMatcherDaemonResults>>,
    tx: SyncSender<FuzzyMatcherDaemonMessage>,
    _handle: JoinHandle<()>,
}

impl FuzzyFileMatcherDaemon {
    pub fn new(mut matcher: FuzzyFileMatcher, topk: usize) -> Self {
        let results = Arc::new(Mutex::new(FuzzyMatcherDaemonResults::default()));
        let (tx, rx) = sync_channel(1024);

        let res = results.clone();
        let handle = thread::spawn(move || {
            let results = res;
            let mut done = false;
            let mut generation = 0;
            loop {
                let msg = if !done {
                    rx.recv_timeout(Duration::from_micros(250))
                } else {
                    rx.recv().map_err(|e| match e {
                        RecvError => RecvTimeoutError::Disconnected,
                    })
                };
                match msg {
                    Ok(FuzzyMatcherDaemonMessage::RestartWalk { hidden }) => {
                        if !hidden {
                            tracing::trace!("restarting normal walk");
                            matcher.restart_walk();
                        } else {
                            tracing::trace!("restarting hidden walk");
                            matcher.restart_walk_custom(|w| {
                                w.hidden(false).ignore(false).git_ignore(false)
                            });
                        }
                        generation += 1;
                        *results.lock().unwrap() = FuzzyMatcherDaemonResults::default();
                        done = false;
                    }
                    Ok(FuzzyMatcherDaemonMessage::SetQuery { query, dirs }) => {
                        matcher.set_query(&query, dirs);
                        generation += 1;
                        done = false;
                    }
                    Ok(FuzzyMatcherDaemonMessage::Stop) | Err(RecvTimeoutError::Disconnected) => {
                        break;
                    }
                    Err(RecvTimeoutError::Timeout) => {
                        if !done {
                            let status = matcher.tick(10);
                            done = status.done;
                            let num_items = matcher.num_items();
                            let topk: Arc<[_]> = matcher.get_top_k(topk).into();
                            *results.lock().unwrap() = FuzzyMatcherDaemonResults {
                                topk,
                                num_items,
                                status,
                                generation,
                            };
                            generation += 1;
                        }
                    }
                }
            }
        });

        Self {
            results,
            tx,
            _handle: handle,
        }
    }

    pub fn get(&self) -> FuzzyMatcherDaemonResults {
        self.results.lock().unwrap().clone()
    }

    pub fn set_query(&self, query: impl AsRef<str>, dirs: bool) {
        let query = query.as_ref().to_owned();
        _ = self
            .tx
            .send(FuzzyMatcherDaemonMessage::SetQuery { query, dirs })
            .ok();
    }

    pub fn restart_walk(&self, hidden: bool) {
        _ = self
            .tx
            .send(FuzzyMatcherDaemonMessage::RestartWalk { hidden })
            .ok();
    }
}

impl Drop for FuzzyFileMatcherDaemon {
    fn drop(&mut self) {
        _ = self.tx.send(FuzzyMatcherDaemonMessage::Stop).ok();
    }
}
