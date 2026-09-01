//! Store creation, connection opening, permission setup, and initial schema gating.

use std::path::{Path, PathBuf};
use std::time::Instant;

use xai_sqlite_journal::JournalMode;

use super::WorkspaceStore;
use crate::error::{Result, classify_open_error};
use crate::owner_only::{create_owner_only, sibling_path, tighten_owner_only};
use crate::schema::{self, SchemaInit, USER_VERSION, read_user_version};
use crate::types::SchemaState;

impl WorkspaceStore {
    /// Create-or-open the store at `db_path`.
    /// Creates the parent directory 0700 and the database file 0600 before SQLite ever opens it; Windows keeps the default profile ACLs.
    /// Opens through [`JournalMode`]: WAL locally, TRUNCATE plus a per-host file on network mounts, where each host then has its own workspace.
    /// Re-tightens file and journal-sibling modes, and initializes or version-gates the schema.
    ///
    /// # Errors
    ///
    /// [`crate::StoreError::Unusable`] when the file is not a SQLite database (never deleted or recreated),
    /// [`crate::StoreError::Busy`] when the busy budget elapses,
    /// [`crate::StoreError::Io`] on directory/mode failures or when the path is not a regular file (e.g. a planted symlink),
    /// [`crate::StoreError::Sqlite`] otherwise.
    pub fn open(db_path: &Path) -> Result<Self> {
        if let Some(parent) = db_path.parent() {
            xai_grok_config::create_dir_all_owner_only(parent)?;
        }
        let mode = JournalMode::for_db_path(db_path);
        let effective = mode.effective_db_path(db_path);

        create_owner_only(&effective)?;
        let opened_at = Instant::now();
        let mut conn = mode
            .open(db_path)
            .map_err(|error| classify_open_error(error.into(), opened_at, &effective))?;
        tighten_owner_only(&effective)?;
        for suffix in ["-wal", "-shm", "-journal"] {
            tighten_owner_only(&sibling_path(&effective, suffix))?;
        }

        let found = read_user_version(&conn)
            .map_err(|error| classify_open_error(error, opened_at, &effective))?;
        if found > USER_VERSION {
            return Self::open_newer_schema(conn, effective, found, opened_at);
        }
        match schema::init_schema(&mut conn)
            .map_err(|error| classify_open_error(error, opened_at, &effective))?
        {
            SchemaInit::Newer { user_version } => {
                Self::open_newer_schema(conn, effective, user_version, opened_at)
            }
            SchemaInit::Ready { created } => {
                if created {
                    tracing::info!(
                        path = %effective.display(),
                        journal_mode = mode.as_str(),
                        user_version = USER_VERSION,
                        "workspace store created"
                    );
                } else {
                    let member_count: i64 = conn
                        .query_row("SELECT COUNT(*) FROM members", [], |row| row.get(0))
                        .map_err(|error| {
                            classify_open_error(error.into(), opened_at, &effective)
                        })?;
                    tracing::debug!(
                        path = %effective.display(),
                        member_count,
                        "workspace store opened"
                    );
                }
                Ok(Self {
                    conn,
                    schema: SchemaState::Current,
                    path: effective,
                })
            }
        }
    }

    /// Finish an open of a file written by a newer Open Grok: gate at the connection so even a bug in this crate cannot write.
    /// Reads remain available while the newer schema is read-compatible.
    fn open_newer_schema(
        conn: rusqlite::Connection,
        effective: PathBuf,
        found: u32,
        opened_at: Instant,
    ) -> Result<Self> {
        conn.pragma_update(None, "query_only", true)
            .map_err(|error| classify_open_error(error.into(), opened_at, &effective))?;
        tracing::warn!(
            path = %effective.display(),
            found,
            supported = USER_VERSION,
            "workspace store written by a newer Open Grok; opening read-only"
        );
        Ok(Self {
            conn,
            schema: SchemaState::NewerReadOnly {
                user_version: found,
            },
            path: effective,
        })
    }
}

#[cfg(test)]
#[path = "store_open_tests.rs"]
mod tests;
