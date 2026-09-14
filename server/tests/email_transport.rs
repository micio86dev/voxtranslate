//! The transactional-email gate, and the one failure it used to hide.
//!
//! There is **no SMTP path in this server**. `src/email.rs` posts to
//! `https://api.resend.com/emails` and that is the entire transport; no `lettre`, no
//! `SMTP_*` variable is read anywhere. Configuring an SMTP host for this deployment
//! therefore configures nothing, which is worth a test saying so out loud, because the
//! belief that production ran on SMTP is what sent a mail outage looking in the wrong
//! place.
//!
//! The gate is all-or-nothing: miss one of the three `RESEND_*` values and `state.resend`
//! is `None`, every send site takes its silent `else`, and no mail is sent and nothing is
//! logged. The nastiest shape is a variable that is **present but empty** — it is set in
//! the deployment dashboard, it reads as configured, and `present()` still rejects it.
//!
//! Mutates process-global env, so it lives in its own integration binary rather than in
//! the lib unit-test binary, for the same reason `config_env.rs` does.

use voxtranslate_server::config::Config;

/// The minimum any `Config::from_env()` needs before the mailer question is even reached.
fn base_env() {
    std::env::set_var("QWEN_API_KEY", "sk-test");
    std::env::set_var("GROQ_API_KEY", "gk");
    for k in ["DATABASE_URL", "GOOGLE_CLIENT_ID", "JWT_SECRET", "PORT"] {
        std::env::remove_var(k);
    }
}

fn set_all_resend_vars() {
    std::env::set_var("RESEND_API_KEY", "re_test");
    std::env::set_var("RESEND_FROM_EMAIL", "noreply@example.test");
    std::env::set_var("RESEND_FROM_NAME", "VoxTranslate");
}

/// One test, not two: both halves mutate process-global env, and as separate `#[test]`
/// functions in one binary they run on parallel threads and clobber each other's setup.
#[test]
fn the_resend_trio_is_the_only_thing_that_turns_email_on() {
    base_env();

    set_all_resend_vars();
    assert!(
        Config::from_env().unwrap().resend.is_some(),
        "three good values must produce a mailer"
    );

    // Present but blank. This is the shape that looks configured in a deployment
    // dashboard — the variable is listed, it has a name, somebody set it — and still
    // leaves production unable to send a single email.
    for blank in ["", "   "] {
        set_all_resend_vars();
        std::env::set_var("RESEND_FROM_NAME", blank);
        assert!(
            Config::from_env().unwrap().resend.is_none(),
            "an empty RESEND_FROM_NAME ({blank:?}) must not pass for configured"
        );
    }

    // And each one alone is enough to close the gate, so no single variable can be
    // dismissed as the unimportant one.
    for missing in ["RESEND_API_KEY", "RESEND_FROM_EMAIL", "RESEND_FROM_NAME"] {
        set_all_resend_vars();
        std::env::remove_var(missing);
        assert!(
            Config::from_env().unwrap().resend.is_none(),
            "{missing} missing must disable the mailer"
        );
    }

    // Setting every SMTP name an operator would reach for changes nothing, because
    // nothing reads them. The mailer still depends only on the RESEND_* trio.
    for k in [
        "SMTP_HOST",
        "SMTP_PORT",
        "SMTP_USER",
        "SMTP_PASSWORD",
        "SMTP_FROM",
        "MAIL_TRANSPORT",
    ] {
        std::env::set_var(k, "something");
    }
    for k in ["RESEND_API_KEY", "RESEND_FROM_EMAIL", "RESEND_FROM_NAME"] {
        std::env::remove_var(k);
    }
    assert!(
        Config::from_env().unwrap().resend.is_none(),
        "a full SMTP configuration must not produce a working mailer — there is no SMTP \
         transport in this server, and pretending otherwise is what hid the outage"
    );

    set_all_resend_vars();
    assert!(
        Config::from_env().unwrap().resend.is_some(),
        "the RESEND_* trio is the only thing that turns email on"
    );
}
