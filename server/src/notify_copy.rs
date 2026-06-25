//! Localized copy for meeting notifications (in-app / email / push), in the
//! **recipient's** language. Mirrors the language bar of `invite.rs`: full strings
//! for it/es/fr/de/pt/ja/zh, English fallback for everything else.
//!
//! [`meeting_copy`] returns the `(title, body)` pair for a notification `kind`
//! (`meeting_invited` | `meeting_reminder` | `meeting_updated` | `meeting_cancelled`),
//! with the meeting title interpolated into the title line. [`user_locale`] looks up
//! the recipient's stored UI locale (defaults to English).

use uuid::Uuid;

use crate::db::Pool;

/// The four notification copies for one language. Each `*_title` is a short prefix
/// shown before the meeting title (e.g. "Invitation: Weekly sync").
struct Copy {
    invited_title: &'static str,
    invited_body: &'static str,
    reminder_title: &'static str,
    reminder_body: &'static str,
    updated_title: &'static str,
    updated_body: &'static str,
    cancelled_title: &'static str,
    cancelled_body: &'static str,
}

fn copy(lang: &str) -> &'static Copy {
    match lang {
        "it" => &Copy {
            invited_title: "Invito",
            invited_body: "Sei stato invitato a una riunione.",
            reminder_title: "Tra poco",
            reminder_body: "La tua riunione sta per iniziare.",
            updated_title: "Aggiornata",
            updated_body: "I dettagli della riunione sono cambiati.",
            cancelled_title: "Annullata",
            cancelled_body: "La riunione è stata annullata.",
        },
        "es" => &Copy {
            invited_title: "Invitación",
            invited_body: "Te han invitado a una reunión.",
            reminder_title: "Empieza pronto",
            reminder_body: "Tu reunión está a punto de empezar.",
            updated_title: "Actualizada",
            updated_body: "Los detalles de la reunión han cambiado.",
            cancelled_title: "Cancelada",
            cancelled_body: "La reunión ha sido cancelada.",
        },
        "fr" => &Copy {
            invited_title: "Invitation",
            invited_body: "Vous avez été invité à une réunion.",
            reminder_title: "Bientôt",
            reminder_body: "Votre réunion va bientôt commencer.",
            updated_title: "Mise à jour",
            updated_body: "Les détails de la réunion ont changé.",
            cancelled_title: "Annulée",
            cancelled_body: "La réunion a été annulée.",
        },
        "de" => &Copy {
            invited_title: "Einladung",
            invited_body: "Du wurdest zu einer Besprechung eingeladen.",
            reminder_title: "Beginnt bald",
            reminder_body: "Deine Besprechung beginnt gleich.",
            updated_title: "Aktualisiert",
            updated_body: "Die Besprechungsdetails haben sich geändert.",
            cancelled_title: "Abgesagt",
            cancelled_body: "Die Besprechung wurde abgesagt.",
        },
        "pt" => &Copy {
            invited_title: "Convite",
            invited_body: "Você foi convidado para uma reunião.",
            reminder_title: "Em breve",
            reminder_body: "Sua reunião está prestes a começar.",
            updated_title: "Atualizada",
            updated_body: "Os detalhes da reunião mudaram.",
            cancelled_title: "Cancelada",
            cancelled_body: "A reunião foi cancelada.",
        },
        "ja" => &Copy {
            invited_title: "招待",
            invited_body: "会議に招待されました。",
            reminder_title: "まもなく開始",
            reminder_body: "まもなく会議が始まります。",
            updated_title: "更新",
            updated_body: "会議の詳細が変更されました。",
            cancelled_title: "キャンセル",
            cancelled_body: "会議がキャンセルされました。",
        },
        "zh" => &Copy {
            invited_title: "邀请",
            invited_body: "您被邀请参加会议。",
            reminder_title: "即将开始",
            reminder_body: "您的会议即将开始。",
            updated_title: "已更新",
            updated_body: "会议详情已更改。",
            cancelled_title: "已取消",
            cancelled_body: "会议已取消。",
        },
        _ => &Copy {
            invited_title: "Invitation",
            invited_body: "You've been invited to a meeting.",
            reminder_title: "Starting soon",
            reminder_body: "Your meeting is about to start.",
            updated_title: "Updated",
            updated_body: "The meeting details have changed.",
            cancelled_title: "Cancelled",
            cancelled_body: "The meeting has been cancelled.",
        },
    }
}

