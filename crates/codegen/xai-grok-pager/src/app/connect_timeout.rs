use std::time::Duration;

macro_rules! connect_ui_timeout_env {
    () => {
        "GROK_CONNECT_UI_TIMEOUT_SECS"
    };
}

pub(super) const CONNECT_UI_TIMEOUT_ENV: &str = connect_ui_timeout_env!();
pub(super) const CONNECT_UI_TIMEOUT_TRY_COMMAND: &str =
    concat!(connect_ui_timeout_env!(), "=60 open-grok");
pub(super) const DEFAULT_CONNECT_UI_TIMEOUT: Duration = Duration::from_secs(30);
const MIN_CONNECT_UI_TIMEOUT_SECS: u64 = 5;

pub(super) fn resolve(env: Option<&str>) -> Duration {
    match env
        .map(str::trim)
        .and_then(|value| value.parse::<u64>().ok())
    {
        None | Some(0) => DEFAULT_CONNECT_UI_TIMEOUT,
        Some(secs) => Duration::from_secs(secs.max(MIN_CONNECT_UI_TIMEOUT_SECS)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_cases() {
        assert_eq!(resolve(None), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("")), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some(" 45 ")), Duration::from_secs(45));
        assert_eq!(resolve(Some("0")), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("garbage")), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("-5")), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("1e3")), DEFAULT_CONNECT_UI_TIMEOUT);
        assert_eq!(resolve(Some("1")), Duration::from_secs(5));
        assert_eq!(resolve(Some("9999")), Duration::from_secs(9999));
    }
}
