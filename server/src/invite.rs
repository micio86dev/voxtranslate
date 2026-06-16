//! In-call invite emails (spec 0082).
//!
//! A participant who knows someone's email can send them a one-tap link to join
//! the call. Privacy contract: we never reveal whether an address belongs to a
//! registered account (no member enumeration), never expose other users'
//! addresses, and send one email *per* recipient so no recipient sees the
//! others. The join link is always built from the server's configured
//! `app_base_url` + the sanitised room code — never a client-supplied URL.

use crate::ai::email_draft::valid_email;
use crate::email_template::{render_button, tagline, EmailButton};

/// Max recipients per invite send. A room seats 4, but a sender may invite a few
/// extra (bounces, declines) — kept low so we can't be used as a spam relay.
pub const MAX_INVITE_EMAILS: usize = 5;

/// Accept a room code only if it's URL- and HTML-safe, so it can be dropped into
/// an email link verbatim. Returns the lowercased code, or `None` to reject.
pub fn sanitize_room(code: &str) -> Option<String> {
    let code = code.trim();
    if code.is_empty() || code.len() > 64 {
        return None;
    }
    if code
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        Some(code.to_ascii_lowercase())
    } else {
        None
    }
}

/// Validate, trim, lowercase and dedupe a list of recipient addresses. Returns a
/// user-facing error string on any problem so the handler can 400 it.
pub fn prepare_recipients(raw: &[String]) -> Result<Vec<String>, String> {
    if raw.is_empty() {
        return Err("add at least one email address".into());
    }
    if raw.len() > MAX_INVITE_EMAILS {
        return Err(format!(
            "too many addresses (max {MAX_INVITE_EMAILS} per invite)"
        ));
    }
    let mut out: Vec<String> = Vec::new();
    for r in raw {
        let e = r.trim().to_lowercase();
        if !valid_email(&e) {
            return Err(format!("invalid email address `{}`", r.trim()));
        }
        if !out.contains(&e) {
            out.push(e);
        }
    }
    Ok(out)
}

/// The localised, fully-rendered pieces of one invite email.
pub struct InviteEmail {
    pub subject: String,
    pub preheader: String,
    pub heading: String,
    pub body_html: String,
    pub body_text: String,
    pub tagline: String,
}

/// Localised copy for the invite, by UI language (falls back to English).
struct Copy {
    /// `{inviter}` placeholder.
    subject: &'static str,
    preheader: &'static str,
    heading: &'static str,
    /// `{inviter}` placeholder.
    intro: &'static str,
    instruction: &'static str,
    button: &'static str,
    link_intro: &'static str,
}

