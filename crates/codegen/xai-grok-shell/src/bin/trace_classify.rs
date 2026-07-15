//! Replay an offline session trace against the Layer-2 TodoGate and
//! Layer-3 LazinessDetector classifier, emitting one JSONL line per
//! turn.
//!
//! Usage:
//!   cargo run --bin trace_classify -- \
//!       --trace /path/to/trace-<id>-all-turns.json \
//!       [--output out.jsonl] \
//!       [--model grok-4.5] \
//!       [--api-base-url https://api.x.ai/v1] \
//!       [--api-key <key> | $XAI_API_KEY | <grok-home>/auth.json] \
//!       [--min-confidence 0.7] \
//!       [--include-reasoning true] \
//!       [--grok-home <path>]
//!
//! The binary name is `trace_classify` (underscore) — that's the file
//! name in `src/bin/`, which cargo's auto-discovery uses verbatim.
//! The task brief calls it `trace-classify` (hyphen) in prose; the
//! canonical CLI invocation is the underscore form.
//!
//! Each JSONL line carries the per-turn gate decision, the parsed
//! classifier verdict (or the abort/parse error if the call failed),
//! and the inputs that drove them.

use std::path::PathBuf;

use clap::Parser;
use xai_grok_shell::trace_classifier::{RunArgs, run, validate_min_confidence};

#[derive(Parser, Debug)]
#[command(
    name = "trace_classify",
    about = "Replay a session trace against the TodoGate + Laziness classifier"
)]
struct Cli {
    /// Path to the offline trace JSON (a top-level array of turn records).
    #[arg(long)]
    trace: PathBuf,

    /// Write JSONL output here (one line per turn). Defaults to stdout
    /// when omitted.
    #[arg(long)]
    output: Option<PathBuf>,

    /// Model the classifier sampler calls. Must be a model the API key
    /// has access to.
    #[arg(long, default_value = "grok-4.5")]
    model: String,

    /// Sampler base URL.
    #[arg(long, default_value = "https://api.x.ai/v1")]
    api_base_url: String,

    /// API key. Overrides `$XAI_API_KEY` when set; falls back to
    /// `$XAI_API_KEY`, then `<grok-home>/auth.json` (`xai::api_key`
    /// scope) when absent or empty.
    #[arg(long)]
    api_key: Option<String>,

    /// Override the LazinessDetector min-confidence threshold (default
    /// matches production's `LAZINESS_DEFAULT_MIN_CONFIDENCE`). Must
    /// be a finite float in `[0.0, 1.0]`. Use this to mirror a
    /// per-model override from the production models catalog. (F6/N5)
    #[arg(long, value_parser = validate_min_confidence)]
    min_confidence: Option<f32>,

    /// Override the harness `[assistant reasoning]` emission flag.
    /// When absent (the default), the binary uses the harness default
    /// `LAZINESS_INCLUDE_REASONING`. Accepts `true` / `false`. The
    /// offline replay tool has no per-model config to consult, so
    /// this is the only override surface here — production resolves
    /// `LazinessDetectorPerModelConfig::include_reasoning` separately.
    #[arg(long)]
    include_reasoning: Option<bool>,

    /// Override the directory containing `auth.json` for the
    /// third-tier API-key fallback. Defaults to the same path the
    /// shell uses (`$GROK_HOME` or `~/.grok`). Exposed primarily for
    /// tests / sandboxed invocations.
    #[arg(long)]
    grok_home: Option<PathBuf>,
}

