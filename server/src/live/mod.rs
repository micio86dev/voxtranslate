//! Live Connect (spec 0121): two phones, two microphones, two independent translation
//! streams running at the same time.
//!
//! This module owns what meetings do not have — server-minted sessions, credentials for a
//! person with no account, permanent desk QR codes, and billing a payer who is not on the
//! call. Everything conversational (room, engines, speech events, transcripts, search)
//! reuses the existing meeting machinery; see `docs/live-connect.md`.

pub mod config;
pub mod routes;
pub mod sessions;
pub mod state;
pub mod sweeper;
pub mod tokens;
pub mod ws;

pub use state::LiveState;
