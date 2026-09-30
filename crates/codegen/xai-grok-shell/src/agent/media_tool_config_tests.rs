use pretty_assertions::assert_eq;
use xai_grok_tools::implementations::grok_build::image_gen::ImageGenConfig;
use xai_grok_tools::implementations::grok_build::video_gen::VideoGenConfig;

use super::{MediaToolCredentials, image_gen_config, load_media_tool_config, video_gen_config};

fn config(toml: &str) -> super::Config {
    load_media_tool_config(&toml::from_str(toml).unwrap(), None).unwrap()
}

fn credentials(static_bearer: Option<&str>) -> MediaToolCredentials {
    MediaToolCredentials {
        static_bearer: static_bearer.map(str::to_owned),
        tier_restricted: false,
    }
}

#[test]
fn the_static_bearer_is_the_configured_key_and_nothing_else() {
    let cfg = config("[endpoints]\nxai_api_base_url = \"https://api.x.ai/v1\"\n");

    let ImageGenConfig::Enabled {
        api_key, base_url, ..
    } = image_gen_config(&cfg, &credentials(None))
    else {
        panic!("image tools are enabled by default");
    };
    assert_eq!(None, api_key);
    assert_eq!("https://api.x.ai/v1", base_url);

    let ImageGenConfig::Enabled { api_key, .. } =
        image_gen_config(&cfg, &credentials(Some("xai-key")))
    else {
        panic!("image tools are enabled by default");
    };
    assert_eq!(Some("xai-key".to_owned()), api_key);
}

/// Runtime resolution is what computes the ZDR guard; a host that skipped it would let a ZDR customer's
/// video download locally.
#[test]
fn a_zdr_config_without_a_bucket_restricts_video_on_every_host() {
    let cfg = config("[tools]\ndisable_zdr_incompatible_tools = true\n");

    let VideoGenConfig::Enabled {
        zdr_restricted,
        zdr_video_output_s3,
        ..
    } = video_gen_config(&cfg, &credentials(None))
    else {
        panic!("video stays advertised under ZDR so the call can explain itself");
    };
    assert!(zdr_restricted);
    assert!(zdr_video_output_s3.is_none());
}

#[test]
fn media_headers_identify_build_traffic() {
    let cfg = config("");
    let ImageGenConfig::Enabled { extra_headers, .. } = image_gen_config(&cfg, &credentials(None))
    else {
        panic!("image tools are enabled by default");
    };
    assert!(extra_headers.contains_key("x-grok-client-identifier"));
    assert!(extra_headers.contains_key("x-grok-client-version"));
    assert!(
        extra_headers
            .get("user-agent")
            .is_some_and(|agent| agent.starts_with("xai-grok-build/")),
        "{extra_headers:?}"
    );
}
