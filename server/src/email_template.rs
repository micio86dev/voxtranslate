//! Branded, Outlook-safe HTML email shell (spec 0082).
//!
//! Every transactional email VoxTranslate sends — call invites, follow-up
//! recaps, bug-report notifications — is wrapped here so the brand (logo +
//! `VoxTranslate` wordmark), layout, and responsive behaviour stay consistent.
//!
//! Rendering constraints, learned the hard way from email clients:
//! - **Tables, not flexbox/grid.** Outlook (Word engine) ignores modern CSS.
//! - **Inline styles only**; `<style>` media queries are a progressive
//!   enhancement that mobile webmail honours and Outlook ignores.
//! - **MSO conditionals** give Outlook a fixed 600px width and a VML
//!   "bulletproof" button (CSS padding buttons collapse in Outlook).
//! - **Wordmark as live text** next to the logo image, so the brand still
//!   reads when a client blocks images by default (Outlook does).
//! - A hidden **preheader** controls the inbox preview snippet.
//! - **Logo is a bare opaque image**, not an image inside a white CSS chip.
//!   Gmail's mobile app dark mode rewrites light CSS backgrounds (`#ffffff`)
//!   to black but never recolours image pixels, so a white `<td>` behind the
//!   logo turned black on Android/iOS Gmail (the `color-scheme:light` meta is
//!   ignored there). `icon.png` is already opaque with its own light backdrop,
//!   so it renders identically in light and dark — don't reintroduce a CSS chip.

/// Primary call-to-action button, rendered bulletproof (VML for Outlook).
pub struct EmailButton<'a> {
    pub label: &'a str,
    pub url: &'a str,
}

/// Everything the shell needs to render one email.
pub struct EmailLayout<'a> {
    /// App origin for the logo image and footer link, e.g. `https://voxtranslate.app`.
    pub app_base_url: &'a str,
    /// Inbox-preview text — hidden in the body, shown by the mail client next to
    /// the subject. Keep it short and meaningful.
    pub preheader: &'a str,
    /// Optional bold heading at the top of the card.
    pub heading: Option<&'a str>,
    /// Inner body HTML — trusted: model output uses a restricted tag set and our
    /// own callers escape any user-supplied text before it reaches here.
    pub body_html: &'a str,
    /// Optional primary button under the body.
    pub button: Option<EmailButton<'a>>,
    /// Footer tagline under the wordmark (localised by the caller).
    pub tagline: &'a str,
}

const BRAND: &str = "#0871ab"; // header bar + wordmark + button
const PAGE_BG: &str = "#eef2f7"; // light page backdrop
const CARD_BG: &str = "#ffffff";
const TEXT: &str = "#1f2933";
const MUTED: &str = "#64748b";
const FONT: &str = "-apple-system,BlinkMacSystemFont,'Segoe UI',Roboto,Helvetica,Arial,sans-serif";

/// Escape a string for use inside an HTML attribute value (e.g. `href`).
fn attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Escape a string for HTML text content.
fn text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// Brand tagline for the email footer, localised by UI language (English
/// fallback). Shared by every email so the footer reads in the recipient's
/// language where we know it.
pub fn tagline(lang: &str) -> &'static str {
    match lang {
        "it" => "Videochiamate tradotte in tempo reale",
        "es" => "Videollamadas traducidas en tiempo real",
        "fr" => "Appels vidéo traduits en temps réel",
        "de" => "In Echtzeit übersetzte Videoanrufe",
        "pt" => "Videochamadas traduzidas em tempo real",
        "ja" => "リアルタイム翻訳のビデオ通話",
        "zh" => "实时翻译视频通话",
        _ => "Real-time translated video calls",
    }
}

/// Render a single bulletproof CTA button (VML rect for Outlook, padded anchor
/// elsewhere). Exposed so callers that need a link *after* the button — e.g. the
/// invite email's "or paste this link" fallback — can place it inside `body_html`.
pub fn render_button(b: &EmailButton) -> String {
    let href = attr(b.url);
    let label = text(b.label);
    format!(
        "<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" \
         style=\"margin:28px 0 8px;\"><tr><td align=\"center\" bgcolor=\"{BRAND}\" \
         style=\"border-radius:10px;\">\
         <!--[if mso]>\
         <v:roundrect xmlns:v=\"urn:schemas-microsoft-com:vml\" \
         xmlns:w=\"urn:schemas-microsoft-com:office:word\" href=\"{href}\" \
         style=\"height:48px;v-text-anchor:middle;width:300px;\" arcsize=\"21%\" \
         stroke=\"f\" fillcolor=\"{BRAND}\">\
         <w:anchorlock/><center style=\"color:#ffffff;font-family:{FONT};\
         font-size:16px;font-weight:700;\">{label}</center></v:roundrect>\
         <![endif]-->\
         <!--[if !mso]><!-- -->\
         <a href=\"{href}\" target=\"_blank\" \
         style=\"display:inline-block;padding:14px 36px;font-family:{FONT};\
         font-size:16px;font-weight:700;color:#ffffff;text-decoration:none;\
         border-radius:10px;background:{BRAND};\">{label}</a>\
         <!--<![endif]-->\
         </td></tr></table>"
    )
}

