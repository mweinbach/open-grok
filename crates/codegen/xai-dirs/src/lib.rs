use std::path::PathBuf;

#[allow(deprecated, clippy::disallowed_methods)]
pub fn home_dir() -> Option<PathBuf> {
    std::env::home_dir()
}

#[cfg(test)]
mod tests {
    #[test]
    #[allow(deprecated, clippy::disallowed_methods)]
    fn home_directory_matches_platform_environment_resolution() {
        assert_eq!(super::home_dir(), std::env::home_dir());
    }
}
