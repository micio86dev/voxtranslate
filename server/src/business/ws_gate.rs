//! Eligibility gates for the assistant WebSocket endpoints.
//!
//! These endpoints used to reject an ineligible org with HTTP 402 during the
//! WebSocket **upgrade**. That status is unreachable from a browser: the
//! `WebSocket` API hands JavaScript a bare `error` event and never exposes the
//! handshake response, so "your subscription lapsed" and "the network is down"
//! arrived at the dashboard as the same thing — and the dashboard, having
//! nothing else to say, said "connection error".
//!
//! So anything the user can act on is reported IN-BAND instead: accept the
//! upgrade, send one `{"type":"error","code":…}` frame, close. That is already
//! how `capacity_full` works (`engine::help_assistant`), and the dashboard
//! already parses those frames. Auth and membership stay pre-upgrade HTTP
//! errors — a client that fails those has no business opening a socket.

use uuid::Uuid;

use crate::db::Pool;

/// Minimum org credit balance required to START an assistant session, so we
/// don't open one that the first tick would immediately tear down.
pub const MIN_SESSION_CREDITS: i32 = 10;

/// Why an org may not open an assistant session right now. Both variants are
/// the user's to resolve, which is why they travel in-band.
#[derive(Debug, PartialEq, Eq)]
pub enum Ineligible {
    /// No active Business/Enterprise subscription (never had one, or the paid
    /// period has ended — a `cancel_at_period_end` sub lapses silently).
    SubscriptionRequired,
    /// Subscribed, but the credit pool can't cover a session.
    InsufficientCredits { balance: i32, required: i32 },
}

impl Ineligible {
    /// Stable machine code the dashboard routes on.
    pub fn code(&self) -> &'static str {
        match self {
            Self::SubscriptionRequired => "subscription_required",
            Self::InsufficientCredits { .. } => "insufficient_credits",
        }
    }

    /// What the user is expected to DO about it. Without this the dashboard can
    /// only show a message; with it, it can show a button.
    fn action(&self) -> &'static str {
        match self {
            Self::SubscriptionRequired => "purchase_subscription",
            Self::InsufficientCredits { .. } => "purchase_credits",
        }
    }

    fn message(&self) -> &'static str {
        match self {
            Self::SubscriptionRequired => {
                "This feature needs an active Business or Enterprise subscription."
            }
            Self::InsufficientCredits { .. } => {
                "Not enough organization credits to start this session."
            }
        }
    }

    /// The in-band error frame, shaped like every other one the assistants send
    /// (`capacity_full`, `credits_exhausted`) so the client parses it unchanged.
    pub fn to_json(&self) -> String {
        let mut body = serde_json::json!({
            "type": "error",
            "code": self.code(),
            "action": self.action(),
            "message": self.message(),
        });
        if let Self::InsufficientCredits { balance, required } = self {
            body["balance"] = (*balance).into();
            body["required"] = (*required).into();
        }
        body.to_string()
    }
}

/// Check whether `org_id` may open an assistant session: active subscription
/// first, then enough credits to be worth starting. `Ok(None)` = go ahead.
pub async fn check(pool: &Pool, org_id: Uuid) -> Result<Option<Ineligible>, sqlx::Error> {
    if !crate::business::credits::org_subscription_active(pool, org_id).await? {
        return Ok(Some(Ineligible::SubscriptionRequired));
    }
    let balance: Option<i32> =
        sqlx::query_scalar("SELECT credits_balance FROM organizations WHERE id = $1")
            .bind(org_id)
            .fetch_optional(pool)
            .await?;
    let balance = balance.unwrap_or(0);
    if balance < MIN_SESSION_CREDITS {
        return Ok(Some(Ineligible::InsufficientCredits {
            balance,
            required: MIN_SESSION_CREDITS,
        }));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(reason: &Ineligible) -> serde_json::Value {
        serde_json::from_str(&reason.to_json()).expect("valid JSON")
    }

    #[test]
    fn subscription_required_frame_names_its_cause_and_the_way_out() {
        let v = parse(&Ineligible::SubscriptionRequired);
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "subscription_required");
        // The dashboard routes on `action` — a generic error frame would leave the
        // user staring at a dead panel with nothing to click.
        assert_eq!(v["action"], "purchase_subscription");
        assert!(v["message"].as_str().is_some_and(|m| !m.is_empty()));
    }

    #[test]
    fn insufficient_credits_frame_carries_the_numbers() {
        let v = parse(&Ineligible::InsufficientCredits {
            balance: 3,
            required: MIN_SESSION_CREDITS,
        });
        assert_eq!(v["type"], "error");
        assert_eq!(v["code"], "insufficient_credits");
        assert_eq!(v["action"], "purchase_credits");
        assert_eq!(v["balance"], 3);
        assert_eq!(v["required"], 10);
    }

    #[test]
    fn the_two_reasons_are_distinguishable() {
        assert_ne!(
            Ineligible::SubscriptionRequired.code(),
            Ineligible::InsufficientCredits {
                balance: 0,
                required: 10
            }
            .code()
        );
    }
}
