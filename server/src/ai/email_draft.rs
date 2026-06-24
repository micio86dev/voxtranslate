//! Follow-up email draft (spec 0016): turn a call transcript into an editable
//! recap email via Groq, billed flat per draft, sent through Resend.
//!
//! Privacy contract: other participants' email addresses NEVER reach the
//! requester. Drafts store participant refs as `user_id` + display name; the
//! send handler resolves ids → addresses server-side, and `sanitize_recipients`
//! strips ids before anything goes over the wire.

use std::time::Duration;

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

use super::{condense_transcript, transcript_to_text};
use crate::billing::usd;
use crate::config::AiConfig;
use crate::db::Pool;
use crate::groq::{ChatRequest, Groq};
use crate::transcripts::TranscriptExport;

/// Flat per-draft cost: `CREDITS_EMAIL_DRAFT`.
pub fn email_cost(ai: &AiConfig) -> Decimal {
    usd(ai.email_draft).round_dp(6)
}

/// Hard cap on To+CC entries per email.
pub const MAX_RECIPIENTS: usize = 10;

/// A recipient as the client sends it: either a session participant by peer
/// id (resolved to a user id server-side) or a raw typed address.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecipientRef {
    Participant {
        peer_id: String,
        #[serde(default)]
        cc: bool,
    },
    Email {
        email: String,
        #[serde(default)]
        cc: bool,
    },
}

/// Pragmatic address check — enough to reject typos before Resend does.
pub fn valid_email(s: &str) -> bool {
    if s.len() > 254 || s.chars().any(char::is_whitespace) {
        return false;
    }
    let Some((local, domain)) = s.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && !domain.is_empty()
        && domain.contains('.')
        && !domain.starts_with('.')
        && !domain.ends_with('.')
        && !domain.contains('@')
}

/// Resolve client recipient refs against the session's participant list
/// (`(peer_id, user_id, name)`) into the JSONB shape stored on the draft:
/// `{"kind":"participant","user_id":…,"name":…,"cc":…}` or
/// `{"kind":"email","email":…,"cc":…}`. Errors are user-facing 400 messages.
pub fn resolve_recipients(
    refs: &[RecipientRef],
    participants: &[(String, Option<Uuid>, String)],
) -> Result<serde_json::Value, String> {
    if refs.is_empty() {
        return Err("at least one recipient is required".into());
    }
    if refs.len() > MAX_RECIPIENTS {
        return Err(format!("too many recipients (max {MAX_RECIPIENTS})"));
    }
    let mut seen_users: Vec<Uuid> = Vec::new();
    let mut seen_emails: Vec<String> = Vec::new();
    let mut out = Vec::new();
    let mut has_to = false;
    for r in refs {
        match r {
            RecipientRef::Participant { peer_id, cc } => {
                let Some((_, user_id, name)) =
                    participants.iter().find(|(pid, _, _)| pid == peer_id)
                else {
                    return Err(format!("unknown participant `{peer_id}`"));
                };
                let Some(uid) = user_id else {
                    return Err(format!("{name} joined as a guest and has no account email"));
                };
                if seen_users.contains(uid) {
                    continue; // same account joined twice — one email
                }
                seen_users.push(*uid);
                if !cc {
                    has_to = true;
                }
                out.push(serde_json::json!({
                    "kind": "participant", "user_id": uid, "name": name, "cc": cc,
                }));
            }
            RecipientRef::Email { email, cc } => {
                let email = email.trim().to_lowercase();
                if !valid_email(&email) {
                    return Err(format!("invalid email address `{email}`"));
                }
                if seen_emails.contains(&email) {
                    continue;
                }
                seen_emails.push(email.clone());
                if !cc {
                    has_to = true;
                }
                out.push(serde_json::json!({ "kind": "email", "email": email, "cc": cc }));
            }
        }
    }
    if !has_to {
        return Err("at least one non-CC recipient is required".into());
    }
    Ok(serde_json::Value::Array(out))
}