fn copy(lang: &str) -> &'static Copy {
    match lang {
        "it" => &Copy {
            subject: "{inviter} ti invita a una videochiamata su VoxTranslate",
            preheader: "Entra nella chiamata tradotta in tempo reale.",
            heading: "Sei stato invitato a una chiamata",
            intro: "{inviter} vorrebbe che ti unissi a una videochiamata su VoxTranslate. \
                    Ognuno parla la propria lingua: VoxTranslate traduce tutto in tempo reale.",
            instruction: "Tocca il pulsante qui sotto per entrare. Non serve installare nulla.",
            button: "Entra nella chiamata",
            link_intro: "Il pulsante non funziona? Copia e incolla questo link nel browser:",
        },
        "es" => &Copy {
            subject: "{inviter} te invita a una videollamada en VoxTranslate",
            preheader: "Únete a la llamada traducida en tiempo real.",
            heading: "Te han invitado a una llamada",
            intro: "{inviter} quiere que te unas a una videollamada en VoxTranslate. \
                    Cada persona habla su idioma: VoxTranslate lo traduce en tiempo real.",
            instruction: "Toca el botón de abajo para entrar. No necesitas instalar nada.",
            button: "Entrar a la llamada",
            link_intro: "¿El botón no funciona? Copia y pega este enlace en tu navegador:",
        },
        "fr" => &Copy {
            subject: "{inviter} vous invite à un appel vidéo sur VoxTranslate",
            preheader: "Rejoignez l'appel traduit en temps réel.",
            heading: "Vous êtes invité à un appel",
            intro: "{inviter} aimerait que vous rejoigniez un appel vidéo sur VoxTranslate. \
                    Chacun parle sa langue : VoxTranslate traduit en temps réel.",
            instruction:
                "Touchez le bouton ci-dessous pour rejoindre. Aucune installation requise.",
            button: "Rejoindre l'appel",
            link_intro: "Le bouton ne marche pas ? Copiez ce lien dans votre navigateur :",
        },
        "de" => &Copy {
            subject: "{inviter} lädt dich zu einem VoxTranslate-Videoanruf ein",
            preheader: "Tritt dem in Echtzeit übersetzten Anruf bei.",
            heading: "Du wurdest zu einem Anruf eingeladen",
            intro: "{inviter} möchte, dass du einem Videoanruf auf VoxTranslate beitrittst. \
                    Jeder spricht seine Sprache – VoxTranslate übersetzt in Echtzeit.",
            instruction:
                "Tippe unten auf die Schaltfläche, um beizutreten. Keine Installation nötig.",
            button: "Anruf beitreten",
            link_intro: "Schaltfläche geht nicht? Kopiere diesen Link in deinen Browser:",
        },
        "pt" => &Copy {
            subject: "{inviter} convida você para uma videochamada no VoxTranslate",
            preheader: "Entre na chamada traduzida em tempo real.",
            heading: "Você foi convidado para uma chamada",
            intro: "{inviter} gostaria que você entrasse em uma videochamada no VoxTranslate. \
                    Cada um fala seu idioma — o VoxTranslate traduz em tempo real.",
            instruction: "Toque no botão abaixo para entrar. Não é preciso instalar nada.",
            button: "Entrar na chamada",
            link_intro: "O botão não funciona? Copie e cole este link no navegador:",
        },
        "ja" => &Copy {
            subject: "{inviter} さんが VoxTranslate のビデオ通話に招待しています",
            preheader: "リアルタイム翻訳の通話に参加しましょう。",
            heading: "通話に招待されました",
            intro: "{inviter} さんが VoxTranslate のビデオ通話への参加を希望しています。\
                    それぞれが自分の言語で話し、VoxTranslate がリアルタイムで翻訳します。",
            instruction: "下のボタンを押すと参加できます。インストールは不要です。",
            button: "通話に参加",
            link_intro: "ボタンが動かない場合は、このリンクをブラウザに貼り付けてください：",
        },
        "zh" => &Copy {
            subject: "{inviter} 邀请你加入 VoxTranslate 视频通话",
            preheader: "加入实时翻译的通话。",
            heading: "你被邀请加入通话",
            intro: "{inviter} 想邀请你加入 VoxTranslate 视频通话。\
                    每个人都说自己的语言，VoxTranslate 实时翻译。",
            instruction: "点击下方按钮即可加入，无需安装任何软件。",
            button: "加入通话",
            link_intro: "按钮无法使用？请把以下链接复制到浏览器：",
        },
        _ => &Copy {
            subject: "{inviter} invited you to a VoxTranslate call",
            preheader: "Join the real-time translated call.",
            heading: "You're invited to a call",
            intro: "{inviter} would like you to join a video call on VoxTranslate. \
                    Everyone speaks their own language — VoxTranslate translates in real time.",
            instruction: "Tap the button below to join. Nothing to install.",
            button: "Join the call",
            link_intro: "Button not working? Copy and paste this link into your browser:",
        },
    }
}

fn esc_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn esc_attr(s: &str) -> String {
    esc_text(s).replace('"', "&quot;")
}

