//! Credentials for Live Connect: invitations, guest tokens and permanent desk QR tokens.
//!
//! Three different things, kept apart on purpose:
//!
//! * **Invite code** — what a B2C link or QR carries. 192 random bits; only its SHA-256 is
//!   stored, so a database leak does not yield working invitations.
//! * **Guest token** — what a person with no account holds after joining. A signed claim
//!   naming one session, one participant and one role, valid for the life of the session so a
//!   dropped phone can reconnect. Signed with a key derived for this purpose only and carrying
//!   its own audience, so the account JWT verifier can never accept it (and the other way
//!   round), even if an operator reused the JWT secret as the key.
//! * **Desk token** — the MAC printed in a permanent QR. It is *derived*, not stored: the
//!   database keeps only `token_version`, so revoking or regenerating needs no secret to be
//!   remembered, and an administrator can re-download the printed asset at any time.
//!
//! Every comparison of secret material is constant-time.

use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use base64::Engine;
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;
use uuid::Uuid;

/// Audience stamped into every guest token. A signed payload without it is refused.
pub const GUEST_AUD: &str = "live-guest";

/// Entropy of an invite code, in bytes.
const INVITE_BYTES: usize = 24;
/// Characters in an encoded invite code (24 bytes, unpadded base64url).
pub const INVITE_LEN: usize = 32;
/// A desk MAC is truncated to 128 bits: more than enough for an online-verified secret,
/// and it keeps the printed QR small.
const DESK_MAC_BYTES: usize = 16;
/// Characters in an encoded desk MAC.
const DESK_MAC_LEN: usize = 22;
/// Length of a desk's public id.
pub const DESK_PUBLIC_ID_LEN: usize = 12;
/// Smallest accepted master secret.
const MIN_SECRET_LEN: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyError {
    TooShort,
}

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort => write!(f, "must be at least {MIN_SECRET_LEN} characters"),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenError {
    Malformed,
    BadSignature,
    WrongAudience,
    Expired,
}

impl TokenError {
    /// Machine-readable code for API responses.
    pub fn code(self) -> &'static str {
        match self {
            Self::Malformed => "malformed_token",
            Self::BadSignature => "bad_token",
            Self::WrongAudience => "bad_token",
            Self::Expired => "token_expired",
        }
    }
}

/// Which seat a participant holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PeerRole {
    Host,
    Staff,
    Guest,
}

/// What a guest token proves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GuestClaims {
    pub aud: String,
    /// The live session.
    pub sid: Uuid,
    /// The participant row, which is also the room peer id.
    pub pid: Uuid,
    pub role: PeerRole,
    /// Unix seconds after which the token is refused.
    pub exp: i64,
}

impl GuestClaims {
    pub fn new(sid: Uuid, pid: Uuid, role: PeerRole, exp: i64) -> Self {
        Self {
            aud: GUEST_AUD.to_string(),
            sid,
            pid,
            role,
            exp,
        }
    }
}

/// Purpose-bound signing keys, derived from one master secret.
#[derive(Clone)]
pub struct LiveKeys {
    guest: [u8; 32],
    desk: [u8; 32],
}

// Never print key material, even by accident in a `{:?}` of a config struct.
impl std::fmt::Debug for LiveKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LiveKeys").finish_non_exhaustive()
    }
}

fn hmac_bytes(key: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    let mut mac =
        <Hmac<Sha256> as KeyInit>::new_from_slice(key).expect("HMAC accepts any key length");
    for p in parts {
        mac.update(p);
    }
    mac.finalize().into_bytes().into()
}

impl LiveKeys {
    /// Derive the per-purpose keys. The labels guarantee that a signature made for one
    /// purpose is worthless for another even though they share a master secret.
    pub fn from_secret(secret: &str) -> Result<Self, KeyError> {
        if secret.trim().len() < MIN_SECRET_LEN {
            return Err(KeyError::TooShort);
        }
        let master = secret.trim().as_bytes();
        Ok(Self {
            guest: hmac_bytes(master, &[b"live-connect/guest-token/v1"]),
            desk: hmac_bytes(master, &[b"live-connect/desk-token/v1"]),
        })
    }

