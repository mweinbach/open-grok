//! Background persistence for tool state.
//!
//! [`ResourcesPersistence`] persists `Resources` state (the new architecture).
//! Old `ToolStatePersistence` and `PersistenceLayer` have been deleted.

use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::types::resources::Resources;

/// Background persistence for `Resources` state/params.
///
/// Same pattern as `ToolStatePersistence` — debounced background writes with
/// atomic rename. Takes a `serde_json::Value` from `Resources::serialize()`
/// and writes it to disk. On load, parses the JSON and feeds it to
/// `Resources::load_from()`.
///
/// This replaces the old `ToolStatePersistence` pipeline for the new
/// architecture. During migration both coexist; once all tools are migrated,
/// `ToolStatePersistence` will be deleted.
pub struct ResourcesPersistence {
    /// Path to the JSON file where Resources state is persisted
    state_path: PathBuf,
    /// Channel to send serialized state to the background writer
    tx: tokio::sync::mpsc::UnboundedSender<ResourcesPersistenceCommand>,
}

enum ResourcesPersistenceCommand {
    /// Write this serialized Resources value to disk
    Save(serde_json::Value),
    /// Flush pending writes and notify when done
    Flush(tokio::sync::oneshot::Sender<()>),
}

impl ResourcesPersistence {
    /// Construct a noop persistence handle for tests. No background task.
    pub fn noop() -> Self {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        Self {
            state_path: PathBuf::from("/dev/null"),
            tx,
        }
    }

    /// Create a new persistence handle and spawn the background writer task.
    pub fn new(state_path: PathBuf) -> Self {
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let writer_path = state_path.clone();

        tokio::spawn(async move {
            Self::writer_loop(rx, writer_path).await;
        });

        Self { state_path, tx }
    }

    /// Load existing Resources state from disk, if the file exists.
    ///
    /// Reads the JSON, parses it into the nested `HashMap<String, HashMap<String, Value>>`
    /// shape that `Resources::load_from()` expects, and applies it to the given resources.
    ///
    /// Returns `true` if state was loaded, `false` if no file or parse error.
    pub fn load(&self, resources: &mut Resources) -> bool {
        let json = match std::fs::read_to_string(&self.state_path) {
            Ok(s) => s,
            Err(_) => return false,
        };

        let top: serde_json::Value = match serde_json::from_str(&json) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(
                    "Failed to parse resources state from {:?}: {}",
                    self.state_path,
                    e
                );
                return false;
            }
        };

        let data = match Self::value_to_nested_map(top) {
            Some(m) => m,
            None => {
                tracing::warn!(
                    "Resources state file {:?} has unexpected shape",
                    self.state_path
                );
                return false;
            }
        };

        resources.load_from(data);
        true
    }

    /// Save the current Resources state (non-blocking).
    /// Sends a serialized snapshot to the background writer.
    pub fn save(&self, resources: &Resources) {
        let snapshot = resources.serialize();
        let _ = self.tx.send(ResourcesPersistenceCommand::Save(snapshot));
    }

    /// Path to the persisted state file.
    pub fn state_path(&self) -> &std::path::Path {
        &self.state_path
    }

    /// Flush pending writes. Call on graceful shutdown.
    pub async fn flush(&self) {
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let _ = self.tx.send(ResourcesPersistenceCommand::Flush(done_tx));
        let _ = done_rx.await;
    }

    /// Convert the `serde_json::Value` (from serialize()) into the nested
    /// HashMap structure that `load_from()` expects.
    fn value_to_nested_map(
        val: serde_json::Value,
    ) -> Option<
        std::collections::HashMap<String, std::collections::HashMap<String, serde_json::Value>>,
    > {
        let top = val.as_object()?;
        let mut result = std::collections::HashMap::new();
        for (cat_key, cat_val) in top {
            let inner_obj = cat_val.as_object()?;
            let mut inner = std::collections::HashMap::new();
            for (k, v) in inner_obj {
                inner.insert(k.clone(), v.clone());
            }
            result.insert(cat_key.clone(), inner);
        }
        Some(result)
    }

    async fn writer_loop(
        mut rx: tokio::sync::mpsc::UnboundedReceiver<ResourcesPersistenceCommand>,
        state_path: PathBuf,
    ) {
        let mut pending: Option<serde_json::Value> = None;
        let mut debounce = tokio::time::interval(Duration::from_millis(500));
        debounce.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                cmd = rx.recv() => {
                    match cmd {
                        Some(ResourcesPersistenceCommand::Save(snapshot)) => {
                            pending = Some(snapshot);
                        }
                        Some(ResourcesPersistenceCommand::Flush(done)) => {
                            if let Some(snapshot) = pending.take() {
                                Self::write_json(&state_path, &snapshot).await;
                            }
                            let _ = done.send(());
                        }
                        None => {
                            if let Some(snapshot) = pending.take() {
                                Self::write_json(&state_path, &snapshot).await;
                            }
                            break;
                        }
                    }
                }
                _ = debounce.tick() => {
                    if let Some(snapshot) = pending.take() {
                        Self::write_json(&state_path, &snapshot).await;
                    }
                }
            }
        }
    }

    async fn write_json(path: &Path, value: &serde_json::Value) {
        match serde_json::to_string_pretty(value) {
            Ok(json) => {
                let tmp_path = path.with_extension("json.tmp");
                if let Err(e) = tokio::fs::write(&tmp_path, json.as_bytes()).await {
                    tracing::warn!("Failed to write resources state to {:?}: {}", tmp_path, e);
                    return;
                }
                // Guard: if a previous bug left a directory at `path`, remove it
                // so the atomic rename can succeed.
                if path.is_dir() {
                    tracing::warn!(
                        "Resources state path {:?} is a directory — removing before write",
                        path
                    );
                    let _ = tokio::fs::remove_dir_all(path).await;
                }
                if let Err(e) = tokio::fs::rename(&tmp_path, path).await {
                    tracing::warn!(
                        "Failed to rename resources state {:?} -> {:?}: {}",
                        tmp_path,
                        path,
                        e
                    );
                }
            }
            Err(e) => {
                tracing::warn!("Failed to serialize resources state: {}", e);
            }
        }
    }
}

