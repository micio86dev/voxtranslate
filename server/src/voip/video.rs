//! Upgrading a telephone call to video (spec 0111, D9, R30).
//!
//! The recipient is on a telephone. There is no video to send them there — so the upgrade
//! is an invitation into the room the call is *already happening in*, opened in a browser,
//! with no account. The private-room guest path this relies on is a deliberate, shipped
//! property of the product (see `CLAUDE.md`), not something introduced here.
//!
//! ## Why the room code is not the link
//!
//! A private room's code is already an unguessable capability, so the obvious design is to
//! hand it over and be done. The reason not to is that a room code has no expiry: it works
//! for as long as the room exists, and an invitation gets forwarded, pasted into a chat,
//! and read by a phone that syncs it to three other devices.
//!
//! So the shared link carries a **signed ticket instead of the room**, and the room is
//! only handed back when that ticket is redeemed — in date, well formed, and for a call
//! that is still live. An expired or forged link yields nothing at all, and the room code
//! never appears in anything the caller passes on.
//!
//! ## What this must never do
//!
//! Disturb the telephone call. Nothing here touches the carrier, the media socket or the
//! billing row: an upgrade that fails leaves two people talking on the phone exactly as
//! they were, which is what D9 requires.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use chrono::{DateTime, Utc};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// How long an invitation stays usable.
///
/// Long enough to read a link out loud, spell it, or send it by whatever the two people
/// already use; short enough that a forwarded message is dead by the time it is read
/// somewhere else. The call itself is the real bound — redemption also refuses a call that
/// has ended — so this is the second of two limits, not the only one.
pub const INVITE_TTL_SECS: i64 = 15 * 60;

/// What a video invitation carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoInvite {
    /// The call being upgraded, so redemption can check it is still live.
    pub call_id: Uuid,
    /// The room to join. Signed, never shared in the clear.
    pub room: String,
    /// Unix seconds after which this is refused.
    pub exp: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InviteError {
    Malformed,
    BadSignature,
    Expired,
}

impl InviteError {
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "malformed_invite",
            Self::BadSignature => "bad_invite",
            Self::Expired => "invite_expired",
        }
    }
}

fn sign(key: &[u8], payload: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key");
    mac.update(payload);
    B64.encode(mac.finalize().into_bytes())
}

pub fn mint(key: &[u8], invite: &VideoInvite) -> String {
    let payload = serde_json::to_vec(invite).expect("VideoInvite serialises");
    let encoded = B64.encode(&payload);
    let sig = sign(key, encoded.as_bytes());
    format!("{encoded}.{sig}")
}

/// Verify a ticket and return what it carries.
///
/// Expiry is checked **after** the signature, deliberately: reporting "expired" for a
/// payload we never authenticated would answer a forger's question about what the payload
/// said.
pub fn verify(key: &[u8], raw: &str, now_unix: i64) -> Result<VideoInvite, InviteError> {
    let (encoded, sig) = raw.split_once('.').ok_or(InviteError::Malformed)?;

    // Constant-time: `==` on a String leaks position through timing, and a forgery oracle
    // on this signature is a forgery oracle on a live conversation.
    let expected = sign(key, encoded.as_bytes());
    if expected.as_bytes().ct_eq(sig.as_bytes()).unwrap_u8() != 1 {
        return Err(InviteError::BadSignature);
    }

    let bytes = B64.decode(encoded).map_err(|_| InviteError::Malformed)?;
    let invite: VideoInvite = serde_json::from_slice(&bytes).map_err(|_| InviteError::Malformed)?;
    if invite.exp <= now_unix {
        return Err(InviteError::Expired);
    }
    Ok(invite)
}

/// Mint an invitation for a call, and say when it dies.
pub fn issue(key: &[u8], call_id: Uuid, room: &str, now: DateTime<Utc>) -> (String, DateTime<Utc>) {
    let expires = now + chrono::Duration::seconds(INVITE_TTL_SECS);
    let ticket = mint(
        key,
        &VideoInvite {
            call_id,
            room: room.to_string(),
            exp: expires.timestamp(),
        },
    );
    (ticket, expires)
}

/// The link the caller shares.
///
/// Points at the API rather than at the app, because the ticket has to be redeemed by
/// something that holds the signing key before a room code exists to join.
pub fn invite_url(api_base: &str, ticket: &str) -> String {
    format!(
        "{}/api/voip/video/{}",
        api_base.trim_end_matches('/'),
        ticket
    )
}

/// The API's own origin, derived from the media WebSocket base.
///
/// `VOIP_MEDIA_WS_BASE` already names the host the carrier opens its media socket against,
/// which is this API — so an invitation link can be built from it exactly. A second
/// environment variable for the same origin is a second thing to keep in step, and the
/// failure when they drift is an invitation that 404s in someone else's browser.
pub fn api_base_from_ws(ws_base: &str) -> String {
    let b = ws_base.trim().trim_end_matches('/');
    match b.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        // Already an http(s) base, or something we do not recognise: pass it through
        // rather than mangling it.
        _ => b.to_string(),
    }
}

