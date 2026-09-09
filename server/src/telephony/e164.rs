//! E.164 destination numbers (spec 0111, R2/R3/R23).
//!
//! Three jobs, and they are deliberately in one place because getting any of them
//! slightly different from the others is how a call gets billed to the wrong country:
//!
//! 1. **Normalise** whatever a human typed into a canonical `+<digits>` string.
//! 2. **Resolve the country**, so allow/block lists and the China gate have something
//!    unambiguous to decide on.
//! 3. **Pseudonymise**, so a telephone number never reaches a log, a metric label or an
//!    analytics row. `docs/gdpr-readiness-2026-09.md` is explicit that MD5 over a
//!    low-entropy value is *not* defensible pseudonymisation — a phone number is
//!    low-entropy (a national number space is brute-forceable in seconds), so this uses a
//!    keyed HMAC and the key never leaves the server.
//!
//! The full number IS persisted in `voip_calls`: a sales rep has to be able to see who
//! they called, and the provider CDR is keyed on it. R23 is about *logs*, not about the
//! record the customer paid for.

use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;

/// Why a destination was refused. Maps 1:1 onto a stable `failure_reason` so the dashboard
/// can translate it without parsing prose.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum E164Error {
    /// Nothing left after stripping punctuation.
    Empty,
    /// A character that is not a digit, and not punctuation we agreed to ignore.
    NonNumeric,
    /// Fewer than 8 digits total. The shortest real E.164 numbers (e.g. `+676…` Tonga,
    /// `+298…` Faroe Islands) are 8; anything shorter is a short code or a typo, and a
    /// short code is not dialable internationally.
    TooShort,
    /// More than 15 digits — E.164's hard maximum.
    TooLong,
    /// Leading digit 0: no country calling code starts with 0.
    LeadingZero,
    /// The leading digits match no assigned country calling code.
    UnknownCountryCode,
}

impl E164Error {
    /// Stable machine-readable reason, persisted and sent to clients.
    pub fn code(self) -> &'static str {
        match self {
            Self::Empty => "number_empty",
            Self::NonNumeric => "number_non_numeric",
            Self::TooShort => "number_too_short",
            Self::TooLong => "number_too_long",
            Self::LeadingZero => "number_leading_zero",
            Self::UnknownCountryCode => "number_unknown_country",
        }
    }
}

/// A validated E.164 number in canonical form: `+` followed by 8–15 digits.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct E164(String);

