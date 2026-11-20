//! Live Connect sessions: the database side of create / join / end.
//!
//! No HTTP in here. Every function takes a pool and returns a plain result, so the rules
//! (who may take a seat, when a session is full, how many a user may hold open) are tested
//! against a real database without a server.
//!
//! A session has one **owner side** seat and up to `max_guests` guest seats. The owner side is
//! the `host` (B2C owner, or a member who created a B2B session) or `staff` (a member who
//! answered a desk scan); a partial unique index on `live_participants` makes "one owner side" a
//! database fact. Guests are capped IN CODE: a unique index cannot say "at most N", so
//! [`join_guest`] counts them under a `FOR UPDATE` lock on the session row, which is what makes
//! simultaneous joins admit exactly `max_guests` and no more.

use chrono::{DateTime, Utc};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::config::LiveConfig;
use super::tokens::{self, GuestClaims, PeerRole};
use crate::db::Pool;

/// Most diagnostic events kept per session; a flapping connection must not grow a table.
const MAX_EVENTS_PER_SESSION: i64 = 200;

// ---- input hygiene ---------------------------------------------------------------------

/// A language code the room machinery accepts: 1–8 alphanumerics or `-`, lowercased. `auto`
/// is refused — each phone declares its language, nothing guesses it.
pub fn normalize_lang(raw: &str) -> Option<String> {
    let l = raw.trim().to_ascii_lowercase();
    let ok = (1..=8).contains(&l.len())
        && l.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && l != "auto";
    ok.then_some(l)
}

/// Characters that reorder or hide text: directional overrides/isolates and marks, zero-width
/// characters, line and paragraph separators, the invisible operators and the BOM. A name
/// carrying them can render as something else (or as nothing at all) on the other person's
/// screen; one made only of them is, once stripped, an empty name like any other.
fn is_text_steering(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'                // zero-width space
        | '\u{200E}'..='\u{200F}' // LRM, RLM (ZWNJ/ZWJ U+200C/D are real writing: kept)
        | '\u{2028}'..='\u{202E}' // line/paragraph separator, embeddings, overrides
        | '\u{2060}'..='\u{206F}' // word joiner, invisible operators, isolates, deprecated formats
        | '\u{061C}'               // Arabic letter mark
        | '\u{FEFF}'               // BOM / zero-width no-break space
    )
}

/// Characters that draw nothing: whitespace, the joiners kept inside real writing, and blank
/// fillers (Hangul/Braille fillers, soft hyphen, combining grapheme joiner, tag characters). A
/// name made only of them looks empty to the other person, so it is treated as empty.
fn is_blank(c: char) -> bool {
    c.is_whitespace()
        || matches!(
            c,
            '\u{200C}'
                | '\u{200D}'
                | '\u{3164}'
                | '\u{2800}'
                | '\u{00AD}'
                | '\u{034F}'
                | '\u{115F}'
                | '\u{1160}'
                | '\u{FFA0}'
                | '\u{E0000}'..='\u{E007F}'
        )
}

/// A display name safe to broadcast to the other phone: control and text-steering characters
/// removed, at most 100 characters, never empty (or empty-looking).
pub fn clean_name(raw: &str) -> String {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() && !is_text_steering(*c))
        .collect::<String>()
        .trim()
        .chars()
        .take(100)
        .collect();
    if cleaned.chars().all(is_blank) {
        "Guest".to_string()
    } else {
        cleaned
    }
}

// ---- types -----------------------------------------------------------------------------

/// Who pays for, and owns, a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    /// A signed-in consumer; their personal balance is charged.
    User(Uuid),
    /// A business; the org wallet is charged and the project is mandatory.
    Org { org_id: Uuid, project_id: Uuid },
}

#[derive(Debug)]
pub struct NewSession<'a> {
    pub owner: Owner,
    /// The signed-in person who takes the owner-side seat.
    pub host_user: Uuid,
    pub host_name: &'a str,
    pub host_lang: &'a str,
    pub save_transcript: bool,
    pub engine_id: Option<&'a str>,
    /// How many guests the invitation admits (1..=3). Validated by the caller.
    pub max_guests: i16,
    /// The host opted, for this conversation only, into the other phones speaking the host's
    /// words in the host's cloned voice (Enhanced only). Validated by the caller.
    pub use_voice_clone: bool,
}