/// Render the full branded HTML document for one email.
pub fn render_html(layout: &EmailLayout) -> String {
    let base = layout.app_base_url.trim_end_matches('/');
    let logo = format!("{base}/icon.png");

    let heading_block = match layout.heading {
        Some(h) if !h.trim().is_empty() => format!(
            "<h1 style=\"margin:0 0 16px;font-size:22px;line-height:1.3;font-weight:700;\
             color:{TEXT};\">{}</h1>",
            text(h)
        ),
        _ => String::new(),
    };

    let button_block = layout
        .button
        .as_ref()
        .map(render_button)
        .unwrap_or_default();

    // Hidden preheader: zero-height, plus a run of zero-width spaces so the
    // client doesn't pull body text into the snippet after it.
    let preheader = format!(
        "<div style=\"display:none;max-height:0;overflow:hidden;mso-hide:all;\
         font-size:1px;line-height:1px;color:{PAGE_BG};opacity:0;\">{}{}</div>",
        text(layout.preheader),
        "&#8203;&#847;&zwnj;&nbsp;".repeat(20)
    );

    format!(
        "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Transitional//EN\" \
         \"http://www.w3.org/TR/xhtml1/DTD/xhtml1-transitional.dtd\">\
<html xmlns=\"http://www.w3.org/1999/xhtml\" lang=\"en\">\
<head>\
<meta charset=\"utf-8\"/>\
<meta name=\"viewport\" content=\"width=device-width,initial-scale=1\"/>\
<meta http-equiv=\"X-UA-Compatible\" content=\"IE=edge\"/>\
<meta name=\"color-scheme\" content=\"light\"/>\
<meta name=\"supported-color-schemes\" content=\"light\"/>\
<title>VoxTranslate</title>\
<!--[if mso]><noscript><xml><o:OfficeDocumentSettings>\
<o:PixelsPerInch>96</o:PixelsPerInch></o:OfficeDocumentSettings></xml></noscript><![endif]-->\
<style>\
@media only screen and (max-width:600px){{\
.vox-card{{width:100% !important;border-radius:0 !important;}}\
.vox-pad{{padding-left:24px !important;padding-right:24px !important;}}\
}}\
a{{color:{BRAND};}}\
</style>\
</head>\
<body style=\"margin:0;padding:0;background:{PAGE_BG};\">\
{preheader}\
<table role=\"presentation\" width=\"100%\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\" \
 style=\"background:{PAGE_BG};\"><tr><td align=\"center\" style=\"padding:24px 12px;\">\
<!--[if mso]><table role=\"presentation\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" \
 border=\"0\"><tr><td><![endif]-->\
<table role=\"presentation\" class=\"vox-card\" width=\"600\" cellpadding=\"0\" cellspacing=\"0\" \
 border=\"0\" style=\"width:600px;max-width:600px;background:{CARD_BG};border-radius:16px;\
 overflow:hidden;box-shadow:0 1px 4px rgba(15,23,42,0.08);\">\
<tr><td style=\"background:{BRAND};padding:22px 32px;\">\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
<td style=\"vertical-align:middle;padding-right:12px;line-height:0;\">\
<table role=\"presentation\" cellpadding=\"0\" cellspacing=\"0\" border=\"0\"><tr>\
<td bgcolor=\"#ffffff\" style=\"background:#ffffff;border-radius:12px;padding:5px;line-height:0;\">\
<img src=\"{logo}\" width=\"40\" height=\"40\" alt=\"VoxTranslate\" \
 style=\"display:block;border-radius:8px;background:#ffffff;\"/></td>\
</tr></table></td>\
<td style=\"vertical-align:middle;font-family:{FONT};font-size:21px;font-weight:700;\
 color:#ffffff;letter-spacing:0.2px;\">VoxTranslate</td>\
</tr></table></td></tr>\
<tr><td class=\"vox-pad\" style=\"padding:34px 40px 12px;font-family:{FONT};font-size:16px;\
 line-height:1.6;color:{TEXT};\">\
{heading_block}\
<div style=\"font-family:{FONT};font-size:16px;line-height:1.6;color:{TEXT};\">{body}</div>\
{button_block}\
</td></tr>\
<tr><td class=\"vox-pad\" style=\"padding:24px 40px 34px;\">\
<hr style=\"border:none;border-top:1px solid #e5e9f0;margin:0 0 18px;\"/>\
<p style=\"margin:0 0 4px;font-family:{FONT};font-size:14px;font-weight:700;color:{TEXT};\">\
VoxTranslate</p>\
<p style=\"margin:0 0 10px;font-family:{FONT};font-size:13px;line-height:1.5;color:{MUTED};\">\
{tagline}</p>\
<p style=\"margin:0;font-family:{FONT};font-size:13px;color:{MUTED};\">\
<a href=\"{base}\" target=\"_blank\" style=\"color:{BRAND};text-decoration:none;\">{base_label}</a>\
</p></td></tr>\
</table>\
<!--[if mso]></td></tr></table><![endif]-->\
</td></tr></table>\
</body></html>",
        base = attr(base),
        base_label = text(
            base.trim_start_matches("https://")
                .trim_start_matches("http://")
        ),
        body = layout.body_html,
        tagline = text(layout.tagline),
    )
}