    // ---- guest tokens ---------------------------------------------------------------

    pub fn mint_guest_token(&self, claims: &GuestClaims) -> String {
        let payload = serde_json::to_vec(claims).expect("GuestClaims serialises");
        let encoded = B64.encode(payload);
        let sig = B64.encode(hmac_bytes(&self.guest, &[encoded.as_bytes()]));
        format!("{encoded}.{sig}")
    }

    /// Verify a guest token. The signature is checked **before** anything in the payload is
    /// read, so an unauthenticated payload can never tell a forger what it said.
    pub fn verify_guest_token(&self, raw: &str, now_unix: i64) -> Result<GuestClaims, TokenError> {
        let (encoded, sig) = raw.split_once('.').ok_or(TokenError::Malformed)?;
        let expected = B64.encode(hmac_bytes(&self.guest, &[encoded.as_bytes()]));
        if expected.as_bytes().ct_eq(sig.as_bytes()).unwrap_u8() != 1 {
            return Err(TokenError::BadSignature);
        }
        let bytes = B64.decode(encoded).map_err(|_| TokenError::Malformed)?;
        let claims: GuestClaims =
            serde_json::from_slice(&bytes).map_err(|_| TokenError::Malformed)?;
        if claims.aud != GUEST_AUD {
            return Err(TokenError::WrongAudience);
        }
        if claims.exp <= now_unix {
            return Err(TokenError::Expired);
        }
        Ok(claims)
    }

    // ---- desk tokens ----------------------------------------------------------------

    /// The MAC printed in a desk's QR for this `token_version`.
    pub fn desk_mac(&self, desk_id: Uuid, token_version: i32) -> String {
        let full = hmac_bytes(
            &self.desk,
            &[desk_id.as_bytes(), &token_version.to_be_bytes()],
        );
        B64.encode(&full[..DESK_MAC_BYTES])
    }

    /// Constant-time check of a presented MAC against the desk's current version.
    pub fn verify_desk_mac(&self, desk_id: Uuid, token_version: i32, presented: &str) -> bool {
        let expected = self.desk_mac(desk_id, token_version);
        expected.as_bytes().ct_eq(presented.as_bytes()).unwrap_u8() == 1
    }
}

// ---- invite codes --------------------------------------------------------------------

/// A fresh, unguessable invite code (192 random bits).
pub fn new_invite_code() -> String {
    use rand::RngExt;
    let mut bytes = [0u8; INVITE_BYTES];
    rand::rng().fill(&mut bytes[..]);
    B64.encode(bytes)
}

/// Shape check, run **before** the database is touched so junk never costs a query.
pub fn is_well_formed_invite(code: &str) -> bool {
    code.len() == INVITE_LEN && code.bytes().all(is_b64url)
}

/// What the database stores in place of the code.
pub fn hash_secret(secret: &str) -> [u8; 32] {
    Sha256::digest(secret.as_bytes()).into()
}

// ---- desk links ----------------------------------------------------------------------

/// A short lookup handle for a desk. It is *not* a secret — the MAC is — so it only has to
/// be unique and unambiguous when read aloud or typed.
pub fn new_desk_public_id() -> String {
    use rand::RngExt;
    const CHARS: &[u8] = b"abcdefghijkmnpqrstuvwxyz23456789";
    let mut rng = rand::rng();
    (0..DESK_PUBLIC_ID_LEN)
        .map(|_| CHARS[rng.random_range(0..CHARS.len())] as char)
        .collect()
}

/// The path segment a desk QR encodes: `<public_id>.<mac>`. Nothing else — no organisation,
/// project or account identifier — is derivable from it.
pub fn desk_link_segment(public_id: &str, mac: &str) -> String {
    format!("{public_id}.{mac}")
}

/// The full URL printed in a desk QR.
pub fn desk_url(app_base: &str, public_id: &str, mac: &str) -> String {
    format!(
        "{}/lc/d/{}",
        app_base.trim_end_matches('/'),
        desk_link_segment(public_id, mac)
    )
}