#[derive(Debug)]
pub struct Created {
    pub session_id: Uuid,
    /// The raw invitation code. Shown once; only its hash is stored.
    pub invite_code: String,
    pub host_peer: Uuid,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
}

#[derive(Debug)]
pub enum CreateError {
    /// The owner already holds the maximum number of open sessions.
    TooManySessions,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for CreateError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

#[derive(Debug)]
pub struct Joined {
    pub session_id: Uuid,
    pub guest_peer: Uuid,
    pub host_name: String,
    pub host_lang: String,
    pub created_at: DateTime<Utc>,
    /// How many guests the session admits in all (this guest included).
    pub max_guests: i16,
    /// The tier the session runs on (a guest inherits it). `None` is a row that predates tiers:
    /// Standard.
    pub engine_id: Option<String>,
    /// Whether the host opted into their cloned voice for this conversation (Enhanced only).
    pub use_voice_clone: bool,
}

#[derive(Debug)]
pub enum JoinError {
    /// No session carries this code.
    NotFound,
    /// The window to join closed before anyone did.
    Expired,
    /// The session has ended.
    Ended,
    /// Every guest seat is taken. A guest who already joined reconnects with their token.
    Full,
    /// The translation engine cannot open the upstream sessions this guest's arrival needs.
    Busy,
    Db(sqlx::Error),
}

impl From<sqlx::Error> for JoinError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e)
    }
}

/// What a verified token resolved to, with the session still open.
#[derive(Debug, Clone)]
pub struct PeerAuth {
    pub session_id: Uuid,
    pub peer_id: Uuid,
    pub role: PeerRole,
    pub lang: String,
    pub display_name: String,
    pub user_id: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub owner_user_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub status: String,
    pub save_transcript: bool,
    pub engine_id: Option<String>,
    /// The host's opt-in to their cloned voice for this conversation.
    pub use_voice_clone: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuthError {
    /// Not a participant of a session that is still open.
    Gone,
}

// ---- create ----------------------------------------------------------------------------

/// Create a session and seat its owner. The invitation code is generated here and returned
/// once; only its SHA-256 is stored.
pub async fn create_session(
    pool: &Pool,
    cfg: &LiveConfig,
    new: NewSession<'_>,
) -> Result<Created, CreateError> {
    let mut tx = pool.begin().await?;

    let (owner_user, org, project) = match new.owner {
        Owner::User(u) => (Some(u), None, None),
        Owner::Org { org_id, project_id } => (None, Some(org_id), Some(project_id)),
    };

    // The cap is per PERSON who opens conversations — the owner of a personal one, or the member
    // who starts a business one — so an org's staff cannot be used to open an unbounded number.
    {
        // Serialise concurrent creates by the same person, so the cap cannot be beaten by
        // opening several sessions in the same instant. Released at commit.
        sqlx::query("SELECT pg_advisory_xact_lock(hashtextextended($1, 0))")
            .bind(format!("live-host:{}", new.host_user))
            .execute(&mut *tx)
            .await?;
        let open: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM live_participants p
             JOIN live_sessions s ON s.id = p.session_id
             WHERE p.role = 'host' AND p.user_id = $1
               AND s.status IN ('waiting', 'active')
               AND (s.status = 'active' OR s.expires_at > now())
               AND s.created_at > now() - make_interval(mins => $2::int)",
        )
        .bind(new.host_user)
        .bind(cfg.max_session_min as i32)
        .fetch_one(&mut *tx)
        .await?;
        if open >= cfg.max_concurrent_per_owner {
            return Err(CreateError::TooManySessions);
        }
    }

    let code = tokens::new_invite_code();
    let hash = tokens::hash_secret(&code);