/// Strip server-only fields (`user_id`) from stored recipients before they go
/// into any API response. Raw typed addresses echo back — the requester typed
/// them — but participant entries expose the display name only.
pub fn sanitize_recipients(recipients: &serde_json::Value) -> serde_json::Value {
    let Some(items) = recipients.as_array() else {
        return serde_json::Value::Array(vec![]);
    };
    let out = items
        .iter()
        .map(|r| {
            if r["kind"] == "participant" {
                serde_json::json!({
                    "kind": "participant", "name": r["name"], "cc": r["cc"],
                })
            } else {
                serde_json::json!({ "kind": "email", "email": r["email"], "cc": r["cc"] })
            }
        })
        .collect();
    serde_json::Value::Array(out)
}

/// Pull the text under `## Executive Summary` from a stored report so the
/// draft prompt can lead with it instead of re-deriving the gist.
pub fn extract_exec_summary(markdown: &str) -> Option<String> {
    let mut lines = markdown.lines();
    lines.find(|l| l.trim().eq_ignore_ascii_case("## executive summary"))?;
    let body: Vec<&str> = lines
        .take_while(|l| !l.trim_start().starts_with("## "))
        .collect();
    let text = body.join("\n").trim().to_string();
    (!text.is_empty()).then_some(text)
}