impl E164 {
    /// Normalise and validate human input.
    ///
    /// Accepts spaces, dashes, dots, parentheses and a non-breaking space (people paste
    /// from Word), a leading `+`, or a leading international access prefix `00` / `011`.
    ///
    /// `00` is only treated as an access prefix when what follows still parses as a
    /// country code — otherwise a number that legitimately begins `00` would be silently
    /// mangled. There is no such assigned code today, but the check costs nothing and the
    /// alternative is a wrong country on a paid call.
    pub fn parse(raw: &str) -> Result<Self, E164Error> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            return Err(E164Error::Empty);
        }

        // Strip the punctuation humans use, and reject anything else outright rather than
        // silently dropping it: a letter in a phone number means the input was not a
        // phone number, and guessing is how you dial a premium-rate line by accident.
        let mut digits = String::with_capacity(trimmed.len());
        for (i, ch) in trimmed.chars().enumerate() {
            match ch {
                '0'..='9' => digits.push(ch),
                '+' if i == 0 => {}
                ' ' | '\u{00a0}' | '-' | '.' | '(' | ')' | '/' => {}
                _ => return Err(E164Error::NonNumeric),
            }
        }
        if digits.is_empty() {
            return Err(E164Error::Empty);
        }

        // International access prefixes. Only stripped when the remainder still resolves,
        // so we never invent a country by chopping real digits off the front.
        let had_plus = trimmed.starts_with('+');
        if !had_plus {
            for prefix in ["011", "00"] {
                if let Some(rest) = digits.strip_prefix(prefix) {
                    if !rest.is_empty()
                        && !rest.starts_with('0')
                        && calling_code_of(rest).is_some()
                        && rest.len() >= MIN_DIGITS
                    {
                        digits = rest.to_string();
                        break;
                    }
                }
            }
        }

        if digits.starts_with('0') {
            return Err(E164Error::LeadingZero);
        }
        if digits.len() < MIN_DIGITS {
            return Err(E164Error::TooShort);
        }
        if digits.len() > MAX_DIGITS {
            return Err(E164Error::TooLong);
        }
        if calling_code_of(&digits).is_none() {
            return Err(E164Error::UnknownCountryCode);
        }

        Ok(Self(format!("+{digits}")))
    }

    /// Canonical `+<digits>` form. This is what goes to the provider and into the DB.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Digits without the leading `+` — the form rate-deck prefixes are matched against.
    pub fn digits(&self) -> &str {
        &self.0[1..]
    }

    /// The assigned country calling code, without `+` (e.g. `"86"`, `"1"`, `"39"`).
    pub fn calling_code(&self) -> &'static str {
        // Safe: `parse` refuses anything this cannot resolve.
        calling_code_of(self.digits()).expect("a parsed E164 always has a calling code").0
    }

    /// ISO 3166-1 alpha-2 region, for allow/block lists and reporting.
    ///
    /// `+1` and `+7` are shared calling codes, so the region is refined by the next
    /// digits where the sharing matters (NANP territories, Kazakhstan). Where a code is
    /// shared and we cannot refine it, the primary assignee is returned — documented
    /// rather than pretended away.
    pub fn region(&self) -> &'static str {
        calling_code_of(self.digits())
            .expect("a parsed E164 always has a calling code")
            .1
    }

    /// Last four digits, for a display form that identifies the call without reprinting
    /// the number.
    pub fn last4(&self) -> &str {
        let d = self.digits();
        &d[d.len() - 4..]
    }

    /// Display form for support and diagnostics: country code, mask, last four.
    pub fn masked(&self) -> String {
        let cc = self.calling_code();
        let rest = &self.digits()[cc.len()..];
        let hidden = rest.len().saturating_sub(4);
        format!("+{}{}{}", cc, "•".repeat(hidden), self.last4())
    }

    /// Keyed pseudonym for logs, metrics and analytics. Stable for a given key, so two
    /// calls to the same number correlate, and irreversible without the key.
    ///
    /// Truncated to 16 hex characters: enough that a collision is not a practical concern
    /// at our volumes, short enough to sit in a log line.
    pub fn pseudonym(&self, key: &[u8]) -> String {
        let mut mac = Hmac::<Sha256>::new_from_slice(key)
            .expect("HMAC accepts a key of any length");
        mac.update(self.0.as_bytes());
        let out = mac.finalize().into_bytes();
        hex::encode(&out[..8])
    }
}

impl std::fmt::Display for E164 {
    /// Deliberately the MASKED form, so a stray `{}` in a log line cannot leak a number.
    /// Use [`as_str`](E164::as_str) where the real value is required.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.masked())
    }
}

const MIN_DIGITS: usize = 8;
const MAX_DIGITS: usize = 15;

/// Longest-match lookup of the country calling code, returning `(code, iso_alpha2)`.
fn calling_code_of(digits: &str) -> Option<(&'static str, &'static str)> {
    // NANP (+1) is one calling code shared by 25 jurisdictions, distinguished by the
    // 3-digit area code. Resolving it matters: an org that blocks "everything except the
    // US" must not thereby permit a premium-rate Caribbean destination, which is one of
    // the oldest toll-fraud plays there is.
    if let Some(area) = digits.strip_prefix('1') {
        if area.len() >= 3 {
            let iso = NANP_AREA_CODES
                .iter()
                .find(|(a, _)| *a == &area[..3])
                .map(|(_, iso)| *iso)
                .unwrap_or("US");
            return Some(("1", iso));
        }
        return None;
    }
    // +7 is Russia and Kazakhstan; Kazakh mobile/geographic ranges begin 6 or 7.
    if let Some(rest) = digits.strip_prefix('7') {
        if rest.is_empty() {
            return None;
        }
        let iso = if rest.starts_with('6') || rest.starts_with('7') {
            "KZ"
        } else {
            "RU"
        };
        return Some(("7", iso));
    }

    // Longest match wins: "1" and "7" are handled above, so 3 then 2 is correct here.
    for len in [3usize, 2] {
        if digits.len() > len {
            let head = &digits[..len];
            if let Some((code, iso)) = CALLING_CODES.iter().find(|(c, _)| *c == head) {
                return Some((code, iso));
            }
        }
    }
    None
}