    let (session_id, created_at, expires_at): (Uuid, DateTime<Utc>, DateTime<Utc>) =
        sqlx::query_as(
            "INSERT INTO live_sessions
                 (owner_user_id, org_id, project_id, status, invite_hash, host_lang,
                  save_transcript, engine_id, max_guests, use_voice_clone, expires_at)
             VALUES ($1, $2, $3, 'waiting', $4, $5, $6, $7, $9, $10,
                     now() + make_interval(secs => $8::int))
             RETURNING id, created_at, expires_at",
        )
        .bind(owner_user)
        .bind(org)
        .bind(project)
        .bind(hash.as_slice())
        .bind(new.host_lang)
        .bind(new.save_transcript)
        .bind(new.engine_id)
        .bind(cfg.join_window_secs as i32)
        .bind(new.max_guests)
        .bind(new.use_voice_clone)
        .fetch_one(&mut *tx)
        .await?;

    let host_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, user_id, display_name, lang, consent_at)
         VALUES ($1, 'host', $2, $3, $4, now())
         RETURNING peer_id",
    )
    .bind(session_id)
    .bind(new.host_user)
    .bind(clean_name(new.host_name))
    .bind(new.host_lang)
    .fetch_one(&mut *tx)
    .await?;

    tx.commit().await?;
    Ok(Created {
        session_id,
        invite_code: code,
        host_peer,
        created_at,
        expires_at,
    })
}

// ---- join ------------------------------------------------------------------------------

/// Upstream translation sessions a room needs when everybody speaks: each participant opens one
/// per distinct OTHER language present, so `p` participants over `d` distinct languages need
/// `sum over participants of (d - 1)`. An upper bound (not everybody speaks all the time) and the
/// figure the capacity gate reasons with.
pub fn engine_sessions_needed(langs: &[&str]) -> usize {
    let mut distinct: Vec<&str> = Vec::with_capacity(langs.len());
    for l in langs {
        if !distinct.contains(l) {
            distinct.push(l);
        }
    }
    langs.len() * distinct.len().saturating_sub(1)
}

/// Engine sessions a guest of language `newcomer` adds to a room of `present`: their own streams
/// plus the new language's stream for every speaker already there. Never below the two a
/// conversation has always needed, so a first guest is judged exactly as before.
pub fn engine_sessions_for_join(present: &[&str], newcomer: &str) -> usize {
    let mut after: Vec<&str> = present.to_vec();
    after.push(newcomer);
    engine_sessions_needed(&after)
        .saturating_sub(engine_sessions_needed(present))
        .max(2)
}

