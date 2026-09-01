use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex};

const MAX_ENTRIES: usize = 32;

#[derive(Default)]
struct Entry {
    slot: Arc<Mutex<Option<reqwest::Client>>>,
    last_used: u64,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(crate) struct CacheKey(String);

pub(crate) fn cached_client<E>(
    key: CacheKey,
    build: impl FnOnce() -> Result<reqwest::Client, E>,
) -> Result<reqwest::Client, E> {
    static CACHE: LazyLock<Mutex<(u64, HashMap<CacheKey, Entry>)>> =
        LazyLock::new(Default::default);
    let slot = {
        let mut guard = CACHE
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (tick, map) = &mut *guard;
        *tick += 1;
        if !map.contains_key(&key)
            && map.len() >= MAX_ENTRIES
            && let Some(lru) = map
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(header_name, _)| header_name.clone())
        {
            map.remove(&lru);
        }
        let entry = map.entry(key).or_default();
        entry.last_used = *tick;
        entry.slot.clone()
    };
    let mut slot = slot
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(client) = &*slot {
        return Ok(client.clone());
    }
    let built = build()?;
    *slot = Some(built.clone());
    Ok(built)
}

pub(crate) fn cache_key(kind: &str, headers: &reqwest::header::HeaderMap) -> CacheKey {
    CacheKey(format!(
        "{}|{}",
        xai_file_utils::sha256_hex(kind.as_bytes()),
        headers_fingerprint(headers),
    ))
}

fn headers_fingerprint(headers: &reqwest::header::HeaderMap) -> String {
    let mut pairs: Vec<(&str, &[u8])> = headers
        .iter()
        .map(|(header_name, header_value)| (header_name.as_str(), header_value.as_bytes()))
        .collect();
    pairs.sort();
    let mut bytes = Vec::new();
    for (name, value) in pairs {
        bytes.extend_from_slice(&(name.len() as u64).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(&(value.len() as u64).to_le_bytes());
        bytes.extend_from_slice(value);
    }
    xai_file_utils::sha256_hex(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use reqwest::header::{HeaderMap, HeaderValue};

    #[test]
    fn routing_and_header_boundaries_partition_cache_keys() {
        let headers = HeaderMap::new();
        assert!(
            cache_key("image_gen:Grok:https://example.test", &headers)
                != cache_key("image_gen:OpenAi:https://example.test", &headers)
        );
        assert!(
            cache_key("web_search:Responses:https://one.test", &headers)
                != cache_key("web_search:Responses:https://two.test", &headers)
        );
        let mut first = HeaderMap::new();
        first.insert("ab", HeaderValue::from_static("c"));
        let mut second = HeaderMap::new();
        second.insert("a", HeaderValue::from_static("bc"));
        assert!(cache_key("route", &first) != cache_key("route", &second));
        assert!(
            !cache_key("https://user:secret@example.test", &headers)
                .0
                .contains("secret")
        );
    }

    static TEST_LOCK: Mutex<()> = Mutex::new(());
    fn lock() -> std::sync::MutexGuard<'static, ()> {
        TEST_LOCK.lock().unwrap_or_else(|entry| entry.into_inner())
    }

    #[test]
    fn any_changed_header_misses_the_cache() {
        let _g = lock();
        let mut h1 = HeaderMap::new();
        h1.insert("authorization", HeaderValue::from_static("Bearer old"));
        h1.insert("x-extra", HeaderValue::from_static("header_value"));
        let mut rotated = h1.clone();
        rotated.insert("authorization", HeaderValue::from_static("Bearer new"));
        let mut extra = h1.clone();
        extra.insert("x-extra", HeaderValue::from_static("v2"));

        let _ = cached_client::<()>(cache_key("rot", &h1), || Ok(reqwest::Client::new()));
        for headers in [&rotated, &extra] {
            let mut built = false;
            let _ = cached_client::<()>(cache_key("rot", headers), || {
                built = true;
                Ok(reqwest::Client::new())
            });
            assert!(built, "changed header must miss the cache");
        }
    }

    #[test]
    fn build_error_is_propagated_and_not_cached() {
        let _g = lock();
        let key = cache_key("header_name-err", &HeaderMap::new());
        let err = cached_client::<&str>(key.clone(), || Err("boom"));
        assert_eq!(err.unwrap_err(), "boom");
        let mut built = false;
        let ok = cached_client::<&str>(key, || {
            built = true;
            Ok(reqwest::Client::new())
        });
        assert!(ok.is_ok() && built, "error must not poison the key");
    }

    #[test]
    fn concurrent_misses_coalesce_on_one_build() {
        let _g = lock();
        use std::sync::Barrier;
        use std::sync::atomic::{AtomicUsize, Ordering};
        let builds = Arc::new(AtomicUsize::new(0));
        let in_build = Arc::new(Barrier::new(2));
        let spawn = |builds: Arc<AtomicUsize>, gate: Arc<Barrier>| {
            std::thread::spawn(move || {
                cached_client::<()>(cache_key("header_name-flight", &HeaderMap::new()), || {
                    builds.fetch_add(1, Ordering::SeqCst);
                    gate.wait();
                    Ok(reqwest::Client::new())
                })
                .unwrap();
            })
        };
        let t1 = spawn(builds.clone(), in_build.clone());
        let t2 = spawn(builds.clone(), in_build.clone());
        in_build.wait();
        t1.join().unwrap();
        t2.join().unwrap();
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "siblings must share one build"
        );
    }

    #[test]
    fn cap_evicts_least_recently_used() {
        let _g = lock();
        for index in 0..MAX_ENTRIES {
            let _ = cached_client::<()>(
                cache_key(&format!("lru-{index}"), &HeaderMap::new()),
                || Ok(reqwest::Client::new()),
            );
        }
        let _ = cached_client::<()>(cache_key("lru-0", &HeaderMap::new()), || panic!("must hit"));
        let _ = cached_client::<()>(cache_key("lru-overflow", &HeaderMap::new()), || {
            Ok(reqwest::Client::new())
        });
        let mut rebuilt_0 = false;
        let _ = cached_client::<()>(cache_key("lru-0", &HeaderMap::new()), || {
            rebuilt_0 = true;
            Ok(reqwest::Client::new())
        });
        assert!(!rebuilt_0, "recently-used entry must survive the cap");
    }
}