/// Assigned ITU-T E.164 country calling codes → primary ISO 3166-1 alpha-2.
///
/// `1` and `7` are absent on purpose: they are shared codes resolved in
/// [`calling_code_of`]. Where a code is shared between a country and its dependencies
/// (e.g. `44` GB/JE/GG/IM) the primary assignee is listed; refine here if a policy ever
/// needs to separate them.
const CALLING_CODES: &[(&str, &str)] = &[
    ("20", "EG"), ("27", "ZA"), ("30", "GR"), ("31", "NL"), ("32", "BE"), ("33", "FR"),
    ("34", "ES"), ("36", "HU"), ("39", "IT"), ("40", "RO"), ("41", "CH"), ("43", "AT"),
    ("44", "GB"), ("45", "DK"), ("46", "SE"), ("47", "NO"), ("48", "PL"), ("49", "DE"),
    ("51", "PE"), ("52", "MX"), ("53", "CU"), ("54", "AR"), ("55", "BR"), ("56", "CL"),
    ("57", "CO"), ("58", "VE"), ("60", "MY"), ("61", "AU"), ("62", "ID"), ("63", "PH"),
    ("64", "NZ"), ("65", "SG"), ("66", "TH"), ("81", "JP"), ("82", "KR"), ("84", "VN"),
    ("86", "CN"), ("90", "TR"), ("91", "IN"), ("92", "PK"), ("93", "AF"), ("94", "LK"),
    ("95", "MM"), ("98", "IR"),
    ("211", "SS"), ("212", "MA"), ("213", "DZ"), ("216", "TN"), ("218", "LY"),
    ("220", "GM"), ("221", "SN"), ("222", "MR"), ("223", "ML"), ("224", "GN"),
    ("225", "CI"), ("226", "BF"), ("227", "NE"), ("228", "TG"), ("229", "BJ"),
    ("230", "MU"), ("231", "LR"), ("232", "SL"), ("233", "GH"), ("234", "NG"),
    ("235", "TD"), ("236", "CF"), ("237", "CM"), ("238", "CV"), ("239", "ST"),
    ("240", "GQ"), ("241", "GA"), ("242", "CG"), ("243", "CD"), ("244", "AO"),
    ("245", "GW"), ("246", "IO"), ("248", "SC"), ("249", "SD"), ("250", "RW"),
    ("251", "ET"), ("252", "SO"), ("253", "DJ"), ("254", "KE"), ("255", "TZ"),
    ("256", "UG"), ("257", "BI"), ("258", "MZ"), ("260", "ZM"), ("261", "MG"),
    ("262", "RE"), ("263", "ZW"), ("264", "NA"), ("265", "MW"), ("266", "LS"),
    ("267", "BW"), ("268", "SZ"), ("269", "KM"),
    ("290", "SH"), ("291", "ER"), ("297", "AW"), ("298", "FO"), ("299", "GL"),
    ("350", "GI"), ("351", "PT"), ("352", "LU"), ("353", "IE"), ("354", "IS"),
    ("355", "AL"), ("356", "MT"), ("357", "CY"), ("358", "FI"), ("359", "BG"),
    ("370", "LT"), ("371", "LV"), ("372", "EE"), ("373", "MD"), ("374", "AM"),
    ("375", "BY"), ("376", "AD"), ("377", "MC"), ("378", "SM"), ("379", "VA"),
    ("380", "UA"), ("381", "RS"), ("382", "ME"), ("383", "XK"), ("385", "HR"),
    ("386", "SI"), ("387", "BA"), ("389", "MK"),
    ("420", "CZ"), ("421", "SK"), ("423", "LI"),
    ("500", "FK"), ("501", "BZ"), ("502", "GT"), ("503", "SV"), ("504", "HN"),
    ("505", "NI"), ("506", "CR"), ("507", "PA"), ("508", "PM"), ("509", "HT"),
    ("590", "GP"), ("591", "BO"), ("592", "GY"), ("593", "EC"), ("594", "GF"),
    ("595", "PY"), ("596", "MQ"), ("597", "SR"), ("598", "UY"), ("599", "CW"),
    ("670", "TL"), ("672", "NF"), ("673", "BN"), ("674", "NR"), ("675", "PG"),
    ("676", "TO"), ("677", "SB"), ("678", "VU"), ("679", "FJ"), ("680", "PW"),
    ("681", "WF"), ("682", "CK"), ("683", "NU"), ("685", "WS"), ("686", "KI"),
    ("687", "NC"), ("688", "TV"), ("689", "PF"), ("690", "TK"), ("691", "FM"),
    ("692", "MH"),
    ("850", "KP"), ("852", "HK"), ("853", "MO"), ("855", "KH"), ("856", "LA"),
    ("880", "BD"), ("886", "TW"),
    ("960", "MV"), ("961", "LB"), ("962", "JO"), ("963", "SY"), ("964", "IQ"),
    ("965", "KW"), ("966", "SA"), ("967", "YE"), ("968", "OM"), ("970", "PS"),
    ("971", "AE"), ("972", "IL"), ("973", "BH"), ("974", "QA"), ("975", "BT"),
    ("976", "MN"), ("977", "NP"), ("992", "TJ"), ("993", "TM"), ("994", "AZ"),
    ("995", "GE"), ("996", "KG"), ("998", "UZ"),
];

