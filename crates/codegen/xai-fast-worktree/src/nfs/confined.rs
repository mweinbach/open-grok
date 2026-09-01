//! Fd-relative helpers + the single owned deleter used by daemon-down `rm`
//! and `clean-artifacts`. Never a weaker sibling of `grove_git::delete_owned`.
pub fn is_safe_worktree_id(id: &str) -> bool {
    !id.is_empty()
        && !id.starts_with('.')
        && !id.contains("..")
        && id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
}

#[cfg(test)]
mod tests {
    use super::is_safe_worktree_id;

    #[test]
    fn ids_cannot_escape_the_worktree_pin_namespace() {
        for invalid in [
            "",
            ".hidden",
            "../main",
            "wt..main",
            "heads/main",
            "heads\\main",
            "wt@{1}",
            "wt\0id",
            "wt id",
        ] {
            assert!(
                !is_safe_worktree_id(invalid),
                "accepted unsafe id {invalid:?}"
            );
        }
        assert!(is_safe_worktree_id("worktree-branch_1.2"));
    }
}