/// Take a guest seat with an invitation code. The same code admits guests one after the other
/// while the session is `waiting` or `active`, until it holds `max_guests` of them.
///
/// The session row is locked (`FOR UPDATE`) for the whole check-and-insert, so N simultaneous
/// joins queue behind each other and exactly `max_guests` get in; the rest get
/// [`JoinError::Full`]. Each guest has a seat of their own (id, token, consent), and a seat is
/// never handed on: a guest who dropped reconnects with the token they were given.
///
/// The join window only matters to a session nobody has joined yet (`waiting`): once it has a
/// guest it is `active` and the invitation stays good until the session ends or fills.
///
/// `free_engine_sessions` is the engine's spare upstream capacity (`None` = unmetered). When the
/// new guest's arrival needs more than that, the join is refused with [`JoinError::Busy`] instead
/// of seating somebody whose direction the engine would silently skip.
pub async fn join_guest(
    pool: &Pool,
    code: &str,
    name: &str,
    lang: &str,
    free_engine_sessions: Option<usize>,
) -> Result<Joined, JoinError> {
    let hash = tokens::hash_secret(code);
    let mut tx = pool.begin().await?;

    #[derive(sqlx::FromRow)]
    struct Invite {
        id: Uuid,
        status: String,
        past_expiry: bool,
        created_at: DateTime<Utc>,
        host_lang: String,
        max_guests: i16,
        engine_id: Option<String>,
        use_voice_clone: bool,
    }
    let row: Option<Invite> = sqlx::query_as(
        "SELECT id, status, (expires_at <= now()) AS past_expiry, created_at, host_lang,
                max_guests, engine_id, use_voice_clone
         FROM live_sessions WHERE invite_hash = $1 FOR UPDATE",
    )
    .bind(hash.as_slice())
    .fetch_optional(&mut *tx)
    .await?;
    let Some(Invite {
        id: session_id,
        status,
        past_expiry,
        created_at,
        host_lang,
        max_guests,
        engine_id,
        use_voice_clone,
    }) = row
    else {
        return Err(JoinError::NotFound);
    };

    match status.as_str() {
        "ended" => return Err(JoinError::Ended),
        "expired" => return Err(JoinError::Expired),
        "waiting" if past_expiry => {
            sqlx::query(
                "UPDATE live_sessions SET status = 'expired', ended_at = now(), end_reason = 'expired'
                 WHERE id = $1",
            )
            .bind(session_id)
            .execute(&mut *tx)
            .await?;
            tx.commit().await?;
            return Err(JoinError::Expired);
        }
        _ => {}
    }

    let present: Vec<(String, String)> =
        sqlx::query_as("SELECT role, lang FROM live_participants WHERE session_id = $1")
            .bind(session_id)
            .fetch_all(&mut *tx)
            .await?;
    let guests = present.iter().filter(|(role, _)| role == "guest").count();
    if guests >= usize::try_from(max_guests).unwrap_or(1) {
        return Err(JoinError::Full);
    }
    if let Some(free) = free_engine_sessions {
        let langs: Vec<&str> = present.iter().map(|(_, l)| l.as_str()).collect();
        if free < engine_sessions_for_join(&langs, lang) {
            return Err(JoinError::Busy);
        }
    }

    // `clock_timestamp()`, not `now()`: `now()` is the START of this transaction, and a join that
    // began before a colleague's committed would sort ahead of it. Seats are read back in join
    // order, and the lock above guarantees this call runs after the previous join's commit.
    let guest_peer: Uuid = sqlx::query_scalar(
        "INSERT INTO live_participants (session_id, role, display_name, lang, consent_at, joined_at)
         VALUES ($1, 'guest', $2, $3, now(), clock_timestamp())
         RETURNING peer_id",
    )
    .bind(session_id)
    .bind(clean_name(name))
    .bind(lang)
    .fetch_one(&mut *tx)
    .await?;

    sqlx::query(
        "UPDATE live_sessions
         SET status = 'active', started_at = COALESCE(started_at, now())
         WHERE id = $1",
    )
    .bind(session_id)
    .execute(&mut *tx)
    .await?;

    let host_name: String = sqlx::query_scalar(
        "SELECT display_name FROM live_participants
         WHERE session_id = $1 AND role IN ('host', 'staff')",
    )
    .bind(session_id)
    .fetch_optional(&mut *tx)
    .await?
    .unwrap_or_else(|| "Host".to_string());

    tx.commit().await?;
    Ok(Joined {
        session_id,
        guest_peer,
        host_name,
        host_lang,
        created_at,
        max_guests,
        engine_id,
        use_voice_clone,
    })
}

/// The tier of the session an invitation code opens, read before the join so the capacity gate
/// can judge the engine that will actually serve it. Outer `None`: no such session (the join
/// answers that itself); inner `None`: a session that predates tiers.
pub async fn engine_for_invite(pool: &Pool, code: &str) -> Option<Option<String>> {
    let hash = tokens::hash_secret(code);
    sqlx::query_scalar::<_, Option<String>>(
        "SELECT engine_id FROM live_sessions WHERE invite_hash = $1",
    )
    .bind(hash.as_slice())
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
}

// ---- tokens ----------------------------------------------------------------------------

/// Mint the token a participant presents from now on. It outlives the join window — it
/// must, so a dropped phone can reconnect — but never the session's hard ceiling.
pub fn issue_token(
    cfg: &LiveConfig,
    session_id: Uuid,
    peer_id: Uuid,
    role: PeerRole,
    session_created: DateTime<Utc>,
) -> Option<String> {
    let keys = cfg.keys.as_ref()?;
    let exp = session_created.timestamp() + cfg.max_session_min * 60 + cfg.reconnect_grace_secs;
    Some(keys.mint_guest_token(&GuestClaims::new(session_id, peer_id, role, exp)))
}

#[derive(sqlx::FromRow)]
struct PeerRow {
    role: String,
    lang: String,
    display_name: String,
    user_id: Option<Uuid>,
    status: String,
    org_id: Option<Uuid>,
    owner_user_id: Option<Uuid>,
    project_id: Option<Uuid>,
    save_transcript: bool,
    engine_id: Option<String>,
    use_voice_clone: bool,
    created_at: DateTime<Utc>,
}

