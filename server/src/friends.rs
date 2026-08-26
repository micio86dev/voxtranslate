//! Persistent friends list with friend requests (send / accept / reject / cancel /
//! remove), plus inviting an existing friend into your current call.
//!
//! A friendship is ONE symmetric row keyed on the ordered UUID pair
//! (`user_low < user_high`) — see migration `029_friends.sql`. `requested_by` records
//! who sent a still-`pending` request so the UI can show it as incoming or outgoing;
//! accepting flips the row to `accepted`, while reject / cancel / unfriend all just
//! delete the row (one `DELETE` covers all three — see [`remove`]).
//!
//! The full request → accept → unfriend cycle, mutual-request auto-accept, the
//! error paths, and call invites are covered end-to-end in `tests/friends.rs`.

#![allow(clippy::result_large_err)]
// Every `Err` in this module IS the handler's HTTP response, the same reasoning
// already recorded at `api.rs:1626` and applied module-wide in `business/mod.rs`,
// `webinar/routes.rs` and `webinar/chat.rs`. Rust 1.98 widened `result_large_err`
// so it now reaches these files; the code did not change.
//
// The lint is about paying to move a large error up a call stack. There is no call
// stack here: an axum handler is the top of one, the `Response` is constructed once
// at the error site and handed straight to the framework. Boxing it would add an
// allocation and an indirection to every error path and buy nothing.

use std::sync::LazyLock;
use std::time::{Duration, Instant};

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use dashmap::DashMap;
use serde::{Deserialize, Serialize};
use serde_json::json;
use sqlx::FromRow;
use uuid::Uuid;

use crate::business::{bad_request, conflict, db_err, not_found, require_pool};
use crate::db::Pool;
use crate::middleware::AuthUser;
use crate::notifications::notify;
use crate::notify_copy::{friend_copy, user_locale};
use crate::AppState;

/// Order a pair of user ids so the smaller is always `user_low` — the storage form
/// that makes A↔B and B↔A the same row.
fn ordered(a: Uuid, b: Uuid) -> (Uuid, Uuid) {
    if a <= b {
        (a, b)
    } else {
        (b, a)
    }
}

// --- DTOs ----------------------------------------------------------------------

/// A user as surfaced in a friend list / request — only the public profile fields
/// the app already shows on a call (no balance, consent, ban state, etc.).
#[derive(Debug, Serialize, FromRow)]
pub struct FriendView {
    pub id: Uuid,
    pub name: String,
    pub email: String,
    pub avatar_url: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct RequestsView {
    /// Requests others sent to me (awaiting my accept/reject).
    pub incoming: Vec<FriendView>,
    /// Requests I sent that are still pending (I can cancel them).
    pub outgoing: Vec<FriendView>,
}

/// Send a request either by `email` (typed in the friends panel) or by `user_id`
/// (the "add friend" button next to a logged-in participant during a call).
#[derive(Debug, Deserialize)]
pub struct RequestInput {
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub user_id: Option<Uuid>,
}

#[derive(Debug, Deserialize)]
pub struct InviteInput {
    /// The room code of the call to invite the friend into.
    pub room: String,
}

// --- Handlers ------------------------------------------------------------------

/// `GET /api/friends` — the caller's accepted friends.
pub async fn list(State(state): State<AppState>, user: AuthUser) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let rows: Vec<FriendView> = sqlx::query_as(
        "SELECT u.id, u.name, u.email, u.avatar_url
           FROM friendships f
           JOIN users u
             ON u.id = CASE WHEN f.user_low = $1 THEN f.user_high ELSE f.user_low END
          WHERE f.status = 'accepted' AND (f.user_low = $1 OR f.user_high = $1)
          ORDER BY lower(u.name)",
    )
    .bind(user.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(rows).into_response())
}

/// `GET /api/friends/requests` — pending requests, split into incoming/outgoing.
pub async fn requests(State(state): State<AppState>, user: AuthUser) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    // Incoming: the other party (the requester) is exactly `requested_by`.
    let incoming: Vec<FriendView> = sqlx::query_as(
        "SELECT u.id, u.name, u.email, u.avatar_url
           FROM friendships f
           JOIN users u ON u.id = f.requested_by
          WHERE f.status = 'pending'
            AND (f.user_low = $1 OR f.user_high = $1)
            AND f.requested_by <> $1
          ORDER BY f.created_at DESC",
    )
    .bind(user.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    // Outgoing: the other party is the non-me side of the pair.
    let outgoing: Vec<FriendView> = sqlx::query_as(
        "SELECT u.id, u.name, u.email, u.avatar_url
           FROM friendships f
           JOIN users u
             ON u.id = CASE WHEN f.user_low = $1 THEN f.user_high ELSE f.user_low END
          WHERE f.status = 'pending'
            AND (f.user_low = $1 OR f.user_high = $1)
            AND f.requested_by = $1
          ORDER BY f.created_at DESC",
    )
    .bind(user.user_id)
    .fetch_all(pool)
    .await
    .map_err(db_err)?;
    Ok(Json(RequestsView { incoming, outgoing }).into_response())
}

