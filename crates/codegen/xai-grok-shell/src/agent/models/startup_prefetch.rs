use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use super::{EarlyPrefetchResult, ModelsPrefetch, SettingsCacheWrite};
use crate::agent::config::{self, Config, ModelEntry};
use crate::auth::{AuthManager, GrokAuth, GrokComConfig};
use crate::util::config::RemoteSettings;

static INFLIGHT: Mutex<Option<Arc<Inflight>>> = Mutex::new(None);
const ACCEPT_DEADLINE: Duration = Duration::from_millis(
    crate::http::STARTUP_FETCH_TIMEOUT.as_millis() as u64
        * (2 + crate::http::SETTINGS_FETCH_MAX_ATTEMPTS as u64)
        + 5_000,
);

struct Receipt {
    model_id: String,
    model_base_url: String,
    models_origin: String,
    auth_identity: [u8; 32],
}

struct Inflight {
    origin: String,
    receipt: Option<Receipt>,
    state: Mutex<State>,
    done: Condvar,
}

#[derive(Default)]
struct State {
    finished: bool,
    panicked: bool,
    models: Option<ModelsPrefetch>,
    settings: Option<RemoteSettings>,
    settings_write: Option<SettingsCacheWrite>,
}

struct FinishGuard(Arc<Inflight>);

impl Drop for FinishGuard {
    fn drop(&mut self) {
        let mut state = self
            .0
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.finished = true;
        state.panicked = std::thread::panicking();
        drop(state);
        self.0.done.notify_all();
    }
}

fn disk_config() -> Option<Config> {
    let raw = crate::config::load_effective_config_disk_only().ok()?;
    Config::new_from_toml_cfg(&raw).ok()
}

fn selected_xai_model(config: &Config) -> Option<(String, ModelEntry)> {
    let model_id = config
        .default_model_override
        .as_deref()
        .or(config.models.default.as_deref())?;
    let catalog = config::resolve_model_list(config, None);
    let entry = config::find_model_by_id(&catalog, model_id)?;
    if entry.info.provider != xai_grok_sampling_types::ModelProvider::Xai
        || entry.has_own_credentials()
        || entry.api_key.is_some()
        || entry.env_key.is_some()
        || entry.auth_provider.is_some()
        || config.endpoints.has_custom_endpoint()
    {
        return None;
    }
    Some((model_id.to_owned(), entry.clone()))
}

pub(crate) fn uses_xai_session(config: &Config) -> bool {
    !crate::agent::auth_method::has_xai_api_key_env() && selected_xai_model(config).is_some()
}

fn disk_auth(config: &Config) -> Option<GrokAuth> {
    AuthManager::new(
        &crate::util::grok_home::grok_home(),
        config.grok_com_config.clone(),
    )
    .current()
}

fn auth_identity(auth: &GrokAuth) -> [u8; 32] {
    use sha2::Digest;
    let mut digest = sha2::Sha256::new();
    digest.update(auth.user_id.as_bytes());
    digest.update([0]);
    digest.update(auth.key.as_bytes());
    digest.finalize().into()
}

pub fn begin_before_policy_gate(config: &Config) -> bool {
    if config.remote_settings.is_some() {
        return false;
    }
    begin_inner(config, || disk_auth(config))
}

pub fn begin(grok_com_config: Option<GrokComConfig>) -> bool {
    let Some(mut config) = disk_config() else {
        return false;
    };
    if let Some(grok_com_config) = grok_com_config {
        config.grok_com_config = grok_com_config;
    }
    begin_before_policy_gate(&config)
}

pub fn begin_with_auth(auth: Option<GrokAuth>) -> bool {
    let Some(config) = disk_config() else {
        return false;
    };
    begin_with_config_and_auth(&config, auth)
}

pub fn begin_with_config_and_auth(config: &Config, auth: Option<GrokAuth>) -> bool {
    begin_inner(config, move || auth)
}