/// Resolve a verified token to a participant of a session that is still open.
///
/// The signature proves the token was minted by us; this proves the seat and the session are
/// still live, so an ended session stops admitting reconnects the moment it ends.
pub async fn authorize_peer(pool: &Pool, claims: &GuestClaims) -> Result<PeerAuth, AuthError> {
    let row: Option<PeerRow> = sqlx::query_as(
        "SELECT p.role, p.lang, p.display_name, p.user_id, s.status,
                s.org_id, s.owner_user_id, s.project_id,
                s.save_transcript, s.engine_id, s.use_voice_clone, s.created_at
         FROM live_participants p
         JOIN live_sessions s ON s.id = p.session_id
         WHERE p.peer_id = $1 AND p.session_id = $2
           AND s.status IN ('waiting', 'active')",
    )
    .bind(claims.pid)
    .bind(claims.sid)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten();
    let Some(row) = row else {
        return Err(AuthError::Gone);
    };

    let role = match row.role.as_str() {
        "host" => PeerRole::Host,
        "staff" => PeerRole::Staff,
        _ => PeerRole::Guest,
    };
    // A token names its own role; it must match the seat it claims.
    if role != claims.role {
        return Err(AuthError::Gone);
    }
    Ok(PeerAuth {
        session_id: claims.sid,
        peer_id: claims.pid,
        role,
        lang: row.lang,
        display_name: row.display_name,
        user_id: row.user_id,
        org_id: row.org_id,
        owner_user_id: row.owner_user_id,
        project_id: row.project_id,
        status: row.status,
        save_transcript: row.save_transcript,
        engine_id: row.engine_id,
        use_voice_clone: row.use_voice_clone,
        created_at: row.created_at,
    })
}

/// Remember which token a seat was given (hash only), so a support operator can tell a seat
/// was issued without ever being able to read the credential back.
pub async fn store_token_hash(pool: &Pool, peer_id: Uuid, token: &str) {
    if let Err(e) = sqlx::query("UPDATE live_participants SET token_hash = $2 WHERE peer_id = $1")
        .bind(peer_id)
        .bind(token_hash(token))
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "live: could not store token hash");
    }
}

/// The tier a stored `engine_id` stands for: a row that predates tiers (`NULL`) is Standard.
pub fn effective_engine(stored: Option<&str>) -> String {
    stored.unwrap_or(super::tiers::DEFAULT_TIER).to_string()
}

/// Move a conversation to `engine_id` (a capacity fallback). Only an open conversation changes.
pub async fn set_engine(pool: &Pool, session_id: Uuid, engine_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE live_sessions SET engine_id = $2
         WHERE id = $1 AND status IN ('waiting', 'active')",
    )
    .bind(session_id)
    .bind(engine_id)
    .execute(pool)
    .await?;
    Ok(())
}

// ---- end / expire ----------------------------------------------------------------------

/// End an open session. Returns whether this call was the one that ended it.
///
/// Generic over the executor so the platform admin can end a session inside the transaction that
/// also records the audit row.
pub async fn end_session<'e, E: sqlx::PgExecutor<'e>>(
    exec: E,
    session_id: Uuid,
    reason: &str,
) -> Result<bool, sqlx::Error> {
    let res = sqlx::query(
        "UPDATE live_sessions
         SET status = 'ended', ended_at = now(), end_reason = $2
         WHERE id = $1 AND status IN ('waiting', 'active')",
    )
    .bind(session_id)
    .bind(reason)
    .execute(exec)
    .await?;
    Ok(res.rows_affected() == 1)
}