/// `POST /api/friends/request` — send a friend request (by email or user id).
///
/// If the target already sent ME a pending request, this accepts it (mutual request
/// = instant friends) instead of erroring on the duplicate.
pub async fn request(
    State(state): State<AppState>,
    user: AuthUser,
    Json(body): Json<RequestInput>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;

    // Rate-limit per requester (M2): without this, /api/friends/request is a free
    // email-enumeration + notification-spam oracle. 20/min is far above any human use.
    if !state.rate_limiter.allow(
        &format!("friend_req:{}", user.user_id),
        20,
        Duration::from_secs(60),
    ) {
        return Err((StatusCode::TOO_MANY_REQUESTS, "slow down").into_response());
    }

    // Resolve + verify the target so the FK can't 500 on a bogus id.
    let target_id: Uuid = if let Some(uid) = body.user_id {
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE id = $1")
            .bind(uid)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?
            .ok_or_else(|| not_found("user not found"))?
    } else {
        let email = body.email.unwrap_or_default().trim().to_lowercase();
        if email.is_empty() {
            return Err(bad_request("email or user_id is required"));
        }
        match sqlx::query_scalar::<_, Uuid>("SELECT id FROM users WHERE lower(email) = $1")
            .bind(&email)
            .fetch_optional(pool)
            .await
            .map_err(db_err)?
        {
            Some(id) => id,
            // M2: don't reveal whether the email is registered. Return the SAME
            // 202 as a queued request instead of a distinguishable 404.
            None => return Ok(StatusCode::ACCEPTED.into_response()),
        }
    };

    if target_id == user.user_id {
        return Err(bad_request("you can't add yourself"));
    }

    let (low, high) = ordered(user.user_id, target_id);
    let existing: Option<(String, Uuid)> = sqlx::query_as(
        "SELECT status, requested_by FROM friendships WHERE user_low = $1 AND user_high = $2",
    )
    .bind(low)
    .bind(high)
    .fetch_optional(pool)
    .await
    .map_err(db_err)?;

    if let Some((status, requested_by)) = existing {
        if status == "accepted" {
            return Err(conflict("already friends"));
        }
        if requested_by == user.user_id {
            return Err(conflict("request already sent"));
        }
        // The target had already requested me → accepting completes the friendship.
        sqlx::query(
            "UPDATE friendships SET status = 'accepted', accepted_at = now()
              WHERE user_low = $1 AND user_high = $2",
        )
        .bind(low)
        .bind(high)
        .execute(pool)
        .await
        .map_err(db_err)?;
        notify_friend(&state, pool, requested_by, "friend_accepted", &user).await;
        return Ok(StatusCode::NO_CONTENT.into_response());
    }

    sqlx::query(
        "INSERT INTO friendships (user_low, user_high, status, requested_by)
         VALUES ($1, $2, 'pending', $3)",
    )
    .bind(low)
    .bind(high)
    .bind(user.user_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    notify_friend(&state, pool, target_id, "friend_request", &user).await;
    // 202 (not 201) so this is indistinguishable from the unknown-email path (M2).
    Ok(StatusCode::ACCEPTED.into_response())
}

/// `POST /api/friends/{id}/accept` — accept a pending request from `{id}`.
pub async fn accept(
    State(state): State<AppState>,
    user: AuthUser,
    Path(other): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (low, high) = ordered(user.user_id, other);
    // Only the recipient (not the requester) can accept, and only while pending.
    let res = sqlx::query(
        "UPDATE friendships SET status = 'accepted', accepted_at = now()
          WHERE user_low = $1 AND user_high = $2
            AND status = 'pending' AND requested_by <> $3",
    )
    .bind(low)
    .bind(high)
    .bind(user.user_id)
    .execute(pool)
    .await
    .map_err(db_err)?;
    if res.rows_affected() == 0 {
        return Err(not_found("no pending request from that user"));
    }
    notify_friend(&state, pool, other, "friend_accepted", &user).await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `DELETE /api/friends/{id}` — reject a request, cancel one I sent, or unfriend.
/// All three are the same row deletion; idempotent (deleting nothing still 204s).
pub async fn remove(
    State(state): State<AppState>,
    user: AuthUser,
    Path(other): Path<Uuid>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    let (low, high) = ordered(user.user_id, other);
    sqlx::query("DELETE FROM friendships WHERE user_low = $1 AND user_high = $2")
        .bind(low)
        .bind(high)
        .execute(pool)
        .await
        .map_err(db_err)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// `POST /api/friends/{id}/invite` — invite an accepted friend into a call. Delivers
/// a notification (in-app / push / email) with the room's join link.
pub async fn invite(
    State(state): State<AppState>,
    user: AuthUser,
    Path(other): Path<Uuid>,
    Json(body): Json<InviteInput>,
) -> Result<Response, Response> {
    let pool = require_pool(&state)?;
    // Sanitize the room code (L5) — same rules as the other invite path — so it can't
    // inject query params into the join link or carry an unsafe value into the
    // notification payload.
    let Some(room) = crate::invite::sanitize_room(&body.room) else {
        return Err(bad_request("invalid room code"));
    };
    let (low, high) = ordered(user.user_id, other);
    let is_friend: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM friendships
                        WHERE user_low = $1 AND user_high = $2 AND status = 'accepted')",
    )
    .bind(low)
    .bind(high)
    .fetch_one(pool)
    .await
    .map_err(db_err)?;
    if !is_friend {
        return Err(not_found("not friends"));
    }
    let join_url = format!("{}/?room={}", state.config.app_base_url, room);
    let lang = user_locale(pool, other).await;
    let (title, text) = friend_copy("call_invite", &lang, &user.name);
    notify(
        &state,
        other,
        "call_invite",
        &lang,
        &title,
        &text,
        json!({ "from_user_id": user.user_id, "from_name": user.name, "room": room, "join_url": join_url }),
    )
    .await;
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// Fan a friend-event notification out to `recipient` in their own language, naming
/// the `actor` (the other person) in the body.
async fn notify_friend(
    state: &AppState,
    pool: &Pool,
    recipient: Uuid,
    kind: &str,
    actor: &AuthUser,
) {
    let lang = user_locale(pool, recipient).await;
    let (title, text) = friend_copy(kind, &lang, &actor.name);
    notify(
        state,
        recipient,
        kind,
        &lang,
        &title,
        &text,
        json!({ "from_user_id": actor.user_id, "from_name": actor.name }),
    )
    .await;
}

// --- "Friend is in a public room" alert (spec: friends, presence) ----------------

/// Anti-spam guard: the last time we told `recipient` that `joiner` is in `room`.
/// In-memory + pruned opportunistically — a dropped entry just means one extra alert.
static FRIEND_ALERT_SEEN: LazyLock<DashMap<(Uuid, Uuid, String), Instant>> =
    LazyLock::new(DashMap::new);
const FRIEND_ALERT_COOLDOWN: Duration = Duration::from_secs(15 * 60);

/// When an authenticated user joins a PUBLIC room, alert their accepted friends so they
/// can drop in for a chat. Best-effort and rate-limited: skips friends who are currently
/// in a call (online), and won't re-alert the same (friend, joiner, room) within the
/// cooldown. Per-channel notification preferences are enforced by [`notify`] — a friend
/// who turned the `friend_active` event off simply isn't reached on that channel.
pub async fn notify_friends_of_public_join(
    state: AppState,
    joiner_id: Uuid,
    joiner_name: String,
    room: String,
) {
    let Some(pool) = state.pool.as_ref() else {
        return;
    };
    // A user who is live-hosting a webinar isn't "hanging out in a public room" — don't
    // nudge their friends to jump into a meet room during their broadcast. The
    // friend_active banner is only ever produced by a public MEET-room join, but a host
    // who opens a public room around their webinar would otherwise ping friends as if
    // they were free to chat. Best-effort: on a DB error, fall through to normal behaviour.
    match sqlx::query_scalar::<_, bool>(
        "SELECT EXISTS(SELECT 1 FROM webinars WHERE host_user_id = $1 AND status = 'live')",
    )
    .bind(joiner_id)
    .fetch_one(pool)
    .await
    {
        Ok(true) => return,
        Ok(false) => {}
        Err(e) => tracing::warn!("friend alert: live-webinar host check failed: {e}"),
    }
    // Drop stale dedupe entries so the map can't grow unbounded.
    FRIEND_ALERT_SEEN.retain(|_, t| t.elapsed() < FRIEND_ALERT_COOLDOWN);

    let friends: Vec<Uuid> = match sqlx::query_scalar(
        "SELECT CASE WHEN user_low = $1 THEN user_high ELSE user_low END
           FROM friendships
          WHERE status = 'accepted' AND (user_low = $1 OR user_high = $1)",
    )
    .bind(joiner_id)
    .fetch_all(pool)
    .await
    {
        Ok(f) => f,
        Err(e) => {
            tracing::error!("friend alert: load friends failed: {e}");
            return;
        }
    };
    if friends.is_empty() {
        return;
    }

    let join_url = format!("{}/?room={}", state.config.app_base_url, room);
    for friend_id in friends {
        // Don't interrupt a friend who's already in a call.
        if state.rooms.is_user_online(friend_id) {
            continue;
        }
        let key = (friend_id, joiner_id, room.clone());
        if FRIEND_ALERT_SEEN
            .get(&key)
            .is_some_and(|t| t.elapsed() < FRIEND_ALERT_COOLDOWN)
        {
            continue;
        }
        FRIEND_ALERT_SEEN.insert(key, Instant::now());

        let lang = user_locale(pool, friend_id).await;
        let (title, body) = friend_copy("friend_active", &lang, &joiner_name);
        notify(
            &state,
            friend_id,
            "friend_active",
            &lang,
            &title,
            &body,
            json!({ "from_user_id": joiner_id, "from_name": joiner_name, "room": room, "join_url": join_url }),
        )
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ordered_is_symmetric() {
        let a = Uuid::from_u128(1);
        let b = Uuid::from_u128(2);
        assert_eq!(ordered(a, b), ordered(b, a));
        assert_eq!(ordered(a, b), (a, b));
    }
}