/// Minimal text→HTML: escape, then `\n\n` → paragraphs, `\n` → `<br>`. Used
/// as the fallback when the model omits `body_html` and for user-edited text.
pub fn text_to_html(text: &str) -> String {
    let escaped = text
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;");
    escaped
        .split("\n\n")
        .filter(|p| !p.trim().is_empty())
        .map(|p| format!("<p>{}</p>", p.trim().replace('\n', "<br>")))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Build the draft system prompt. Pure, for tests. Tone is whitelisted at the
/// API layer ("professional"|"friendly"|"concise") so it can sit in a
/// directive slot; guidelines are data with explicit precedence rules.
pub fn draft_prompt(
    tone: Option<&str>,
    lang: &str,
    guidelines: Option<&str>,
    has_summary: bool,
) -> String {
    let mut system = String::from(
        "You write follow-up emails after meetings, from the call transcript. The \
         transcript lines are `[HH:MM:SS] Speaker (lang): text`; `[chat]` marks \
         text-chat messages. Participants may speak different languages.\n",
    );
    if has_summary {
        system.push_str(
            "An EXECUTIVE SUMMARY from the session report precedes the transcript — \
             lead with it, then fill in specifics from the transcript.\n",
        );
    }
    system.push_str(
        "Write a recap email: what was discussed, decisions made, and next steps \
         with owners. Do not invent facts not present in the transcript.\n",
    );
    system.push_str(&format!("Tone: {}.\n", tone.unwrap_or("professional")));
    system.push_str(&format!(
        "Write the email in {}.\n",
        crate::groq::lang_name(lang),
    ));
    if let Some(g) = guidelines {
        system.push_str(&format!(
            "\nThe requester added these guidelines (apply them where they don't conflict \
             with the rules above):\n{g}\n"
        ));
    }
    system.push_str(
        "\nRespond with a JSON object: {\"subject\": string, \"body_text\": string \
         (plain text, paragraphs separated by blank lines), \"body_html\": string \
         (simple HTML, <p>/<ul>/<li>/<strong> only)}. No greeting placeholders like \
         [Name] — address the team collectively.",
    );
    system
}

/// A generated draft, pre-persistence.
#[derive(Debug)]
pub struct EmailDraft {
    pub subject: String,
    pub body_text: String,
    pub body_html: String,
}

/// Generate the draft. Returns `(draft, model_used)`. Mirrors the report's
/// fallback-model dance: 4xx from the primary model retries once on the
/// fallback so a Groq model retirement degrades instead of breaking.
pub async fn generate_draft(
    groq: &Groq,
    ai: &AiConfig,
    export: &TranscriptExport,
    report_summary: Option<&str>,
    tone: Option<&str>,
    lang: &str,
    guidelines: Option<&str>,
) -> Result<(EmailDraft, String), String> {
    let text = condense_transcript(groq, ai, transcript_to_text(export)).await?;
    let user = match report_summary {
        Some(s) => {
            format!("EXECUTIVE SUMMARY (from the session report):\n{s}\n\nTRANSCRIPT:\n{text}")
        }
        None => text,
    };
    let system = draft_prompt(tone, lang, guidelines, report_summary.is_some());

    let mut req = ChatRequest::new(ai.report_model.clone(), system, user);
    // gpt-oss reasoning + a full email can exceed 1536 (Groq returns
    // `json_validate_failed` / "max completion tokens reached"); give it room.
    req.max_tokens = 3072;
    req.temperature = 0.3;
    req.timeout = Duration::from_secs(30);
    req.max_retries = 3;

    let fallback_req = ChatRequest {
        model: ai.fallback_model.clone(),
        ..req.clone()
    };
    let (v, model) = match groq.chat_json(req).await {
        Ok(v) => (v, ai.report_model.clone()),
        // 4xx = the model id itself was rejected; other failures aren't the
        // model's fault and retrying would double the wait for the same outcome.
        Err(e) if e.contains("groq returned 4") && ai.fallback_model != ai.report_model => {
            tracing::warn!("email-draft model failed ({e}); retrying on fallback model");
            (
                groq.chat_json(fallback_req).await?,
                ai.fallback_model.clone(),
            )
        }
        Err(e) => return Err(e),
    };

    let subject: String = v["subject"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(200)
        .collect();
    let body_text = v["body_text"]
        .as_str()
        .unwrap_or_default()
        .trim()
        .to_string();
    if subject.is_empty() || body_text.is_empty() {
        return Err("model returned an incomplete draft".into());
    }
    let body_html = match v["body_html"].as_str().map(str::trim) {
        Some(h) if !h.is_empty() => h.to_string(),
        _ => text_to_html(&body_text),
    };
    Ok((
        EmailDraft {
            subject,
            body_text,
            body_html,
        },
        model,
    ))
}

/// One stored email, as the endpoints see it (json-shaping happens in api.rs).
#[derive(Debug, sqlx::FromRow)]
pub struct EmailRow {
    pub id: Uuid,
    pub user_id: Uuid,
    pub status: String,
    pub subject: String,
    pub body_html: String,
    pub body_text: String,
    pub recipients: serde_json::Value,
    pub tone: Option<String>,
    pub guidelines: Option<String>,
    pub lang: Option<String>,
    pub resend_id: Option<String>,
    pub sent_at: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
}

const EMAIL_COLUMNS: &str = "id, user_id, status, subject, body_html, body_text, \
                             recipients, tone, guidelines, lang, resend_id, sent_at, created_at";

/// Persist a generated draft; multiple per session (regenerate keeps history).
#[allow(clippy::too_many_arguments)]
pub async fn save_email(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
    draft: &EmailDraft,
    recipients: &serde_json::Value,
    tone: Option<&str>,
    guidelines: Option<&str>,
    lang: &str,
) -> Result<EmailRow, sqlx::Error> {
    sqlx::query_as(&format!(
        "INSERT INTO session_emails
             (session_id, user_id, subject, body_html, body_text, recipients, tone, guidelines, lang)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9)
         RETURNING {EMAIL_COLUMNS}",
    ))
    .bind(session_id)
    .bind(user_id)
    .bind(&draft.subject)
    .bind(&draft.body_html)
    .bind(&draft.body_text)
    .bind(recipients)
    .bind(tone)
    .bind(guidelines)
    .bind(lang)
    .fetch_one(pool)
    .await
}

/// Latest email for a session, OWNER-scoped — drafts can hold raw addresses
/// the requester typed, which must not leak to other participants.
pub async fn latest_email(
    pool: &Pool,
    session_id: Uuid,
    user_id: Uuid,
) -> Result<Option<EmailRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {EMAIL_COLUMNS} FROM session_emails
         WHERE session_id = $1 AND user_id = $2
         ORDER BY created_at DESC LIMIT 1",
    ))
    .bind(session_id)
    .bind(user_id)
    .fetch_optional(pool)
    .await
}

