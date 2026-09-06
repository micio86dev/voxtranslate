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

/// `(title, body)` for the friend / call-invite notifications (Phase 2), localized to
/// `lang`, with the other person's display name `actor` interpolated where `{n}`
/// appears. Same language bar as [`meeting_copy`]; English fallback for the rest.
pub fn friend_copy(kind: &str, lang: &str, actor: &str) -> (String, String) {
    let (title, body): (&str, &str) = match kind {
        "friend_request" => match lang {
            "it" => (
                "Richiesta di amicizia",
                "{n} ti ha inviato una richiesta di amicizia.",
            ),
            "es" => (
                "Solicitud de amistad",
                "{n} te ha enviado una solicitud de amistad.",
            ),
            "fr" => ("Demande d'ami", "{n} vous a envoyé une demande d'ami."),
            "de" => (
                "Freundschaftsanfrage",
                "{n} hat dir eine Freundschaftsanfrage gesendet.",
            ),
            "pt" => ("Pedido de amizade", "{n} enviou-te um pedido de amizade."),
            "ja" => ("友達リクエスト", "{n}さんから友達リクエストが届きました。"),
            "zh" => ("好友请求", "{n} 向你发送了好友请求。"),
            _ => ("Friend request", "{n} sent you a friend request."),
        },
        "friend_accepted" => match lang {
            "it" => (
                "Amicizia accettata",
                "{n} ha accettato la tua richiesta di amicizia.",
            ),
            "es" => ("Amistad aceptada", "{n} aceptó tu solicitud de amistad."),
            "fr" => ("Demande acceptée", "{n} a accepté votre demande d'ami."),
            "de" => (
                "Anfrage angenommen",
                "{n} hat deine Freundschaftsanfrage angenommen.",
            ),
            "pt" => ("Amizade aceite", "{n} aceitou o teu pedido de amizade."),
            "ja" => ("リクエスト承認", "{n}さんが友達リクエストを承認しました。"),
            "zh" => ("请求已接受", "{n} 接受了你的好友请求。"),
            _ => (
                "Friend request accepted",
                "{n} accepted your friend request.",
            ),
        },
        "call_invite" => match lang {
            "it" => ("Invito alla chiamata", "{n} ti ha invitato a una chiamata."),
            "es" => (
                "Invitación a una llamada",
                "{n} te ha invitado a una llamada.",
            ),
            "fr" => ("Invitation à un appel", "{n} vous a invité à un appel."),
            "de" => ("Anrufeinladung", "{n} hat dich zu einem Anruf eingeladen."),
            "pt" => ("Convite para chamada", "{n} convidou-te para uma chamada."),
            "ja" => ("通話への招待", "{n}さんが通話に招待しました。"),
            "zh" => ("通话邀请", "{n} 邀请你加入通话。"),
            _ => ("Call invite", "{n} invited you to a call."),
        },
        "friend_active" => match lang {
            "it" => (
                "Un amico è online",
                "{n} è in una stanza pubblica. Va di fare due chiacchiere?",
            ),
            "es" => (
                "Un amigo está en línea",
                "{n} está en una sala pública. ¿Te apetece charlar?",
            ),
            "fr" => (
                "Un ami est en ligne",
                "{n} est dans un salon public. Envie de discuter ?",
            ),
            "de" => (
                "Ein Freund ist online",
                "{n} ist in einem öffentlichen Raum. Lust auf einen Plausch?",
            ),
            "pt" => (
                "Um amigo está online",
                "{n} está numa sala pública. Que tal conversar?",
            ),
            "ja" => (
                "友達がオンラインです",
                "{n}さんが公開ルームにいます。少しおしゃべりしませんか？",
            ),
            "zh" => ("好友在线", "{n} 正在一个公开房间里。想聊聊吗？"),
            _ => (
                "A friend is online",
                "{n} is in a public room. Fancy a chat?",
            ),
        },
        _ => ("Notification", "{n}"),
    };
    (title.to_string(), body.replace("{n}", actor))
}

/// `(title, body)` for a webinar friend-alert notification, localized to `lang`,
/// with the host's display name interpolated where `{n}` appears. Kinds:
/// `webinar_soon` (a scheduled public webinar is about to start) and `webinar_live`
/// (an unscheduled public webinar just went live). Same language bar as
/// [`friend_copy`]; English fallback for the rest.
pub fn webinar_copy(kind: &str, lang: &str, host: &str) -> (String, String) {
    let (title, body): (&str, &str) = match kind {
        "webinar_soon" => match lang {
            "it" => ("Webinar tra poco", "Il webinar di {n} sta per iniziare."),
            "es" => (
                "El webinar empieza pronto",
                "El webinar de {n} está a punto de empezar.",
            ),
            "fr" => (
                "Webinaire bientôt",
                "Le webinaire de {n} va bientôt commencer.",
            ),
            "de" => (
                "Webinar beginnt bald",
                "Das Webinar von {n} beginnt gleich.",
            ),
            "pt" => (
                "Webinar em breve",
                "O webinar de {n} está prestes a começar.",
            ),
            "ja" => (
                "まもなくウェビナー開始",
                "{n}さんのウェビナーがまもなく始まります。",
            ),
            "zh" => ("网络研讨会即将开始", "{n} 的网络研讨会即将开始。"),
            _ => ("Webinar starting soon", "{n}'s webinar is about to start."),
        },
        "webinar_live" => match lang {
            "it" => ("Webinar iniziato", "{n} è in diretta ora."),
            "es" => ("Webinar en directo", "{n} está en directo ahora."),
            "fr" => ("Webinaire en direct", "{n} est en direct maintenant."),
            "de" => ("Webinar läuft", "{n} ist jetzt live."),
            "pt" => ("Webinar ao vivo", "{n} está ao vivo agora."),
            "ja" => ("ウェビナー開始", "{n}さんが今ライブ配信中です。"),
            "zh" => ("网络研讨会已开始", "{n} 正在直播。"),
            _ => ("Webinar live now", "{n} is live now."),
        },
        _ => ("Notification", "{n}"),
    };
    (title.to_string(), body.replace("{n}", host))
}