/// Where redemption sends the recipient.
pub fn join_url(app_base: &str, room: &str) -> String {
    format!("{}/?room={}", app_base.trim_end_matches('/'), room)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &[u8] = b"video-invite-key";

    fn now() -> DateTime<Utc> {
        DateTime::from_timestamp(1_800_000_000, 0).unwrap()
    }

    #[test]
    fn an_invite_round_trips() {
        let call = Uuid::new_v4();
        let (ticket, expires) = issue(KEY, call, "ph-abc", now());
        let back = verify(KEY, &ticket, now().timestamp()).expect("valid");
        assert_eq!(back.call_id, call);
        assert_eq!(back.room, "ph-abc");
        assert_eq!(expires, now() + chrono::Duration::seconds(INVITE_TTL_SECS));
    }

    #[test]
    fn the_room_never_appears_in_the_shared_link() {
        // The whole reason the link is not simply the room code: an invitation gets
        // forwarded and pasted, and a room code has no expiry.
        let (ticket, _) = issue(KEY, Uuid::new_v4(), "ph-secret-room", now());
        let url = invite_url("https://api.example", &ticket);
        assert!(
            !url.contains("ph-secret-room"),
            "the room leaked into the link: {url}"
        );
        assert!(url.starts_with("https://api.example/api/voip/video/"));
    }

    #[test]
    fn a_forged_ticket_is_refused() {
        let (ticket, _) = issue(KEY, Uuid::new_v4(), "ph-abc", now());
        assert_eq!(
            verify(b"someone-elses-key", &ticket, now().timestamp()),
            Err(InviteError::BadSignature)
        );
    }

    #[test]
    fn tampering_with_the_room_invalidates_the_signature() {
        // The attack this is actually for: re-point a valid invitation at a different
        // room and walk into a conversation that is not yours.
        let (ticket, _) = issue(KEY, Uuid::new_v4(), "ph-mine", now());
        let (encoded, sig) = ticket.split_once('.').unwrap();
        let mut payload: VideoInvite =
            serde_json::from_slice(&B64.decode(encoded).unwrap()).unwrap();
        payload.room = "ph-someone-elses".into();
        let forged = format!(
            "{}.{}",
            B64.encode(serde_json::to_vec(&payload).unwrap()),
            sig
        );
        assert_eq!(
            verify(KEY, &forged, now().timestamp()),
            Err(InviteError::BadSignature)
        );
    }

    #[test]
    fn an_expired_invite_is_refused() {
        let (ticket, _) = issue(KEY, Uuid::new_v4(), "ph-abc", now());
        let late = now().timestamp() + INVITE_TTL_SECS + 1;
        assert_eq!(verify(KEY, &ticket, late), Err(InviteError::Expired));
    }

    #[test]
    fn expiry_is_checked_after_the_signature() {
        // A forger must not be able to learn that their guessed payload parsed, or when it
        // claimed to expire, from the error they get back.
        let stale = VideoInvite {
            call_id: Uuid::new_v4(),
            room: "ph-abc".into(),
            exp: 1,
        };
        let unsigned = format!(
            "{}.notasignature",
            B64.encode(serde_json::to_vec(&stale).unwrap())
        );
        assert_eq!(
            verify(KEY, &unsigned, now().timestamp()),
            Err(InviteError::BadSignature),
            "an unauthenticated payload must never be reported as merely expired"
        );
    }

    #[test]
    fn garbage_is_refused_without_panicking() {
        for raw in ["", ".", "no-dot", "a.b", "!!!.???"] {
            assert!(
                verify(KEY, raw, now().timestamp()).is_err(),
                "accepted {raw:?}"
            );
        }
    }

    #[test]
    fn the_api_origin_comes_from_the_media_base() {
        // One origin, one source. Two env vars for the same host drift, and the failure is
        // an invitation that 404s in a stranger's browser with nothing to explain why.
        assert_eq!(
            api_base_from_ws("wss://api.voxtranslate.app"),
            "https://api.voxtranslate.app"
        );
        assert_eq!(
            api_base_from_ws("ws://localhost:3000/"),
            "http://localhost:3000"
        );
        assert_eq!(
            api_base_from_ws("https://api.example/"),
            "https://api.example",
            "an http base passes through rather than being mangled"
        );
    }

    #[test]
    fn the_join_url_is_the_ordinary_room_deep_link() {
        // Deliberately the same link the app has understood since the beginning; a video
        // upgrade is a room join, not a second way into the product.
        assert_eq!(
            join_url("https://app.example/", "ph-abc"),
            "https://app.example/?room=ph-abc"
        );
    }
}