/// One email by id, scoped to its session (ownership is checked in the handler
/// so a mismatch can 403 instead of 404).
pub async fn get_email(
    pool: &Pool,
    session_id: Uuid,
    email_id: Uuid,
) -> Result<Option<EmailRow>, sqlx::Error> {
    sqlx::query_as(&format!(
        "SELECT {EMAIL_COLUMNS} FROM session_emails WHERE id = $1 AND session_id = $2",
    ))
    .bind(email_id)
    .bind(session_id)
    .fetch_optional(pool)
    .await
}

/// Persist pre-send edits (subject/body). Send rebuilds body_html from the
/// edited text, so both stay in sync.
pub async fn update_draft(
    pool: &Pool,
    email_id: Uuid,
    subject: &str,
    body_html: &str,
    body_text: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session_emails SET subject = $2, body_html = $3, body_text = $4 WHERE id = $1",
    )
    .bind(email_id)
    .bind(subject)
    .bind(body_html)
    .bind(body_text)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Flip a draft to sent and record Resend's message id.
pub async fn mark_sent(pool: &Pool, email_id: Uuid, resend_id: &str) -> Result<(), sqlx::Error> {
    sqlx::query(
        "UPDATE session_emails SET status = 'sent', resend_id = $2, sent_at = now() WHERE id = $1",
    )
    .bind(email_id)
    .bind(resend_id)
    .execute(pool)
    .await
    .map(|_| ())
}