/// Build the full localised invite email for one recipient. `inviter` is the
/// sender's display name; `join_url` is the server-built join link.
pub fn build_invite_email(lang: &str, inviter: &str, join_url: &str) -> InviteEmail {
    let c = copy(lang);
    let inviter = inviter.trim();
    let inviter = if inviter.is_empty() {
        "Someone"
    } else {
        inviter
    };

    let subject = c.subject.replace("{inviter}", inviter);
    let intro = c.intro.replace("{inviter}", inviter);

    let button = render_button(&EmailButton {
        label: c.button,
        url: join_url,
    });

    let body_html = format!(
        "<p style=\"margin:0 0 16px;\">{intro}</p>\
         <p style=\"margin:0 0 4px;\">{instruction}</p>\
         {button}\
         <p style=\"margin:18px 0 0;font-size:14px;line-height:1.5;color:#64748b;\">\
         {link_intro}<br/>\
         <a href=\"{href}\" target=\"_blank\" \
         style=\"color:#0871ab;text-decoration:underline;word-break:break-all;\">{shown}</a></p>",
        intro = esc_text(&intro),
        instruction = esc_text(c.instruction),
        button = button,
        link_intro = esc_text(c.link_intro),
        href = esc_attr(join_url),
        shown = esc_text(join_url),
    );

    let body_text = format!(
        "{intro}\n\n{instruction}\n\n{button_label}:\n{url}\n\n{link_intro}\n{url}",
        intro = intro,
        instruction = c.instruction,
        button_label = c.button,
        url = join_url,
        link_intro = c.link_intro,
    );

    InviteEmail {
        subject,
        preheader: c.preheader.to_string(),
        heading: c.heading.to_string(),
        body_html,
        body_text,
        tagline: tagline(lang).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_room_accepts_safe_rejects_unsafe() {
        assert_eq!(sanitize_room("Blue-Fox_42").as_deref(), Some("blue-fox_42"));
        assert_eq!(sanitize_room("  abc  ").as_deref(), Some("abc"));
        assert!(sanitize_room("").is_none());
        assert!(sanitize_room("bad room").is_none()); // space
        assert!(sanitize_room("a/b").is_none()); // path
        assert!(sanitize_room("a\"b").is_none()); // quote
        assert!(sanitize_room("x?y=1").is_none()); // query injection
        assert!(sanitize_room(&"a".repeat(65)).is_none()); // too long
    }

    #[test]
    fn prepare_recipients_validates_dedupes_caps() {
        let ok = prepare_recipients(&[
            "  Alice@Example.com ".into(),
            "alice@example.com".into(), // dup after normalise
            "bob@test.io".into(),
        ])
        .unwrap();
        assert_eq!(ok, vec!["alice@example.com", "bob@test.io"]);

        assert!(prepare_recipients(&[])
            .unwrap_err()
            .contains("at least one"));
        assert!(prepare_recipients(&["nope".into()])
            .unwrap_err()
            .contains("invalid email"));
        let many: Vec<String> = (0..=MAX_INVITE_EMAILS)
            .map(|i| format!("u{i}@x.co"))
            .collect();
        assert!(prepare_recipients(&many).unwrap_err().contains("too many"));
    }

    #[test]
    fn build_invite_email_localises_and_embeds_link() {
        let it = build_invite_email("it", "Anna", "https://voxtranslate.app/?room=blue-fox");
        assert!(it.subject.starts_with("Anna ti invita"));
        assert!(it.body_html.contains("Anna vorrebbe"));
        assert!(it.body_html.contains("Entra nella chiamata")); // button label
        assert!(it
            .body_html
            .contains("href=\"https://voxtranslate.app/?room=blue-fox\""));
        assert!(it
            .body_text
            .contains("https://voxtranslate.app/?room=blue-fox"));
        assert_eq!(it.tagline, "Videochiamate tradotte in tempo reale");

        // Unknown language → English fallback.
        let en = build_invite_email("xx", "Bob", "https://voxtranslate.app/?room=x");
        assert!(en.subject.contains("invited you to a VoxTranslate call"));
        assert!(en.heading.contains("You're invited"));
    }

    #[test]
    fn build_invite_email_escapes_inviter_name() {
        let e = build_invite_email("en", "<b>Eve</b>&", "https://voxtranslate.app/?room=x");
        assert!(
            !e.body_html.contains("<b>Eve</b>"),
            "name escaped: {}",
            e.body_html
        );
        assert!(e.body_html.contains("&lt;b&gt;Eve"));
        // Subject is plain text (not HTML) so it keeps the raw chars.
        assert!(e.subject.contains("<b>Eve</b>&"));
    }

    #[test]
    fn empty_inviter_falls_back() {
        let e = build_invite_email("en", "   ", "https://voxtranslate.app/?room=x");
        assert!(e.subject.starts_with("Someone invited you"));
    }
}