/// NANP area codes that are NOT the United States. Everything else under `+1` resolves to
/// `US`, which is wrong for Canada in reporting but right for policy — Canada and the US
/// are the same rate band, while these territories are not.
const NANP_AREA_CODES: &[(&str, &str)] = &[
    ("242", "BS"), ("246", "BB"), ("264", "AI"), ("268", "AG"), ("284", "VG"),
    ("340", "VI"), ("345", "KY"), ("441", "BM"), ("473", "GD"), ("649", "TC"),
    ("658", "JM"), ("664", "MS"), ("670", "MP"), ("671", "GU"), ("684", "AS"),
    ("721", "SX"), ("758", "LC"), ("767", "DM"), ("784", "VC"), ("787", "PR"),
    ("809", "DO"), ("829", "DO"), ("849", "DO"), ("868", "TT"), ("869", "KN"),
    ("876", "JM"), ("939", "PR"),
    // Canada: listed so reporting is honest even though the rate band matches the US.
    ("204", "CA"), ("226", "CA"), ("236", "CA"), ("249", "CA"), ("250", "CA"),
    ("289", "CA"), ("306", "CA"), ("343", "CA"), ("365", "CA"), ("403", "CA"),
    ("416", "CA"), ("418", "CA"), ("431", "CA"), ("437", "CA"), ("438", "CA"),
    ("450", "CA"), ("506", "CA"), ("514", "CA"), ("519", "CA"), ("548", "CA"),
    ("579", "CA"), ("581", "CA"), ("587", "CA"), ("604", "CA"), ("613", "CA"),
    ("639", "CA"), ("647", "CA"), ("672", "CA"), ("705", "CA"), ("709", "CA"),
    ("742", "CA"), ("778", "CA"), ("780", "CA"), ("782", "CA"), ("807", "CA"),
    ("819", "CA"), ("825", "CA"), ("867", "CA"), ("873", "CA"), ("902", "CA"),
    ("905", "CA"),
];

#[cfg(test)]
mod tests {
    use super::*;

    fn ok(raw: &str) -> E164 {
        E164::parse(raw).unwrap_or_else(|e| panic!("{raw:?} should parse, got {e:?}"))
    }

    // ---- normalisation --------------------------------------------------------

    #[test]
    fn canonicalises_the_punctuation_people_actually_type() {
        // Every one of these is the same Italian mobile.
        for raw in [
            "+39 320 1234567",
            "+39-320-1234567",
            "+39 (320) 123.4567",
            "  +393201234567  ",
            "+39\u{00a0}320\u{00a0}1234567",
        ] {
            assert_eq!(ok(raw).as_str(), "+393201234567", "input {raw:?}");
        }
    }

    #[test]
    fn strips_an_international_access_prefix() {
        assert_eq!(ok("00393201234567").as_str(), "+393201234567");
        assert_eq!(ok("011393201234567").as_str(), "+393201234567");
    }

    #[test]
    fn an_explicit_plus_is_never_re_interpreted_as_an_access_prefix() {
        // `+00…` is not a number with an access prefix, it is a leading zero — refuse it
        // rather than quietly dialing something else.
        assert_eq!(E164::parse("+00393201234567"), Err(E164Error::LeadingZero));
    }

    #[test]
    fn rejects_input_that_is_not_a_number() {
        assert_eq!(E164::parse(""), Err(E164Error::Empty));
        assert_eq!(E164::parse("   "), Err(E164Error::Empty));
        assert_eq!(E164::parse("+"), Err(E164Error::Empty));
        assert_eq!(E164::parse("+39320CALLME"), Err(E164Error::NonNumeric));
        assert_eq!(E164::parse("tel:+393201234567"), Err(E164Error::NonNumeric));
    }

    #[test]
    fn rejects_lengths_outside_e164() {
        assert_eq!(E164::parse("+3932012"), Err(E164Error::TooShort));
        assert_eq!(E164::parse("+3932012345678901"), Err(E164Error::TooLong));
    }

    #[test]
    fn rejects_an_unassigned_country_code() {
        // 999 is not assigned; dialing it would be billed at whatever the carrier decides.
        assert_eq!(
            E164::parse("+9991234567890"),
            Err(E164Error::UnknownCountryCode)
        );
    }

    // ---- country resolution ---------------------------------------------------