/// `(title, body)` for a notification `kind`, localized to `lang`, with `meeting_title`
/// interpolated. Unknown kinds fall back to the meeting title alone with an empty body.
pub fn meeting_copy(kind: &str, lang: &str, meeting_title: &str) -> (String, String) {
    let c = copy(lang);
    let (prefix, body) = match kind {
        "meeting_invited" => (c.invited_title, c.invited_body),
        "meeting_reminder" => (c.reminder_title, c.reminder_body),
        "meeting_updated" => (c.updated_title, c.updated_body),
        "meeting_cancelled" => (c.cancelled_title, c.cancelled_body),
        _ => return (meeting_title.to_string(), String::new()),
    };
    (format!("{prefix}: {meeting_title}"), body.to_string())
}

/// Localized label for the "Join" call-to-action button in notification emails.
pub fn join_label(lang: &str) -> &'static str {
    match lang {
        "it" => "Entra",
        "es" => "Entrar",
        "fr" => "Rejoindre",
        "de" => "Beitreten",
        "pt" => "Entrar",
        "ja" => "参加",
        "zh" => "加入",
        _ => "Join",
    }
}

/// The recipient's stored UI locale, or `"en"` when unknown. Used to pick
/// [`meeting_copy`] / `invite::build_invite_email` language for that user.
pub async fn user_locale(pool: &Pool, user_id: Uuid) -> String {
    sqlx::query_scalar::<_, Option<String>>("SELECT locale FROM users WHERE id = $1")
        .bind(user_id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
        .flatten()
        .unwrap_or_else(|| "en".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn localizes_title_and_body() {
        let (t, b) = meeting_copy("meeting_invited", "it", "Sync settimanale");
        assert_eq!(t, "Invito: Sync settimanale");
        assert_eq!(b, "Sei stato invitato a una riunione.");
    }

    #[test]
    fn unknown_lang_falls_back_to_english() {
        let (t, b) = meeting_copy("meeting_cancelled", "xx", "Demo");
        assert_eq!(t, "Cancelled: Demo");
        assert_eq!(b, "The meeting has been cancelled.");
    }

    #[test]
    fn unknown_kind_yields_bare_title() {
        let (t, b) = meeting_copy("something_else", "it", "Demo");
        assert_eq!(t, "Demo");
        assert!(b.is_empty());
    }

    #[test]
    fn every_kind_localizes_for_each_language() {
        let kinds = [
            "meeting_invited",
            "meeting_reminder",
            "meeting_updated",
            "meeting_cancelled",
        ];
        for lang in ["en", "it", "es", "fr", "de", "pt", "ja", "zh", "xx"] {
            for kind in kinds {
                let (title, body) = meeting_copy(kind, lang, "Standup");
                assert!(
                    title.contains("Standup"),
                    "{lang}/{kind} title missing meeting name"
                );
                assert!(
                    !title.is_empty() && !body.is_empty(),
                    "{lang}/{kind} empty copy"
                );
            }
        }
    }

    #[test]
    fn join_label_localizes_with_english_fallback() {
        assert_eq!(join_label("it"), "Entra");
        assert_eq!(join_label("de"), "Beitreten");
        assert_eq!(join_label("ja"), "参加");
        assert_eq!(join_label("en"), "Join");
        assert_eq!(join_label("xx"), "Join"); // unknown → fallback
    }
}