// Old `PersistenceLayer` / `PersistenceRunner` deleted.
// ToolState persistence replaced by Resources persistence via `ResourcesPersistence`.

#[cfg(test)]
mod tests {
    use super::*;

    // ResourcesPersistence tests
    // -----------------------------------------------------------------------

    use crate::types::resources::{Resources, State, WebCitationCounter};

    #[tokio::test]
    async fn resources_save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("resources_state.json");

        let persistence = ResourcesPersistence::new(state_path);

        // Build resources with registered state types
        let mut resources = Resources::new();
        resources.register_state::<WebCitationCounter>();

        // Populate WebCitationCounter
        {
            let counter = resources.get_or_default::<State<WebCitationCounter>>();
            counter.counter = 7;
        }

        // Save and flush
        persistence.save(&resources);
        persistence.flush().await;

        // Load into fresh resources (with same registrations)
        let mut restored = Resources::new();
        restored.register_state::<WebCitationCounter>();
        assert!(persistence.load(&mut restored));

        // Verify WebCitationCounter roundtripped
        let counter = restored.get::<State<WebCitationCounter>>().unwrap();
        assert_eq!(counter.counter, 7);
    }

    #[tokio::test]
    async fn resources_load_returns_false_when_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("nonexistent.json");

        let persistence = ResourcesPersistence::new(state_path);
        let mut resources = Resources::new();
        assert!(!persistence.load(&mut resources));
    }

    #[tokio::test]
    async fn resources_load_returns_false_on_corrupt_json() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("resources_state.json");
        std::fs::write(&state_path, "{ this is not valid json }").unwrap();

        let persistence = ResourcesPersistence::new(state_path);
        let mut resources = Resources::new();
        assert!(!persistence.load(&mut resources));
    }

    /// Atomic-rename guarantee: a concurrent reader hammering the path while
    /// the writer streams 200 snapshots must never observe torn JSON.
    #[tokio::test]
    async fn writer_atomic_rename_never_exposes_partial_json() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("resources_state.json");
        let persistence = ResourcesPersistence::new(state_path.clone());

        let mut resources = Resources::new();
        resources.register_state::<WebCitationCounter>();

        let done = std::sync::Arc::new(AtomicBool::new(false));
        let reader_done = done.clone();
        let reader_path = state_path.clone();
        let reader = tokio::spawn(async move {
            while !reader_done.load(Ordering::Relaxed) {
                if let Ok(s) = tokio::fs::read_to_string(&reader_path).await {
                    assert!(
                        serde_json::from_str::<serde_json::Value>(&s).is_ok(),
                        "reader observed a torn/partial write (atomic-rename violated): {s:?}"
                    );
                }
                tokio::task::yield_now().await;
            }
        });

        for i in 0..200u64 {
            {
                let counter = resources.get_or_default::<State<WebCitationCounter>>();
                counter.counter = i as u32;
            }
            persistence.save(&resources);
            persistence.flush().await;
        }

        done.store(true, Ordering::Relaxed);
        reader.await.unwrap();

        // Final state is intact and reflects the last write.
        let content = std::fs::read_to_string(&state_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert!(parsed["state"]["grok_build.WebCitation"].is_object());
    }

    #[tokio::test]
    async fn resources_flush_writes_pending() {
        let dir = tempfile::tempdir().unwrap();
        let state_path = dir.path().join("resources_state.json");

        let persistence = ResourcesPersistence::new(state_path.clone());

        let mut resources = Resources::new();
        resources.register_state::<WebCitationCounter>();
        {
            let counter = resources.get_or_default::<State<WebCitationCounter>>();
            counter.counter = 42;
        }

        persistence.save(&resources);
        persistence.flush().await;

        // File should exist with correct structure
        assert!(state_path.exists());
        let content = std::fs::read_to_string(&state_path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&content).unwrap();
        // Should have "state" category with "grok_build.WebCitation" key
        assert!(parsed["state"]["grok_build.WebCitation"].is_object());
    }
}