fn begin_inner(config: &Config, auth: impl FnOnce() -> Option<GrokAuth>) -> bool {
    if crate::managed_config::policy_repair_pending()
        || crate::agent::auth_method::has_xai_api_key_env()
        || !crate::util::config::resolve_remote_fetch_enabled()
    {
        return false;
    }
    let Some((model_id, entry)) = selected_xai_model(config) else {
        return false;
    };
    if let Some(cell) = INFLIGHT.lock().unwrap().as_ref() {
        return cell.origin == config.endpoints.proxy_url()
            && cell
                .receipt
                .as_ref()
                .is_none_or(|receipt| receipt.model_id == model_id);
    }
    if cfg!(test) {
        return false;
    }
    let Some(auth) = auth().filter(GrokAuth::is_managed_mcp_eligible) else {
        return false;
    };
    let identity = auth_identity(&auth);
    let Some(env) =
        super::resolve_prefetch_env_from_parts(Some(auth), config.endpoints.clone(), true)
    else {
        return false;
    };
    let mut registry = INFLIGHT.lock().unwrap();
    if let Some(cell) = registry.as_ref() {
        return cell.origin == env.endpoints.proxy_url()
            && cell
                .receipt
                .as_ref()
                .is_none_or(|receipt| receipt.model_id == model_id);
    }
    let cell = Arc::new(Inflight {
        origin: env.endpoints.proxy_url(),
        receipt: Some(Receipt {
            model_id,
            model_base_url: entry.info.base_url,
            models_origin: crate::remote::models_list_url(&env.endpoints, env.model_fetch_auth),
            auth_identity: identity,
        }),
        state: Mutex::new(State::default()),
        done: Condvar::new(),
    });
    let worker_cell = cell.clone();
    std::thread::spawn(move || {
        let _guard = FinishGuard(worker_cell.clone());
        let (models, settings, settings_write) = super::run_prefetch(env);
        let mut state = worker_cell.state.lock().unwrap();
        state.models = Some(models);
        state.settings = settings;
        state.settings_write = settings_write;
    });
    *registry = Some(cell);
    true
}

struct Finished(Arc<Inflight>);

fn wait_finished(cell: Arc<Inflight>, timeout: Duration) -> Option<Finished> {
    let (state, _) = cell
        .done
        .wait_timeout_while(cell.state.lock().unwrap(), timeout, |state| !state.finished)
        .unwrap();
    let finished = state.finished;
    drop(state);
    finished.then_some(Finished(cell))
}

impl Finished {
    fn take(self) -> State {
        let mut registry = INFLIGHT.lock().unwrap();
        if registry
            .as_ref()
            .is_some_and(|cell| Arc::ptr_eq(cell, &self.0))
        {
            registry.take();
        }
        drop(registry);
        std::mem::take(&mut *self.0.state.lock().unwrap())
    }
}

fn still_accepted(cell: &Inflight, config: Option<&Config>) -> bool {
    if !crate::util::config::resolve_remote_fetch_enabled()
        || crate::managed_config::policy_repair_pending()
        || crate::agent::auth_method::has_xai_api_key_env()
    {
        return false;
    }
    let Some(receipt) = cell.receipt.as_ref() else {
        return cfg!(any(test, feature = "test-support"))
            && cell.origin == config::EndpointsConfig::from_effective_config().proxy_url();
    };
    let mut current = match disk_config() {
        Some(config) => config,
        None => return false,
    };
    current.default_model_override = config
        .and_then(|config| {
            config
                .default_model_override
                .as_ref()
                .or(config.models.default.as_ref())
        })
        .cloned()
        .or_else(|| Some(receipt.model_id.clone()));
    let Some((model_id, entry)) = selected_xai_model(&current) else {
        return false;
    };
    let Some(auth) = disk_auth(&current).filter(GrokAuth::is_managed_mcp_eligible) else {
        return false;
    };
    cell.origin == current.endpoints.proxy_url()
        && model_id == receipt.model_id
        && entry.info.base_url == receipt.model_base_url
        && crate::remote::models_list_url(
            &current.endpoints,
            super::ModelFetchAuth::resolve(&current.endpoints, true),
        ) == receipt.models_origin
        && auth_identity(&auth) == receipt.auth_identity
}