/// Expire sessions nobody joined in time, and close sessions past their hard ceiling.
/// Returns the ids of the sessions that changed, so each can be finalised.
pub async fn expire_stale(pool: &Pool, cfg: &LiveConfig) -> Result<Vec<Uuid>, sqlx::Error> {
    let mut changed: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE live_sessions
         SET status = 'expired', ended_at = now(), end_reason = 'expired'
         WHERE status = 'waiting' AND expires_at <= now()
         RETURNING id",
    )
    .fetch_all(pool)
    .await?;
    let over: Vec<Uuid> = sqlx::query_scalar(
        "UPDATE live_sessions
         SET status = 'ended', ended_at = now(), end_reason = 'max_duration'
         WHERE status = 'active'
           AND created_at <= now() - make_interval(mins => $1::int)
         RETURNING id",
    )
    .bind(cfg.max_session_min as i32)
    .fetch_all(pool)
    .await?;
    changed.extend(over);
    Ok(changed)
}

// ---- views and diagnostics -------------------------------------------------------------

#[derive(Debug, serde::Serialize)]
pub struct ParticipantView {
    pub peer_id: Uuid,
    pub role: String,
    pub display_name: String,
    pub lang: String,
}

#[derive(Debug, serde::Serialize)]
pub struct SessionView {
    pub id: Uuid,
    pub status: String,
    pub host_lang: String,
    /// How many guests the invitation admits in all.
    pub max_guests: i16,
    /// The tier every seat of this conversation runs on (`standard` or `gemini_live_translate`).
    /// Follows a capacity fallback: after one it reads `standard`.
    pub engine: String,
    /// Whether the other phones speak the host's words in the host's cloned voice (Enhanced
    /// only). Never the voice id itself: that reaches a phone only inside `enhanced_speak`.
    pub voice_clone: bool,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub participants: Vec<ParticipantView>,
}

