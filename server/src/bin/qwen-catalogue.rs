//! Does a Model Studio region serve the models the Standard tier dials?
//!
//! Run this against a region BEFORE pointing production at it. A key from a region
//! without our realtime models authenticates perfectly and only fails later, per
//! session, with "Access to model denied" — this turns that into a one-line answer.
//!
//! ```text
//! # the primary route, from .env / the process environment
//! cargo run -p voxtranslate-server --bin qwen-catalogue
//!
//! # the configured fallback route (QWEN_FALLBACK_*)
//! cargo run -p voxtranslate-server --bin qwen-catalogue -- --fallback
//!
//! # a region that is not configured anywhere yet — e.g. a fresh Frankfurt key
//! QWEN_REALTIME_ENDPOINT=wss://dashscope.eu-central-1.aliyuncs.com/api-ws/v1/realtime \
//! DASHSCOPE_API_KEY=sk-... \
//!   cargo run -p voxtranslate-server --bin qwen-catalogue
//! ```
//!
//! Exit code is the verdict: `0` the region is usable, `1` it is not, `2` the check
//! itself could not run.

use voxtranslate_server::config::QwenConfig;
use voxtranslate_server::engine::qwen_catalogue;

#[tokio::main]
async fn main() {
    let _ = dotenvy::dotenv();

    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.iter().any(|a| a == "-h" || a == "--help") {
        eprintln!("usage: qwen-catalogue [--fallback]\n");
        eprintln!("Reads the same QWEN_*/DASHSCOPE_* variables the server does and asks the");
        eprintln!("region's /compatible-mode/v1/models whether it serves both models we dial.");
        std::process::exit(2);
    }

    let primary = QwenConfig::from_env();
    let want_fallback = args.iter().any(|a| a == "--fallback");

    let config = if want_fallback {
        match primary.fallback_config() {
            Some(c) => c,
            None => {
                eprintln!("no fallback route configured — set QWEN_FALLBACK_ENDPOINT and/or QWEN_FALLBACK_API_KEY");
                std::process::exit(2);
            }
        }
    } else {
        primary
    };

    if config.api_key.trim().is_empty() {
        eprintln!("no API key — set DASHSCOPE_API_KEY (or QWEN_API_KEY)");
        std::process::exit(2);
    }

    let required = qwen_catalogue::required_models(&config);
    println!(
        "route     : {}",
        if want_fallback { "FALLBACK" } else { "primary" }
    );
    println!("endpoint  : {}", config.endpoint);
    println!(
        "workspace : {}",
        config.workspace_id.as_deref().unwrap_or("(none)")
    );
    // Key SHAPE only, never the value — same discipline as the live protocol probe.
    println!(
        "key       : len={} prefix={}…",
        config.api_key.len(),
        config.api_key.chars().take(3).collect::<String>()
    );

    let report = match qwen_catalogue::check(
        &config.endpoint,
        &config.api_key,
        config.workspace_id.as_deref(),
        &required,
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("\nFAILED: {e}");
            std::process::exit(2);
        }
    };

    println!("catalogue : {}", report.url);
    println!("serves    : {} models", report.available);
    for m in &report.required {
        let ok = !report.missing.contains(m);
        println!("  [{}] {m}", if ok { "ok " } else { "MISSING" });
    }

    if report.ok() {
        println!("\nOK — this region serves both models. Safe to point the Standard tier at it.");
        return;
    }

    println!(
        "\nNOT USABLE — this region is missing: {}",
        report.missing.join(", ")
    );
    println!("A key here authenticates fine and then denies the model at session.update.");
    println!("Either pick another region, or override the id (QWEN_REALTIME_MODEL /");
    println!("QWEN_ASR_MODEL) if this region publishes the same model under another name.");
    std::process::exit(1);
}