/// Split and shape-check a scanned segment. Returns `(public_id, mac)`.
pub fn split_desk_segment(raw: &str) -> Option<(&str, &str)> {
    let (public_id, mac) = raw.split_once('.')?;
    let ok = public_id.len() == DESK_PUBLIC_ID_LEN
        && public_id
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && mac.len() == DESK_MAC_LEN
        && mac.bytes().all(is_b64url);
    ok.then_some((public_id, mac))
}

fn is_b64url(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'-' || b == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;

    const SECRET: &str = "0123456789abcdef0123456789abcdef-live-test";

    fn keys() -> LiveKeys {
        LiveKeys::from_secret(SECRET).unwrap()
    }

    fn claims(exp: i64) -> GuestClaims {
        GuestClaims::new(Uuid::new_v4(), Uuid::new_v4(), PeerRole::Guest, exp)
    }

    #[test]
    fn short_secrets_are_refused() {
        assert_eq!(
            LiveKeys::from_secret("too-short").unwrap_err(),
            KeyError::TooShort
        );
        assert!(LiveKeys::from_secret(&"x".repeat(MIN_SECRET_LEN)).is_ok());
    }

    #[test]
    fn debug_never_prints_key_material() {
        let shown = format!("{:?}", keys());
        assert!(!shown.contains("0123") && shown.contains("LiveKeys"));
    }

    // ---- invite codes

    #[test]
    fn invite_codes_are_unique_well_formed_and_hash_stably() {
        let a = new_invite_code();
        let b = new_invite_code();
        assert_ne!(a, b);
        assert!(is_well_formed_invite(&a));
        assert_eq!(a.len(), INVITE_LEN);
        assert_eq!(hash_secret(&a), hash_secret(&a));
        assert_ne!(hash_secret(&a), hash_secret(&b));
    }

    #[test]
    fn malformed_invites_are_rejected_before_any_lookup() {
        for bad in [
            "",
            "short",
            &"a".repeat(INVITE_LEN + 1),
            &format!("{}!", "a".repeat(INVITE_LEN - 1)),
        ] {
            assert!(!is_well_formed_invite(bad), "{bad:?}");
        }
    }

    // ---- guest tokens

    #[test]
    fn a_guest_token_round_trips() {
        let k = keys();
        let c = claims(2_000_000_000);
        let token = k.mint_guest_token(&c);
        assert_eq!(k.verify_guest_token(&token, 1_900_000_000), Ok(c));
    }

    #[test]
    fn an_expired_guest_token_is_refused() {
        let k = keys();
        let token = k.mint_guest_token(&claims(1_000));
        assert_eq!(
            k.verify_guest_token(&token, 1_000),
            Err(TokenError::Expired)
        );
        assert_eq!(
            k.verify_guest_token(&token, 5_000),
            Err(TokenError::Expired)
        );
    }

    #[test]
    fn a_tampered_payload_or_signature_is_refused() {
        let k = keys();
        let token = k.mint_guest_token(&claims(2_000_000_000));
        let (payload, sig) = token.split_once('.').unwrap();

        // A different, validly encoded payload under the original signature.
        let forged = B64.encode(serde_json::to_vec(&claims(2_000_000_000)).unwrap());
        assert_eq!(
            k.verify_guest_token(&format!("{forged}.{sig}"), 1),
            Err(TokenError::BadSignature)
        );
        // A flipped signature.
        let mut flipped = sig.to_string();
        flipped.replace_range(0..1, if sig.starts_with('A') { "B" } else { "A" });
        assert_eq!(
            k.verify_guest_token(&format!("{payload}.{flipped}"), 1),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn garbage_is_malformed_or_unsigned_never_accepted() {
        let k = keys();
        for raw in ["", "nodot", ".", "a.b", "a.b.c"] {
            assert!(k.verify_guest_token(raw, 1).is_err(), "{raw:?}");
        }
    }

    #[test]
    fn a_token_signed_with_another_key_is_refused() {
        let other =
            LiveKeys::from_secret("a-completely-different-secret-of-sufficient-length").unwrap();
        let token = other.mint_guest_token(&claims(2_000_000_000));
        assert_eq!(
            keys().verify_guest_token(&token, 1),
            Err(TokenError::BadSignature)
        );
    }

    #[test]
    fn a_correctly_signed_payload_with_the_wrong_audience_is_refused() {
        let k = keys();
        let mut c = claims(2_000_000_000);
        c.aud = "somebody-else".into();
        let token = k.mint_guest_token(&c);
        assert_eq!(
            k.verify_guest_token(&token, 1),
            Err(TokenError::WrongAudience)
        );
    }

    /// The account verifier must never admit a guest token, and the guest verifier must never
    /// admit an account JWT — including in the worst misconfiguration, where an operator pastes
    /// the JWT secret in as the Live Connect key.
    #[test]
    fn guest_tokens_and_account_jwts_cannot_stand_in_for_each_other() {
        let jwt_secret = SECRET; // same string on purpose
        let k = LiveKeys::from_secret(jwt_secret).unwrap();

        let guest = k.mint_guest_token(&claims(2_000_000_000));
        assert!(
            crate::auth::verify_jwt(jwt_secret, &guest).is_err(),
            "a guest token must not authenticate as an account"
        );

        let account = crate::auth::issue_jwt(jwt_secret, &Uuid::new_v4(), "a@b.c", "A", 1).unwrap();
        assert!(
            k.verify_guest_token(&account, 1).is_err(),
            "an account JWT must not be accepted as a guest token"
        );
    }

    // ---- desk tokens

    #[test]
    fn a_desk_mac_verifies_only_for_its_desk_and_version() {
        let k = keys();
        let desk = Uuid::new_v4();
        let mac = k.desk_mac(desk, 1);
        assert_eq!(mac.len(), DESK_MAC_LEN);
        assert!(k.verify_desk_mac(desk, 1, &mac));
        assert!(
            !k.verify_desk_mac(desk, 2, &mac),
            "regenerating kills the old QR"
        );
        assert!(
            !k.verify_desk_mac(Uuid::new_v4(), 1, &mac),
            "not transferable between desks"
        );
        assert!(!k.verify_desk_mac(desk, 1, ""));
        assert!(!k.verify_desk_mac(desk, 1, &mac[..mac.len() - 1]));
    }

    #[test]
    fn desk_macs_are_stable_and_depend_on_the_key() {
        let desk = Uuid::new_v4();
        assert_eq!(keys().desk_mac(desk, 3), keys().desk_mac(desk, 3));
        let other =
            LiveKeys::from_secret("a-completely-different-secret-of-sufficient-length").unwrap();
        assert_ne!(keys().desk_mac(desk, 3), other.desk_mac(desk, 3));
    }

    #[test]
    fn the_same_master_secret_gives_unrelated_guest_and_desk_keys() {
        let k = keys();
        assert_ne!(k.guest, k.desk, "purpose labels must separate the keys");
    }

    #[test]
    fn a_desk_link_carries_only_a_handle_and_a_mac() {
        let k = keys();
        let desk = Uuid::new_v4();
        let public_id = new_desk_public_id();
        let mac = k.desk_mac(desk, 1);
        let url = desk_url("https://app.example/", &public_id, &mac);

        assert_eq!(url, format!("https://app.example/lc/d/{public_id}.{mac}"));
        assert!(
            !url.contains(&desk.to_string()),
            "the desk uuid must not leak into a QR"
        );

        let segment = url.rsplit('/').next().unwrap();
        assert_eq!(
            split_desk_segment(segment),
            Some((public_id.as_str(), mac.as_str()))
        );
    }

    #[test]
    fn scanned_segments_are_shape_checked() {
        for bad in [
            "",
            "nodot",
            "short.abc",
            "abcdefghijkm.",
            "ABCDEFGHIJKM.0123456789012345678901",
        ] {
            assert_eq!(split_desk_segment(bad), None, "{bad:?}");
        }
    }
}