/// The session as a participant sees it: status and who is in it. No identifiers beyond
/// the peer ids already shared on the call, and no money.
pub async fn session_view(
    pool: &Pool,
    session_id: Uuid,
) -> Result<Option<SessionView>, sqlx::Error> {
    #[derive(sqlx::FromRow)]
    struct Head {
        status: String,
        host_lang: String,
        max_guests: i16,
        engine_id: Option<String>,
        use_voice_clone: bool,
        created_at: DateTime<Utc>,
        expires_at: DateTime<Utc>,
    }
    let head: Option<Head> = sqlx::query_as(
        "SELECT status, host_lang, max_guests, engine_id, use_voice_clone, created_at, expires_at
         FROM live_sessions WHERE id = $1",
    )
    .bind(session_id)
    .fetch_optional(pool)
    .await?;
    let Some(Head {
        status,
        host_lang,
        max_guests,
        engine_id,
        use_voice_clone,
        created_at,
        expires_at,
    }) = head
    else {
        return Ok(None);
    };
    let rows: Vec<(Uuid, String, String, String)> = sqlx::query_as(
        "SELECT peer_id, role, display_name, lang
         FROM live_participants WHERE session_id = $1 ORDER BY joined_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await?;
    Ok(Some(SessionView {
        id: session_id,
        status,
        host_lang,
        max_guests,
        engine: effective_engine(engine_id.as_deref()),
        voice_clone: use_voice_clone,
        created_at,
        expires_at,
        participants: rows
            .into_iter()
            .map(|(peer_id, role, display_name, lang)| ParticipantView {
                peer_id,
                role,
                display_name,
                lang,
            })
            .collect(),
    }))
}

/// Append a diagnostic event, silently dropping it past the per-session bound. Diagnostics
/// must never fail a call, so errors are logged, not returned.
pub async fn record_event(pool: &Pool, session_id: Uuid, kind: &str, detail: serde_json::Value) {
    let res = sqlx::query(
        "INSERT INTO live_session_events (session_id, kind, detail)
         SELECT $1, $2, $3
         WHERE $2 = 'room_session'
            OR (SELECT count(*) FROM live_session_events WHERE session_id = $1) < $4",
    )
    .bind(session_id)
    .bind(kind)
    .bind(detail)
    .bind(MAX_EVENTS_PER_SESSION)
    .execute(pool)
    .await;
    if let Err(e) = res {
        tracing::warn!(error = %e, kind, "live: could not record session event");
    }
}

/// SHA-256 of a token, for `live_participants.token_hash`.
pub fn token_hash(token: &str) -> Vec<u8> {
    Sha256::digest(token.as_bytes()).to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_room_needs_one_upstream_session_per_speaker_per_other_language() {
        // Alone, or all in one language: nothing to translate.
        assert_eq!(engine_sessions_needed(&[]), 0);
        assert_eq!(engine_sessions_needed(&["it"]), 0);
        assert_eq!(engine_sessions_needed(&["it", "it", "it"]), 0);
        // The classic pair: one stream each way.
        assert_eq!(engine_sessions_needed(&["it", "es"]), 2);
        // Three languages, four phones (two share one): each phone translates into the 2 others.
        assert_eq!(engine_sessions_needed(&["it", "es", "es", "en"]), 8);
        // Four phones, four languages: 4 x 3.
        assert_eq!(engine_sessions_needed(&["it", "es", "en", "fr"]), 12);
    }

    #[test]
    fn a_join_is_judged_by_what_the_newcomer_adds_and_never_below_the_two_of_a_pair() {
        // A first guest in another language: exactly the pair's two, as before.
        assert_eq!(engine_sessions_for_join(&["it"], "es"), 2);
        // A first guest in the host's own language adds nothing, but the old gate still wanted two.
        assert_eq!(engine_sessions_for_join(&["it"], "it"), 2);
        // A second guest in a third language: 3 phones x 2 languages = 6, minus the pair's 2.
        assert_eq!(engine_sessions_for_join(&["it", "es"], "en"), 4);
        // A second guest in a language already there: 3 phones x 1 other language = 3, minus the
        // pair's 2 is 1, floored at the two a conversation has always needed.
        assert_eq!(engine_sessions_for_join(&["it", "es"], "es"), 2);
        // The fourth phone of four languages: 12 minus the three phones' 6.
        assert_eq!(engine_sessions_for_join(&["it", "es", "en"], "fr"), 6);
    }

    #[test]
    fn languages_are_normalised_and_auto_is_refused() {
        assert_eq!(normalize_lang(" IT "), Some("it".into()));
        assert_eq!(normalize_lang("pt-BR"), Some("pt-br".into()));
        for bad in ["", "auto", "AUTO", "abcdefghi", "it_IT", "it!", "  "] {
            assert_eq!(normalize_lang(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn names_lose_control_characters_and_are_bounded() {
        assert_eq!(clean_name("  Marco\n\t "), "Marco");
        assert_eq!(clean_name("\u{0007}\u{0000}"), "Guest");
        assert_eq!(clean_name(""), "Guest");
        assert_eq!(clean_name(&"x".repeat(500)).chars().count(), 100);
        assert_eq!(clean_name("José 李"), "José 李");
        // Bidi overrides can make a name read as something it is not.
        assert_eq!(clean_name("Anna\u{202E}fdp.exe"), "Annafdp.exe");
        assert_eq!(clean_name("\u{2066}\u{200F}"), "Guest");
        // Zero-width, separator and invisible characters too: nothing of them survives, and a
        // name made only of them is an empty one.
        for c in [
            '\u{200B}', '\u{2028}', '\u{2029}', '\u{2060}', '\u{2061}', '\u{206F}', '\u{061C}',
            '\u{FEFF}',
        ] {
            assert_eq!(
                clean_name(&format!("An{c}na")),
                "Anna",
                "U+{:04X}",
                c as u32
            );
            assert_eq!(
                clean_name(&format!("{c}{c} ")),
                "Guest",
                "U+{:04X}",
                c as u32
            );
        }
        // Joiners are part of real writing (Persian, Sinhala, emoji sequences): kept inside a
        // name, but a name that is nothing else is an empty one. So are blank fillers.
        assert_eq!(clean_name("می\u{200C}خواهم"), "می\u{200C}خواهم");
        assert_eq!(clean_name("ශ්\u{200D}රී"), "ශ්\u{200D}රී");
        for only in [
            "\u{200C}\u{200D}",
            "\u{3164}",
            "\u{2800}\u{2800}",
            "\u{00AD}",
            "\u{034F}",
            "\u{E0041}\u{E0042}",
            " \u{3164} ",
        ] {
            assert_eq!(clean_name(only), "Guest", "{only:?}");
        }
        // Ordinary text is untouched.
        assert_eq!(clean_name("Zoë-Marie"), "Zoë-Marie");
    }
}