/// `(title, body)` for the expiring-subscription warning, localized to `lang`,
/// with the number of days left interpolated where `{n}` appears.
///
/// Same language bar as [`webinar_copy`]; English fallback for the rest. The
/// body deliberately names the consequence rather than the plan: what the
/// recipient needs to know is that features stop, and features stopping without
/// warning is exactly what this exists to prevent.
pub fn subscription_copy(lang: &str, days_left: i64) -> (String, String) {
    let (title, body): (&str, &str) = match lang {
        "it" => (
            "Abbonamento in scadenza",
            "Il tuo abbonamento scade tra {n} giorni. Rinnovalo per non perdere registrazioni, trascrizioni e assistente vocale.",
        ),
        "es" => (
            "Tu suscripción caduca pronto",
            "Tu suscripción caduca en {n} días. Renuévala para no perder grabaciones, transcripciones y el asistente de voz.",
        ),
        "fr" => (
            "Abonnement bientôt expiré",
            "Votre abonnement expire dans {n} jours. Renouvelez-le pour conserver enregistrements, transcriptions et assistant vocal.",
        ),
        "de" => (
            "Abonnement läuft bald ab",
            "Ihr Abonnement läuft in {n} Tagen ab. Verlängern Sie es, um Aufzeichnungen, Transkripte und Sprachassistent zu behalten.",
        ),
        "pt" => (
            "Assinatura a expirar",
            "A sua assinatura expira em {n} dias. Renove-a para não perder gravações, transcrições e assistente de voz.",
        ),
        "ja" => (
            "サブスクリプションの有効期限が近づいています",
            "サブスクリプションはあと{n}日で終了します。録画・文字起こし・音声アシスタントを継続するには更新してください。",
        ),
        "zh" => (
            "订阅即将到期",
            "您的订阅将在 {n} 天后到期。请续订以继续使用录制、转写和语音助手。",
        ),
        _ => (
            "Your subscription is about to expire",
            "Your subscription ends in {n} days. Renew it to keep recordings, transcripts and the voice assistant.",
        ),
    };
    (
        title.to_string(),
        body.replace("{n}", &days_left.to_string()),
    )
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
    fn subscription_warning_names_the_days_and_falls_back_to_english() {
        let (t, b) = subscription_copy("it", 7);
        assert_eq!(t, "Abbonamento in scadenza");
        assert!(b.starts_with("Il tuo abbonamento scade tra 7 giorni"));

        // An unshipped language must still say something useful, not a placeholder.
        let (t, b) = subscription_copy("sw", 3);
        assert_eq!(t, "Your subscription is about to expire");
        assert!(b.contains("in 3 days"), "got: {b}");
        assert!(!b.contains("{n}"), "the placeholder must be substituted");
    }

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

    #[test]
    fn friend_copy_localizes_every_kind_and_language() {
        // Every friend/call notification kind, in every supported UI language, must
        // yield a non-empty title and a body with the actor's name substituted in.
        let kinds = [
            "friend_request",
            "friend_accepted",
            "call_invite",
            "friend_active",
        ];
        for lang in ["en", "it", "es", "fr", "de", "pt", "ja", "zh", "xx"] {
            for kind in kinds {
                let (title, body) = friend_copy(kind, lang, "Alice");
                assert!(!title.is_empty(), "{lang}/{kind} empty title");
                assert!(
                    body.contains("Alice"),
                    "{lang}/{kind} body missing actor name: {body}"
                );
                assert!(!body.contains("{n}"), "{lang}/{kind} left {{n}} unreplaced");
            }
        }
    }

    #[test]
    fn friend_copy_unknown_lang_falls_back_to_english() {
        let (title, body) = friend_copy("friend_request", "xx", "Bob");
        assert_eq!(title, "Friend request");
        assert_eq!(body, "Bob sent you a friend request.");
    }

    #[test]
    fn webinar_copy_localizes_every_kind_and_language() {
        // Every webinar friend-alert kind, in every supported UI language, must yield a
        // non-empty title and a body with the host's name substituted in.
        let kinds = ["webinar_soon", "webinar_live"];
        for lang in ["en", "it", "es", "fr", "de", "pt", "ja", "zh", "xx"] {
            for kind in kinds {
                let (title, body) = webinar_copy(kind, lang, "Alice");
                assert!(!title.is_empty(), "{lang}/{kind} empty title");
                assert!(
                    body.contains("Alice"),
                    "{lang}/{kind} body missing host name: {body}"
                );
                assert!(!body.contains("{n}"), "{lang}/{kind} left {{n}} unreplaced");
            }
        }
    }

    #[test]
    fn webinar_copy_unknown_lang_falls_back_to_english() {
        let (title, body) = webinar_copy("webinar_soon", "xx", "Bob");
        assert_eq!(title, "Webinar starting soon");
        assert_eq!(body, "Bob's webinar is about to start.");
    }

    #[test]
    fn friend_copy_unknown_kind_is_a_bare_notification() {
        // Anything outside the known kinds collapses to a generic title with the
        // actor as the whole body (the `{n}` template).
        let (title, body) = friend_copy("mystery", "it", "Carla");
        assert_eq!(title, "Notification");
        assert_eq!(body, "Carla");
    }
}
