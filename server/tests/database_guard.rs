//! The rule that a process on a laptop never talks to a deployed database.
//!
//! This is not hypothetical. `server/.env` on a developer machine points at a deployed
//! database, every binary here loads it through `dotenvy`, and `voip-rates` opens its
//! write with `DELETE FROM voip_rates`. `tests/integration.rs` loaded it too, so
//! `cargo test` with no `DATABASE_URL` exported ran the suite — which creates users,
//! deletes rows and drives global sweeps — against it. Nothing stood in the way except
//! the habit of exporting the variable.
//!
//! No database needed: this is string and environment logic.

use voxtranslate_server::db::{is_local_database, redacted_target};

#[test]
fn loopback_in_every_spelling_is_local() {
    for url in [
        "postgres://u:p@localhost:5432/vox",
        "postgres://u:p@127.0.0.1:5432/vox",
        "postgresql://u:p@127.0.0.1/vox",
        "postgres://u:p@[::1]:5432/vox",
        "postgres://localhost/vox",
        // A bare socket path has no host at all, so it cannot be anywhere else.
        "/var/run/postgresql",
        // A container-network service name. Single-label, so not a machine on the
        // internet — this is how the local Docker stack reaches its database.
        "postgres://vox:vox@postgres:5432/voxtranslate_dev",
        "postgres://vox:vox@db/voxtranslate_dev",
    ] {
        assert!(is_local_database(url), "{url}");
    }
}

#[test]
fn a_deployed_host_is_not_local_however_it_is_dressed() {
    for url in [
        // The two this project actually uses — production and the one `.env` points at.
        "postgres://u:p@aws-0-eu-west-1.pooler.supabase.com:5432/postgres",
        "postgres://u:p@aws-1-eu-central-1.pooler.supabase.com:5432/postgres",
        "postgres://u:p@db.internal.railway.app:5432/railway",
        // A password containing "localhost" must not buy its way past the check: the
        // host is what matters, and it is read after the last `@`.
        "postgres://user:localhost@real-host.example.com:5432/vox",
        // Nor a database NAMED localhost.
        "postgres://u:p@real-host.example.com:5432/localhost",
        // Every deployed host this project uses is fully qualified, which is what makes
        // the single-label rule above safe.
        "postgres://u:p@something.railway.app:5432/railway",
    ] {
        assert!(!is_local_database(url), "{url}");
    }
}

#[test]
fn the_target_is_printable_without_printing_the_password() {
    let shown = redacted_target("postgres://admin:sup3r-s3cret@db.example.com:5432/postgres");
    assert!(shown.contains("db.example.com"), "{shown}");
    assert!(shown.contains("postgres"), "{shown}");
    // The whole point: this string is safe to put in a log or a terminal.
    assert!(!shown.contains("sup3r-s3cret"), "{shown}");
    assert!(!shown.contains("admin"), "{shown}");
}

#[test]
fn the_test_url_helper_hands_back_a_local_database() {
    // The suite itself proves the refusal path — it runs with a local DATABASE_URL, and
    // every DB-gated test in this crate now goes through this helper. What is asserted
    // here is that it does not refuse the case it is supposed to allow.
    match voxtranslate_server::db::test_database_url() {
        Some(url) => assert!(is_local_database(&url), "the suite got a remote database"),
        None => eprintln!("skipping — no DATABASE_URL"),
    }
}