pub fn wait_settings(timeout: Duration) -> Option<RemoteSettings> {
    wait_settings_if(timeout, |cell| still_accepted(cell, None))
}

fn wait_settings_if(
    timeout: Duration,
    accepted: impl Fn(&Inflight) -> bool,
) -> Option<RemoteSettings> {
    let cell = INFLIGHT.lock().unwrap().clone()?;
    if !accepted(&cell) {
        return None;
    }
    let finished = wait_finished(cell, timeout)?;
    if !accepted(&finished.0) {
        return None;
    }
    finished.0.state.lock().unwrap().settings.clone()
}

pub(crate) enum Accept {
    Consumed(Option<Box<RemoteSettings>>),
    Miss,
}

pub(crate) fn accept() -> Accept {
    accept_with_deadline(ACCEPT_DEADLINE, None)
}

pub(crate) fn accept_for_config(config: &Config) -> Accept {
    accept_with_deadline(ACCEPT_DEADLINE, Some(config))
}

fn accept_with_deadline(timeout: Duration, config: Option<&Config>) -> Accept {
    accept_if(timeout, |cell| still_accepted(cell, config))
}

fn accept_if(timeout: Duration, accepted: impl Fn(&Inflight) -> bool) -> Accept {
    let Some(cell) = INFLIGHT.lock().unwrap().clone() else {
        return Accept::Miss;
    };
    let Some(finished) = wait_finished(cell, timeout) else {
        tracing::warn!("startup prefetch exceeded its deadline; keeping the fetch registered");
        return Accept::Consumed(None);
    };
    let accepted = accepted(&finished.0);
    let mut state = finished.take();
    if !accepted {
        return Accept::Miss;
    }
    if state.panicked {
        tracing::warn!("startup prefetch worker panicked");
    }
    if let Some(models) = state.models.take() {
        models.commit();
    }
    if let Some(write) = state.settings_write.take() {
        write.commit();
    }
    Accept::Consumed(state.settings.take().map(Box::new))
}

pub(super) fn wait_early_result() -> EarlyPrefetchResult {
    let empty = || EarlyPrefetchResult {
        models: None,
        settings: None,
    };
    let Some(cell) = INFLIGHT.lock().unwrap().clone() else {
        return empty();
    };
    let Some(finished) = wait_finished(cell, ACCEPT_DEADLINE) else {
        return empty();
    };
    if !still_accepted(&finished.0, None) {
        return empty();
    }
    let state = finished.0.state.lock().unwrap();
    EarlyPrefetchResult {
        models: state
            .models
            .as_ref()
            .and_then(ModelsPrefetch::models)
            .cloned(),
        settings: state.settings.clone(),
    }
}

#[cfg(any(test, feature = "test-support"))]
pub fn clear_for_tests() {
    INFLIGHT.lock().unwrap().take();
}

#[cfg(any(test, feature = "test-support"))]
pub fn inject_for_tests(settings: Option<RemoteSettings>) {
    inject_with_origin_for_tests(
        settings,
        config::EndpointsConfig::from_effective_config().proxy_url(),
    );
}

#[cfg(any(test, feature = "test-support"))]
pub fn inject_with_origin_for_tests(settings: Option<RemoteSettings>, origin: String) {
    *INFLIGHT.lock().unwrap() = Some(Arc::new(Inflight {
        origin,
        receipt: None,
        state: Mutex::new(State {
            finished: true,
            settings,
            ..State::default()
        }),
        done: Condvar::new(),
    }));
}

#[cfg(any(test, feature = "test-support"))]
pub fn inflight_for_tests() -> bool {
    INFLIGHT.lock().unwrap().is_some()
}

#[cfg(test)]
#[path = "startup_prefetch_tests.rs"]
mod tests;