/// `current_thread` flavour: the replay is strictly sequential
/// (one turn at a time), and a multi-threaded runtime would force
/// every writer (including `StdoutLock`) to be `Send` — which it
/// isn't. The sequential nature also means we never schedule work in
/// parallel, so `current_thread` is the right cost shape too.
#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let args = RunArgs {
        trace: cli.trace,
        output: cli.output,
        model_id: cli.model,
        api_base_url: cli.api_base_url,
        api_key: cli.api_key,
        min_confidence: cli.min_confidence,
        include_reasoning: cli.include_reasoning,
        grok_home: cli.grok_home,
    };
    let summary = run(args).await?;
    eprintln!("{}", summary.render());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn cli_parses_minimal_args() {
        let cli = Cli::try_parse_from(["trace_classify", "--trace", "foo.json", "--model", "bar"])
            .expect("parse");
        assert_eq!(cli.trace, PathBuf::from("foo.json"));
        assert_eq!(cli.model, "bar");
        assert_eq!(cli.api_base_url, "https://api.x.ai/v1");
        assert!(cli.output.is_none());
        assert!(cli.api_key.is_none());
        assert!(cli.min_confidence.is_none());
        assert!(cli.include_reasoning.is_none());
        assert!(cli.grok_home.is_none());
    }

    /// Per-model knob (mirrored as a CLI override on the offline tool):
    /// `--include-reasoning true` and `--include-reasoning false` both
    /// parse; absent → `None` so the harness default applies.
    #[test]
    fn cli_include_reasoning_override_parses() {
        let cli_true = Cli::try_parse_from([
            "trace_classify",
            "--trace",
            "foo.json",
            "--include-reasoning",
            "true",
        ])
        .expect("parse true");
        assert_eq!(cli_true.include_reasoning, Some(true));

        let cli_false = Cli::try_parse_from([
            "trace_classify",
            "--trace",
            "foo.json",
            "--include-reasoning",
            "false",
        ])
        .expect("parse false");
        assert_eq!(cli_false.include_reasoning, Some(false));

        let cli_absent =
            Cli::try_parse_from(["trace_classify", "--trace", "foo.json"]).expect("parse absent");
        assert!(cli_absent.include_reasoning.is_none());
    }

    #[test]
    fn cli_grok_home_override_parses() {
        let cli = Cli::try_parse_from([
            "trace_classify",
            "--trace",
            "foo.json",
            "--grok-home",
            "/tmp/scratch-grok",
        ])
        .expect("parse");
        assert_eq!(cli.grok_home, Some(PathBuf::from("/tmp/scratch-grok")));
    }

    #[test]
    fn cli_requires_trace() {
        let err = Cli::try_parse_from(["trace_classify"]).expect_err("missing --trace");
        let msg = err.to_string();
        assert!(msg.contains("--trace"), "error mentions --trace: {msg}");
    }

    /// F18 — assert the documented defaults actually take effect.
    #[test]
    fn cli_defaults_match_documented_values() {
        let cmd = Cli::command();
        let by_id = |id: &str| {
            cmd.get_arguments()
                .find(|a| a.get_id().as_str() == id)
                .unwrap_or_else(|| panic!("arg {id} missing"))
                .get_default_values()
                .iter()
                .map(|v| v.to_string_lossy().into_owned())
                .collect::<Vec<_>>()
        };
        assert_eq!(by_id("model"), vec!["grok-4.5"]);
        assert_eq!(by_id("api_base_url"), vec!["https://api.x.ai/v1"]);
        assert!(by_id("min_confidence").is_empty(), "no default");
        assert!(by_id("include_reasoning").is_empty(), "no default");
    }

    /// F6 — `--min-confidence 0.5` parses and lands in `RunArgs`.
    #[test]
    fn cli_min_confidence_override_parses() {
        let cli = Cli::try_parse_from([
            "trace_classify",
            "--trace",
            "foo.json",
            "--min-confidence",
            "0.42",
        ])
        .expect("parse");
        assert_eq!(cli.min_confidence, Some(0.42));
    }

    /// N5 — clap `value_parser` rejects out-of-range / non-finite
    /// floats at parse time, before they reach `RunArgs`. Bad values
    /// are passed via `--min-confidence=VALUE` syntax so negative
    /// literals aren't mis-parsed as short flags.
    #[test]
    fn cli_min_confidence_rejects_bad_values() {
        for bad in ["1.5", "-0.1", "nan", "inf", "not-a-float"] {
            let arg = format!("--min-confidence={bad}");
            let err = Cli::try_parse_from(["trace_classify", "--trace", "foo.json", arg.as_str()])
                .expect_err(bad);
            // Parsing failed — that's all we need. Exact error text
            // is clap-version-dependent.
            let _ = err.to_string();
        }
    }
}
