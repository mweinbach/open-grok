//! Content-free local status rendering for the session-pinned memory-v2 stack.

use super::*;

fn opaque_scope_id(scope: &std::path::Path) -> String {
    let digest = blake3::hash(scope.as_os_str().as_encoded_bytes()).to_hex();
    format!("ws-{}", &digest[..12])
}

impl SessionActor {
    pub(super) async fn memory_v2_status(&self) -> String {
        if self.memory.mode() != Some(crate::config::MemoryMode::V2) {
            return "Memory status is available for memory v2. `/memory` keeps the legacy browser behavior."
                .to_owned();
        }
        let Some(storage) = self
            .memory
            .storage()
            .or_else(|| self.memory.configured_storage.clone())
        else {
            return "Memory v2 is not configured for this session.".to_owned();
        };
        if self.memory.v2_config.rollout == crate::config::MemoryV2Rollout::Off
            || !self.memory.v2_config.file_writes_enabled
        {
            return format!(
                "**Memory v2 status**\n\n\
                 - **enabled:** false\n\
                 - **pinned mode:** v2\n\
                 - **rollout:** {}\n\
                 - **capture / automatic Dream / manual Dream / file writes:** false / false / false / {}\n\
                 - **state:** unavailable (disabled fail-closed)",
                self.memory.v2_config.rollout.as_str(),
                self.memory.v2_config.file_writes_enabled,
            );
        }
        let global = storage.global_dir().to_path_buf();
        let workspace = storage.workspace_dir().to_path_buf();
        let session_id = self.session_info.id.to_string();
        let clock = xai_grok_memory::system_v2_clock();
        let result = tokio::task::spawn_blocking({
            let global = global.clone();
            let workspace = workspace.clone();
            move || {
                let global_status = xai_grok_memory::V2MaintenanceStore::open_with_clock(
                    &global,
                    xai_grok_memory::V2MemoryScope::Global,
                    &global,
                    &workspace,
                    clock.clone(),
                )
                .map_err(|_| ())?
                .status_now()
                .map_err(|_| ())?;
                let workspace_store = xai_grok_memory::V2MaintenanceStore::open_with_clock(
                    &workspace,
                    xai_grok_memory::V2MemoryScope::Workspace,
                    &global,
                    &workspace,
                    clock.clone(),
                )
                .map_err(|_| ())?;
                let workspace_status = workspace_store.status_now().map_err(|_| ())?;
                let cursors = xai_grok_memory::V2CaptureStore::open_with_clock(
                    &workspace,
                    xai_grok_memory::V2MemoryScope::Workspace,
                    clock,
                )
                .and_then(|store| {
                    store
                        .ensure_session(&session_id)
                        .and_then(|()| store.cursors(&session_id))
                })
                .map_err(|_| ())?;
                Ok::<_, ()>((global_status, workspace_status, cursors))
            }
        })
        .await;
        let (global_status, workspace_status, cursors) = match result {
            Ok(Ok(status)) => status,
            Ok(Err(_)) | Err(_) => {
                return "Memory v2 status is unavailable (state could not be read safely)."
                    .to_owned();
            }
        };
        let controls = self.memory.v2_config;
        let scope_id = opaque_scope_id(&workspace);
        format!(
            "**Memory v2 status**\n\n\
             - **enabled:** {}\n\
             - **pinned mode:** v2\n\
             - **rollout:** {}\n\
             - **capture / automatic Dream / manual Dream / file writes:** {} / {} / {} / {}\n\
             - **global scope:** shared\n\
             - **workspace scope:** {scope_id}\n\
             - **session cursors requested / captured / indexed:** {} / {} / {}\n\
             - **workspace capture pending / running / retry / terminal-failed / completed:** {} / {} / {} / {} / {}\n\
             - **workspace pending observations exposed / hidden / oldest exposed age seconds:** {} / {} / {}\n\
             - **workspace Dream lease / snapshot / coalesced:** {:?} / {} / {}\n\
             - **workspace last success / failure:** {} / {}\n\
             - **workspace archive / tombstone counts:** {} / {}\n\
             - **global archive / tombstone counts:** {} / {}",
            self.memory.is_enabled(),
            controls.rollout.as_str(),
            controls.can_capture(),
            controls.can_run_automatic_dream(),
            controls.can_run_manual_dream(),
            controls.file_writes_enabled,
            cursors.requested,
            cursors.captured,
            cursors.indexed,
            workspace_status.pending_capture_jobs,
            workspace_status.running_capture_jobs,
            workspace_status.retry_capture_jobs,
            workspace_status.failed_capture_jobs,
            workspace_status.completed_capture_jobs,
            workspace_status.pending_observations,
            workspace_status.hidden_pending_observations,
            workspace_status
                .oldest_pending_age_secs
                .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            workspace_status.dream_lease,
            workspace_status.dream_snapshot_size,
            workspace_status.has_coalesced_trigger,
            workspace_status
                .last_success_at
                .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            workspace_status
                .last_failure_at
                .map_or_else(|| "none".to_owned(), |value| value.to_string()),
            workspace_status.archive_count,
            workspace_status.tombstone_count,
            global_status.archive_count,
            global_status.tombstone_count,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::opaque_scope_id;

    #[test]
    fn scope_identifier_is_stable_opaque_and_content_free() {
        let root = std::path::Path::new("/private/workspaces/secret-project");
        let first = opaque_scope_id(root);
        assert_eq!(first, opaque_scope_id(root));
        assert!(first.starts_with("ws-"));
        assert_eq!(first.len(), 15);
        for forbidden in [
            root.to_string_lossy().as_ref(),
            "secret-project",
            "private observation",
            "SQL error",
            "permission denied",
        ] {
            assert!(!first.contains(forbidden));
        }
    }
}