    #[test]
    fn resolves_the_country_calling_code_by_longest_match() {
        assert_eq!(ok("+393201234567").calling_code(), "39");
        assert_eq!(ok("+8613800138000").calling_code(), "86");
        assert_eq!(ok("+442071838750").calling_code(), "44");
        // 3-digit codes must beat a 2-digit prefix of themselves: 35 is unassigned, 351 is
        // Portugal — a shortest-match table would have made this Greece-adjacent nonsense.
        assert_eq!(ok("+351212345678").calling_code(), "351");
        assert_eq!(ok("+298123456").calling_code(), "298");
    }

    #[test]
    fn china_resolves_to_cn_because_a_gate_depends_on_it() {
        let n = ok("+8613800138000");
        assert_eq!(n.calling_code(), "86");
        assert_eq!(n.region(), "CN");
    }

    #[test]
    fn nanp_territories_are_not_all_the_united_states() {
        // The toll-fraud case: an org allowing only "US" must not thereby allow a
        // premium-rate Caribbean range that also answers to +1.
        assert_eq!(ok("+12125551234").region(), "US");
        assert_eq!(ok("+14165551234").region(), "CA");
        assert_eq!(ok("+12685551234").region(), "AG");
        assert_eq!(ok("+18765551234").region(), "JM");
        // All of them still share the calling code.
        assert_eq!(ok("+12685551234").calling_code(), "1");
    }

    #[test]
    fn plus_seven_splits_russia_from_kazakhstan() {
        assert_eq!(ok("+79161234567").region(), "RU");
        assert_eq!(ok("+77011234567").region(), "KZ");
        assert_eq!(ok("+77011234567").calling_code(), "7");
    }

    // ---- privacy --------------------------------------------------------------

    #[test]
    fn the_display_form_shows_country_and_last_four_and_nothing_else() {
        let n = ok("+8613800138000");
        assert_eq!(n.last4(), "8000");
        let masked = n.masked();
        assert!(masked.starts_with("+86"), "{masked}");
        assert!(masked.ends_with("8000"), "{masked}");
        assert!(
            !masked.contains("1380013"),
            "the subscriber digits must not survive masking: {masked}"
        );
    }

    #[test]
    fn the_default_formatting_is_masked_so_a_stray_log_line_cannot_leak() {
        // This is the whole reason `Display` is not `as_str`.
        let n = ok("+393201234567");
        assert_eq!(format!("{n}"), n.masked());
        assert!(!format!("{n}").contains("3201234"));
    }

    #[test]
    fn the_pseudonym_is_stable_keyed_and_not_the_number() {
        let key = b"a-server-side-secret";
        let a = ok("+393201234567");
        let b = ok("+393201234567");
        let other = ok("+393201234568");

        assert_eq!(a.pseudonym(key), b.pseudonym(key), "stable for correlation");
        assert_ne!(a.pseudonym(key), other.pseudonym(key), "distinguishes numbers");
        assert_ne!(
            a.pseudonym(key),
            a.pseudonym(b"a-different-secret"),
            "keyed: without the key it cannot be recomputed"
        );
        assert!(!a.pseudonym(key).contains("3201234"));
        assert_eq!(a.pseudonym(key).len(), 16);
    }

    // ---- error codes ----------------------------------------------------------

    #[test]
    fn every_rejection_carries_a_stable_machine_readable_reason() {
        // The dashboard localises these; they are API surface, not prose.
        for (e, code) in [
            (E164Error::Empty, "number_empty"),
            (E164Error::NonNumeric, "number_non_numeric"),
            (E164Error::TooShort, "number_too_short"),
            (E164Error::TooLong, "number_too_long"),
            (E164Error::LeadingZero, "number_leading_zero"),
            (E164Error::UnknownCountryCode, "number_unknown_country"),
        ] {
            assert_eq!(e.code(), code);
        }
    }

    #[test]
    fn the_calling_code_table_has_no_duplicates_or_shadowed_entries() {
        // A duplicate would make longest-match depend on table order, which is exactly the
        // kind of bug that only shows up as one country billed as another.
        let mut seen = std::collections::HashSet::new();
        for (code, _) in CALLING_CODES {
            assert!(seen.insert(*code), "duplicate calling code {code}");
            assert!(
                !code.starts_with('1') && !code.starts_with('7'),
                "{code}: shared codes 1 and 7 are resolved in code, not in the table"
            );
        }
        let mut areas = std::collections::HashSet::new();
        for (area, _) in NANP_AREA_CODES {
            assert!(areas.insert(*area), "duplicate NANP area code {area}");
            assert_eq!(area.len(), 3, "{area}: NANP area codes are three digits");
        }
    }
}
