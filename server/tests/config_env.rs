//! `Config::from_env` guest-vs-billing detection. This test mutates
//! process-global env (`DATABASE_URL`, etc.), so it lives in its own integration
//! binary — running it inside the lib unit-test binary would race the DB-gated
//! tests that read `DATABASE_URL`.

use voxtranslate_server::config::Config;

#[test]
fn from_env_detects_guest_and_billing_modes() {
    std::env::set_var("DEEPGRAM_API_KEY", "dk");
    std::env::set_var("GROQ_API_KEY", "gk");
    for k in ["DATABASE_URL", "GOOGLE_CLIENT_ID", "JWT_SECRET", "PORT"] {
        std::env::remove_var(k);
    }

    // Guest-only mode (no billing env).
    let c = Config::from_env().unwrap();
    assert_eq!(c.port, 3001);
    assert!(!c.billing_enabled());
    assert_eq!(c.auto_detect_buffer_ms, 3000); // default

    std::env::set_var("AUTO_DETECT_BUFFER_MS", "4500");
    assert_eq!(Config::from_env().unwrap().auto_detect_buffer_ms, 4500);

    // Billing mode activates when the three core values are present.
    std::env::set_var("DATABASE_URL", "postgres://x");
    std::env::set_var("GOOGLE_CLIENT_ID", "gid");
    std::env::set_var("JWT_SECRET", "secret");
    std::env::set_var("COST_PER_MINUTE", "0.01");
    std::env::set_var("MARKUP_PERCENTAGE", "0.5");
    let c = Config::from_env().unwrap();
    let b = c.billing.as_ref().expect("billing enabled");
    assert!((b.pricing.user_rate_per_minute - 0.015).abs() < 1e-9);
    assert_eq!(b.jwt_expiry_hours, 168);

    // AI feature config defaults (no env set).
    assert_eq!(b.glossary_max_entries, 200);
    assert_eq!(b.ai.report_model, "llama-3.3-70b-versatile");
    assert_eq!(b.ai.fallback_model, "llama-3.1-8b-instant");
    assert!((b.ai.report_base - 0.05).abs() < 1e-9);
    assert_eq!(b.ai.suggestions_interval_secs, 15);
    // Resend disabled until all three vars are present.
    assert!(c.resend.is_none());
    std::env::set_var("RESEND_API_KEY", "re_x");
    std::env::set_var("RESEND_FROM_EMAIL", "noreply@vox.example");
    assert!(Config::from_env().unwrap().resend.is_none()); // still missing name
    std::env::set_var("RESEND_FROM_NAME", "VoxTranslate");
    std::env::set_var("CREDITS_REPORT_BASE", "0.10");
    std::env::set_var("GLOSSARY_MAX_ENTRIES", "50");
    let c = Config::from_env().unwrap();
    let b = c.billing.as_ref().unwrap();
    let r = c.resend.as_ref().expect("resend enabled");
    assert_eq!(r.from_email, "noreply@vox.example");
    assert!((b.ai.report_base - 0.10).abs() < 1e-9);
    assert_eq!(b.glossary_max_entries, 50);

    // Chat file upload storage (spec 0018): disabled until BOTH URL + key set.
    std::env::set_var("DEEPGRAM_API_KEY", "dk"); // restore (overwritten below)
    assert!(Config::from_env().unwrap().storage.is_none());
    std::env::set_var("SUPABASE_URL", "https://ref.supabase.co/");
    assert!(
        Config::from_env().unwrap().storage.is_none(),
        "URL alone is not enough"
    );
    std::env::set_var("SUPABASE_SERVICE_KEY", "service-key");
    let c = Config::from_env().unwrap();
    let s = c.storage.as_ref().expect("storage enabled");
    // Trailing slash trimmed; bucket defaults to chat-files; 25 MiB default cap.
    assert_eq!(s.supabase_url, "https://ref.supabase.co");
    assert_eq!(s.service_key, "service-key");
    assert_eq!(s.bucket, "chat-files");
    assert_eq!(s.max_bytes, 25 * 1024 * 1024);
    assert_eq!(s.signed_ttl_secs, 7 * 24 * 60 * 60); // 7 days default
    std::env::set_var("SUPABASE_BUCKET", "custom-bucket");
    std::env::set_var("SUPABASE_MAX_UPLOAD_BYTES", "1048576");
    std::env::set_var("SUPABASE_SIGNED_URL_TTL_SECS", "3600");
    let s2 = Config::from_env()
        .unwrap()
        .storage
        .expect("storage enabled");
    assert_eq!(s2.bucket, "custom-bucket");
    assert_eq!(s2.max_bytes, 1_048_576);
    assert_eq!(s2.signed_ttl_secs, 3600);

    // A missing required key still fails.
    std::env::set_var("DEEPGRAM_API_KEY", "  ");
    assert!(Config::from_env().is_err());

    for k in [
        "DEEPGRAM_API_KEY",
        "GROQ_API_KEY",
        "DATABASE_URL",
        "GOOGLE_CLIENT_ID",
        "JWT_SECRET",
        "COST_PER_MINUTE",
        "MARKUP_PERCENTAGE",
        "RESEND_API_KEY",
        "RESEND_FROM_EMAIL",
        "RESEND_FROM_NAME",
        "CREDITS_REPORT_BASE",
        "GLOSSARY_MAX_ENTRIES",
        "AUTO_DETECT_BUFFER_MS",
        "SUPABASE_URL",
        "SUPABASE_SERVICE_KEY",
        "SUPABASE_BUCKET",
        "SUPABASE_MAX_UPLOAD_BYTES",
        "SUPABASE_SIGNED_URL_TTL_SECS",
    ] {
        std::env::remove_var(k);
    }
}