/// Session participants as `(peer_id, user_id, name)` for recipient resolution.
pub async fn session_participants(
    pool: &Pool,
    session_id: Uuid,
) -> Result<Vec<(String, Option<Uuid>, String)>, sqlx::Error> {
    sqlx::query_as(
        "SELECT peer_id, user_id, name FROM session_participants
         WHERE session_id = $1 ORDER BY joined_at",
    )
    .bind(session_id)
    .fetch_all(pool)
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    fn ai() -> AiConfig {
        Config::test_with_billing("postgres://x", "s", 2.0)
            .billing
            .unwrap()
            .ai
    }

    fn participants() -> Vec<(String, Option<Uuid>, String)> {
        vec![
            ("p1".into(), Some(Uuid::nil()), "Anna".into()),
            ("p2".into(), None, "Guest Gio".into()),
        ]
    }

    #[test]
    fn email_cost_is_flat_draft_price() {
        assert_eq!(email_cost(&ai()), usd(0.02));
    }

    #[test]
    fn valid_email_accepts_and_rejects() {
        assert!(valid_email("a@b.co"));
        assert!(valid_email("first.last+tag@sub.example.com"));
        assert!(!valid_email("nope"));
        assert!(!valid_email("@b.co"));
        assert!(!valid_email("a@"));
        assert!(!valid_email("a@nodot"));
        assert!(!valid_email("a@.dot"));
        assert!(!valid_email("a@dot."));
        assert!(!valid_email("a b@x.co"));
        assert!(!valid_email("a@b@c.co"));
        assert!(!valid_email(&format!("{}@x.co", "a".repeat(255))));
    }

    #[test]
    fn resolve_recipients_happy_path_and_dedup() {
        let refs = vec![
            RecipientRef::Participant {
                peer_id: "p1".into(),
                cc: false,
            },
            RecipientRef::Participant {
                peer_id: "p1".into(),
                cc: true,
            }, // dup user
            RecipientRef::Email {
                email: "Ext@X.com".into(),
                cc: true,
            },
            RecipientRef::Email {
                email: "ext@x.com".into(),
                cc: false,
            }, // dup email
        ];
        let v = resolve_recipients(&refs, &participants()).unwrap();
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), 2, "dups collapsed: {v}");
        assert_eq!(items[0]["kind"], "participant");
        assert_eq!(items[0]["name"], "Anna");
        assert_eq!(items[0]["user_id"], Uuid::nil().to_string());
        assert_eq!(items[1]["kind"], "email");
        assert_eq!(items[1]["email"], "ext@x.com", "lowercased");
    }

    #[test]
    fn resolve_recipients_rejects_bad_refs() {
        let p = participants();
        let err = |refs: Vec<RecipientRef>| resolve_recipients(&refs, &p).unwrap_err();

        assert!(err(vec![]).contains("at least one recipient"));
        assert!(err(vec![RecipientRef::Participant {
            peer_id: "ghost".into(),
            cc: false
        }])
        .contains("unknown participant `ghost`"));
        assert!(err(vec![RecipientRef::Participant {
            peer_id: "p2".into(),
            cc: false
        }])
        .contains("guest"),);
        assert!(err(vec![RecipientRef::Email {
            email: "bad".into(),
            cc: false
        }])
        .contains("invalid email"));
        // CC-only — nobody in To.
        assert!(err(vec![RecipientRef::Email {
            email: "a@x.co".into(),
            cc: true
        }])
        .contains("non-CC"));
        // Over the cap.
        let many: Vec<_> = (0..=MAX_RECIPIENTS)
            .map(|i| RecipientRef::Email {
                email: format!("u{i}@x.co"),
                cc: false,
            })
            .collect();
        assert!(err(many).contains("too many recipients (max 10)"));
    }

    #[test]
    fn sanitize_recipients_strips_user_id() {
        let stored = serde_json::json!([
            { "kind": "participant", "user_id": Uuid::nil(), "name": "Anna", "cc": false },
            { "kind": "email", "email": "ext@x.com", "cc": true },
        ]);
        let clean = sanitize_recipients(&stored);
        assert!(!clean.to_string().contains("user_id"), "{clean}");
        assert_eq!(clean[0]["name"], "Anna");
        assert_eq!(clean[1]["email"], "ext@x.com");
    }

    #[test]
    fn extract_exec_summary_finds_section() {
        let md = "# Report\n\n## Executive Summary\nWe agreed on Q3 dates.\nBudget set.\n\n## Key Points\n- a\n";
        assert_eq!(
            extract_exec_summary(md).as_deref(),
            Some("We agreed on Q3 dates.\nBudget set.")
        );
        assert_eq!(extract_exec_summary("## Key Points\n- a\n"), None);
        assert_eq!(
            extract_exec_summary("## Executive Summary\n\n## Key Points\n"),
            None
        );
    }

    #[test]
    fn text_to_html_escapes_and_paragraphs() {
        let html = text_to_html("a & b <c>\nline2\n\npara two");
        assert_eq!(html, "<p>a &amp; b &lt;c&gt;<br>line2</p>\n<p>para two</p>");
    }

    #[test]
    fn draft_prompt_tone_lang_summary_guidelines() {
        let p = draft_prompt(Some("friendly"), "it", Some("mention the demo"), true);
        assert!(p.contains("Tone: friendly."));
        assert!(p.contains("Italian"));
        assert!(p.contains("EXECUTIVE SUMMARY"));
        assert!(p.contains("mention the demo"));
        assert!(
            p.contains("JSON"),
            "json_mode needs the word JSON in the prompt"
        );

        let d = draft_prompt(None, "en", None, false);
        assert!(d.contains("Tone: professional."));
        assert!(!d.contains("EXECUTIVE SUMMARY"));
        assert!(!d.contains("guidelines"));
    }
}