/// Plain-text counterpart: the body text, the button as a labelled link, then a
/// short branded footer. Mirrors `render_html` so multipart emails stay in sync.
pub fn render_text(
    body_text: &str,
    button: Option<&EmailButton>,
    app_base_url: &str,
    tagline: &str,
) -> String {
    let base = app_base_url.trim_end_matches('/');
    let mut out = body_text.trim_end().to_string();
    if let Some(b) = button {
        out.push_str(&format!("\n\n{}:\n{}", b.label, b.url));
    }
    out.push_str(&format!("\n\n—\nVoxTranslate\n{tagline}\n{base}\n"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout<'a>(button: Option<EmailButton<'a>>) -> EmailLayout<'a> {
        EmailLayout {
            app_base_url: "https://voxtranslate.app/",
            preheader: "You're invited to a call",
            heading: Some("Join the call"),
            body_html: "<p>Anna invited you.</p>",
            button,
            tagline: "Real-time translated video calls",
        }
    }

    #[test]
    fn html_has_brand_wordmark_logo_and_preheader() {
        let html = render_html(&layout(None));
        // Live-text wordmark survives image blocking.
        assert!(html.contains(">VoxTranslate<"), "wordmark text present");
        // Logo image points at the app origin, trailing slash trimmed.
        assert!(html.contains("src=\"https://voxtranslate.app/icon.png\""));
        assert!(!html.contains("app//icon.png"), "double slash trimmed");
        // Preheader text is present and hidden.
        assert!(html.contains("You're invited to a call"));
        assert!(html.contains("mso-hide:all"));
        // Heading rendered.
        assert!(html.contains("Join the call"));
        // Table-based, Outlook-aware.
        assert!(html.contains("role=\"presentation\""));
        assert!(html.contains("[if mso]"));
    }

    #[test]
    fn html_button_is_bulletproof_when_present() {
        let html = render_html(&layout(Some(EmailButton {
            label: "Enter the call",
            url: "https://voxtranslate.app/?room=abc",
        })));
        assert!(html.contains("Enter the call"));
        assert!(html.contains("v:roundrect"), "VML button for Outlook");
        assert!(html.contains("href=\"https://voxtranslate.app/?room=abc\""));
        // No button artefacts when absent.
        let plain = render_html(&layout(None));
        assert!(!plain.contains("v:roundrect"));
    }

    #[test]
    fn attribute_values_are_escaped() {
        let html = render_html(&EmailLayout {
            button: Some(EmailButton {
                label: "Go",
                url: "https://x.test/?a=1&b=2\"><script>",
            }),
            ..layout(None)
        });
        assert!(!html.contains("<script>"), "url escaped in href: {html}");
        assert!(html.contains("&amp;b=2"));
    }

    #[test]
    fn text_includes_button_link_and_footer() {
        let t = render_text(
            "Hello there.",
            Some(&EmailButton {
                label: "Enter the call",
                url: "https://voxtranslate.app/?room=abc",
            }),
            "https://voxtranslate.app/",
            "Real-time translated video calls",
        );
        assert!(t.contains("Hello there."));
        assert!(t.contains("Enter the call:\nhttps://voxtranslate.app/?room=abc"));
        assert!(t.contains("VoxTranslate"));
        assert!(t.trim_end().ends_with("https://voxtranslate.app"));
    }
}
