//! Live Connect (spec 0121): a host and up to three guests, each with a phone and a microphone,
//! every phone an independent translation stream running at the same time.
//!
//! This module owns what meetings do not have — server-minted sessions, credentials for a
//! person with no account, permanent desk QR codes, and billing a payer who is not on the
//! call. Everything conversational (room, engines, speech events, transcripts, search)
//! reuses the existing meeting machinery; see `docs/live-connect.md`.

pub mod admin;
pub mod billing;
pub mod config;
pub mod dashboard;
pub mod desks;
pub mod echo;
pub mod erase;
pub mod finalize;
pub mod routes;
pub mod sessions;
pub mod state;
pub mod summary;
pub mod sweeper;
pub mod tokens;
pub mod ws;

pub use state::LiveState;

/// True for a room name Live Connect mints itself (`live-<session uuid>`).
///
/// Those rooms are private to the seats of one conversation, so no other door into the room
/// machinery may open them: not the public `/ws` (a stranger who learned the session id would
/// receive the subtitles and translated audio) and not the business binding. Only the exact shape
/// is reserved — an existing user-chosen name such as `live-room` keeps working.
pub fn is_live_room(room: &str) -> bool {
    room.trim()
        .to_ascii_lowercase()
        .strip_prefix("live-")
        .is_some_and(|rest| uuid::Uuid::parse_str(rest).is_ok())
}
