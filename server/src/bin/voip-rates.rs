//! Import a provider rate deck into `voip_rates` (spec 0111, R5, D11).
//!
//! ```sh
//! DATABASE_URL=… cargo run -p voxtranslate-server --bin voip-rates -- rates.csv
//! DATABASE_URL=… cargo run -p voxtranslate-server --bin voip-rates -- --provider telnyx --dry-run rates.csv
//! ```
//!
//! ## Why a file and not an API call
//!
//! `TelephonyProvider::fetch_rate_deck` reports `Unsupported` for Telnyx, and that is not
//! an oversight — `GET /v2/public/pricing?primitive=voice` returns **404** on both
//! `api.telnyx.eu` and `api.telnyx.com`, and `/v2/pricing/products` returns a product
//! catalogue with no prefixes and no per-minute prices. What Telnyx actually offers is a
//! rate deck you **download** from the Outbound Voice Profile, so that is what this reads.
//!
//! The alternative — leaving the adapter pointed at an endpoint that does not exist —
//! means every call is refused with `rate_unavailable` for a reason no log explains.
//!
//! ## Freshness is a correctness property
//!
//! `voip_rates.fetched_at` is what `VOIP_RATE_MAX_AGE_SECS` measures, and a deck past that
//! age refuses calls rather than pricing them from stale numbers. So this stamps **now**
//! on every row it writes, and re-running it is how a deck stays fresh. Put it on a
//! schedule; a rate deck imported once is a rate deck that expires.
//!
//! ## What it will not do
//!
//! Guess. If a column cannot be identified the import stops and prints the headers it
//! actually saw, because a rate deck read with the wrong column mapping does not fail —
//! it prices every call wrongly, and the first anyone hears of it is the invoice.

use std::collections::HashMap;
use std::process::ExitCode;

use rust_decimal::Decimal;
use voxtranslate_server::db;

/// Header names we accept for each field, lowercased. Providers spell these differently
/// and change them between exports; recognising several is cheaper than a support ticket.
const PREFIX_KEYS: &[&str] = &[
    "prefix",
    "dial prefix",
    "dialprefix",
    "code",
    "country code",
    "destination prefix",
    "e164 prefix",
];
const COST_KEYS: &[&str] = &[
    "rate",
    "cost",
    "price",
    "rate per minute",
    "cost per minute",
    "price per minute",
    "per minute",
    "ppm",
    "usd",
];
const DESC_KEYS: &[&str] = &[
    "destination",
    "description",
    "name",
    "country",
    "destination name",
    "region",
];
const TYPE_KEYS: &[&str] = &["type", "number type", "line type", "destination type"];

fn main() -> ExitCode {
    let _ = dotenvy::dotenv();
    let args: Vec<String> = std::env::args().skip(1).collect();

    let mut provider = "telnyx".to_string();
    let mut dry_run = false;
    let mut path: Option<String> = None;
    let mut it = args.iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "--provider" => match it.next() {
                Some(v) => provider = v.clone(),
                None => return fail("--provider needs a value"),
            },
            "--dry-run" => dry_run = true,
            "-h" | "--help" => {
                eprintln!(
                    "usage: voip-rates [--provider <id>] [--dry-run] <rate-deck.csv>\n\
                     \n\
                     Imports a downloaded rate deck into voip_rates. Stamps fetched_at with\n\
                     the current time, which is what VOIP_RATE_MAX_AGE_SECS measures — so\n\
                     re-run it on a schedule, or the deck goes stale and refuses calls."
                );
                return ExitCode::SUCCESS;
            }
            other if other.starts_with('-') => {
                return fail(&format!("unknown option {other}"));
            }
            other => path = Some(other.to_string()),
        }
    }

    let Some(path) = path else {
        return fail("no rate deck given (expected a CSV path)");
    };
    let raw = match std::fs::read_to_string(&path) {
        Ok(r) => r,
        Err(e) => return fail(&format!("could not read {path}: {e}")),
    };

    let parsed = match parse(&raw) {
        Ok(p) => p,
        Err(e) => return fail(&e),
    };

    println!(
        "{} rate(s) parsed from {path} for provider `{provider}`",
        parsed.len()
    );
    if let Some(first) = parsed.first() {
        println!(
            "  sample: +{} = {} /min  ({}, {})",
            first.prefix, first.cost_per_minute, first.description, first.number_type
        );
    }

    if dry_run {
        println!("--dry-run: nothing written");
        return ExitCode::SUCCESS;
    }

    let Ok(url) = std::env::var("DATABASE_URL") else {
        return fail("DATABASE_URL is not set");
    };

    // Say where this is going BEFORE going there. `dotenvy` above means an unset
    // DATABASE_URL is silently filled in from `server/.env`, which on a developer machine
    // points at a deployed database — and the write below opens with a DELETE. An importer
    // that wipes and replaces a rate deck must never leave the operator guessing which
    // deck. Host and database only: the credentials in the URL are not ours to print.
    println!("target: {}", redacted_target(&url));

    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => return fail(&format!("could not start a runtime: {e}")),
    };
    rt.block_on(async move {
        let pool = match db::connect(&url).await {
            Ok(p) => p,
            Err(e) => return fail(&format!("database: {e}")),
        };
        match write(&pool, &provider, &parsed).await {
            Ok(n) => {
                println!("wrote {n} rate(s); fetched_at stamped now");
                ExitCode::SUCCESS
            }
            Err(e) => fail(&format!("write failed: {e}")),
        }
    })
}

/// `host:port/database` from a connection URL, with any credentials dropped.
///
/// Deliberately string-sliced rather than parsed: this runs on the path that is about to
/// DELETE, so it must not be able to fail or panic on a URL shape it did not expect. An
/// unparseable URL still prints something the operator can recognise.
fn redacted_target(url: &str) -> String {
    let after_scheme = url.split("://").nth(1).unwrap_or(url);
    // Credentials, if present, end at the last '@' before the host.
    let host_and_path = after_scheme.rsplit('@').next().unwrap_or(after_scheme);
    host_and_path.split('?').next().unwrap_or(host_and_path).to_string()
}

fn fail(msg: &str) -> ExitCode {
    eprintln!("voip-rates: {msg}");
    ExitCode::FAILURE
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportedRate {
    pub prefix: String,
    pub cost_per_minute: Decimal,
    pub description: String,
    pub number_type: String,
}

/// Parse a rate deck, or say exactly why it could not be.
pub fn parse(raw: &str) -> Result<Vec<ImportedRate>, String> {
    let mut lines = raw.lines().filter(|l| !l.trim().is_empty());
    let header = lines.next().ok_or("the file is empty")?;
    let delim = detect_delimiter(header);
    let cols: Vec<String> = split(header, delim)
        .into_iter()
        .map(|c| c.trim().trim_matches('"').to_lowercase())
        .collect();

    let find = |keys: &[&str]| -> Option<usize> {
        // Exact match first: a header called "rate" must not lose to "rate centre"
        // because the latter happened to contain the former.
        keys.iter()
            .find_map(|k| cols.iter().position(|c| c == k))
            .or_else(|| {
                keys.iter()
                    .find_map(|k| cols.iter().position(|c| c.contains(k)))
            })
    };

    let missing = |what: &str, keys: &[&str]| -> String {
        format!(
            "could not find the {what} column.\n  headers seen: {}\n  names accepted: {}\n\
             Refusing to guess: a deck read with the wrong mapping does not fail, it prices \
             every call wrongly, and the first anyone hears of it is the invoice.\n\
             Add the real header name to `{}_KEYS` in src/bin/voip-rates.rs.",
            cols.join(", "),
            keys.join(", "),
            what.to_uppercase().replace(' ', "_"),
        )
    };

    let i_prefix = find(PREFIX_KEYS).ok_or_else(|| missing("prefix", PREFIX_KEYS))?;
    let i_cost = find(COST_KEYS).ok_or_else(|| missing("cost", COST_KEYS))?;
    let i_desc = find(DESC_KEYS);
    let i_type = find(TYPE_KEYS);

    let mut out: HashMap<String, ImportedRate> = HashMap::new();
    for (n, line) in lines.enumerate() {
        let f = split(line, delim);
        let get = |i: usize| -> String {
            f.get(i)
                .map(|v| v.trim().trim_matches('"').to_string())
                .unwrap_or_default()
        };

        let prefix: String = get(i_prefix)
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect();
        if prefix.is_empty() {
            continue; // a blank or a totals row, not an error
        }

        let raw_cost = get(i_cost);
        let cost: Decimal = parse_price(&raw_cost).ok_or_else(|| {
            format!(
                "line {}: {raw_cost:?} is not a price. Stopping rather than importing a \
                 partial deck — half a rate deck refuses the calls it has no rows for, \
                 which reads as an outage.",
                n + 2
            )
        })?;
        if cost < Decimal::ZERO {
            return Err(format!("line {}: negative rate {cost}", n + 2));
        }

        let number_type = i_type
            .map(get)
            .map(|t| normalise_type(&t))
            .unwrap_or_else(|| "other".into());
        let description = i_desc.map(get).unwrap_or_default();

        // Last row wins for a repeated prefix, and the primary key would reject the
        // second anyway. Deduping here keeps the reported count honest.
        out.insert(
            prefix.clone(),
            ImportedRate {
                prefix,
                cost_per_minute: cost,
                description,
                number_type,
            },
        );
    }

    if out.is_empty() {
        return Err("no usable rows — every line had an empty prefix".into());
    }
    let mut v: Vec<ImportedRate> = out.into_values().collect();
    v.sort_by(|a, b| a.prefix.cmp(&b.prefix));
    Ok(v)
}

/// Read a price written in either decimal convention.
///
/// The delimiter heuristic below already states the reason this has to exist: a European
/// export is semicolon-delimited *because the comma is the decimal separator*. So the
/// comma in `0,0085` is a decimal point, not decoration — dropping it reads eight
/// thousandths as **85**, four orders of magnitude, with no error anywhere. That is the
/// precise failure this file was written to refuse, and it does not announce itself: the
/// row imports cleanly and the first anyone hears of it is the invoice.
///
/// The rule that handles every convention without guessing: **the last separator is the
/// decimal point**, and anything before it groups digits. `1,234.56` and `1.234,56` are
/// then the same number, which they are. A lone comma is therefore decimal — for a
/// per-minute rate that is the only sane reading, since nobody publishes a destination at
/// one thousand two hundred a minute.
fn parse_price(raw: &str) -> Option<Decimal> {
    let kept: String = raw
        .chars()
        .filter(|c| c.is_ascii_digit() || matches!(c, '.' | ',' | '-'))
        .collect();
    let normalised = match kept.rfind(['.', ',']) {
        None => kept,
        Some(i) => {
            // Separators are ASCII, so splitting on the byte index is safe.
            let (head, tail) = kept.split_at(i);
            let head: String = head.chars().filter(|c| !matches!(c, '.' | ',')).collect();
            format!("{head}.{}", &tail[1..])
        }
    };
    normalised.parse().ok()
}

/// Tab, semicolon or comma. Exports differ by locale, and a European CSV is very often
/// semicolon-delimited because the comma is the decimal separator.
fn detect_delimiter(header: &str) -> char {
    for d in ['\t', ';', ','] {
        if header.contains(d) {
            return d;
        }
    }
    ','
}

/// Split respecting double quotes, so a description containing the delimiter survives.
fn split(line: &str, delim: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    for c in line.chars() {
        match c {
            '"' => quoted = !quoted,
            _ if c == delim && !quoted => out.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
    }
    out.push(cur);
    out
}

/// The column is advisory — the price comes from the prefix — but the schema constrains
/// it, so an unrecognised value becomes `other` rather than failing the import.
fn normalise_type(raw: &str) -> String {
    let t = raw.to_lowercase();
    if t.contains("mobile") || t.contains("cell") || t.contains("wireless") {
        "mobile".into()
    } else if t.contains("landline") || t.contains("fixed") || t.contains("geographic") {
        "landline".into()
    } else {
        "other".into()
    }
}

/// Replace the deck for one provider, in one transaction.
///
/// Delete-then-insert rather than upsert: a prefix the provider **removed** must disappear,
/// and an upsert would leave it priced from the last deck that mentioned it for ever. All
/// or nothing, because a half-written deck refuses the calls it has no rows for.
pub async fn write(
    pool: &db::Pool,
    provider: &str,
    rates: &[ImportedRate],
) -> Result<usize, sqlx::Error> {
    let mut tx = pool.begin().await?;
    sqlx::query("DELETE FROM voip_rates WHERE provider = $1")
        .bind(provider)
        .execute(&mut *tx)
        .await?;

    for r in rates {
        sqlx::query(
            "INSERT INTO voip_rates
                (provider, prefix, description, cost_per_minute, currency, number_type, fetched_at)
             VALUES ($1, $2, $3, $4, 'USD', $5, now())",
        )
        .bind(provider)
        .bind(&r.prefix)
        .bind(&r.description)
        .bind(r.cost_per_minute)
        .bind(&r.number_type)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(rates.len())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_an_ordinary_comma_deck() {
        let csv = "Prefix,Destination,Rate\n39,Italy,0.0085\n3932,Italy Mobile,0.0410\n";
        let r = parse(csv).unwrap();
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].prefix, "39");
        assert_eq!(r[0].cost_per_minute, "0.0085".parse::<Decimal>().unwrap());
        assert_eq!(r[1].description, "Italy Mobile");
    }

    #[test]
    fn reads_a_semicolon_deck() {
        // European exports are routinely semicolon-delimited. Guessing comma would put the
        // whole row in one column and report "could not find the prefix column", which is
        // a confusing way to say "wrong delimiter".
        let csv = "Prefix;Destination;Rate\n44;United Kingdom;0.0072\n";
        let r = parse(csv).unwrap();
        assert_eq!(r[0].prefix, "44");
        assert_eq!(r[0].description, "United Kingdom");
    }

    #[test]
    fn a_description_containing_the_delimiter_survives() {
        let csv = "Prefix,Destination,Rate\n1,\"United States, Alaska\",0.0090\n";
        let r = parse(csv).unwrap();
        assert_eq!(r[0].description, "United States, Alaska");
        assert_eq!(r[0].cost_per_minute, "0.0090".parse::<Decimal>().unwrap());
    }

    #[test]
    fn a_plus_and_currency_symbols_are_stripped() {
        // Decks arrive formatted for humans. `+39` is the same prefix as `39`, and a `$`
        // in the rate column is decoration, not data.
        let csv = "Prefix,Destination,Rate\n+39,Italy,$0.0085\n";
        let r = parse(csv).unwrap();
        assert_eq!(r[0].prefix, "39");
        assert_eq!(r[0].cost_per_minute, "0.0085".parse::<Decimal>().unwrap());
    }

    #[test]
    fn the_number_type_is_recognised_or_becomes_other() {
        let csv = "Prefix,Destination,Rate,Type\n\
                   3932,Italy,0.041,Mobile\n\
                   3902,Italy,0.008,Fixed Line\n\
                   3980,Italy,0.500,Premium\n";
        let r = parse(csv).unwrap();
        let by: HashMap<_, _> = r
            .iter()
            .map(|x| (x.prefix.as_str(), &x.number_type))
            .collect();
        assert_eq!(by["3932"], "mobile");
        assert_eq!(by["3902"], "landline");
        // The schema constrains this column, so an unknown value must not fail the import
        // over something the price does not even depend on.
        assert_eq!(by["3980"], "other");
    }

    #[test]
    fn an_unmappable_header_stops_and_says_what_it_saw() {
        // The whole point. Guessing produces a deck that prices every call wrongly and
        // fails nowhere, which is discovered on the invoice.
        let err = parse("Foo,Bar,Baz\n1,2,3\n").unwrap_err();
        assert!(err.contains("could not find the prefix column"), "{err}");
        assert!(
            err.contains("foo, bar, baz"),
            "must show the real headers: {err}"
        );
        assert!(err.contains("Refusing to guess"), "{err}");
    }

    #[test]
    fn a_bad_price_stops_the_whole_import() {
        // Not skipped. A partial deck refuses the calls it has no rows for, and an
        // operator reads that as an outage rather than as a malformed file.
        let err = parse("Prefix,Destination,Rate\n39,Italy,n/a\n").unwrap_err();
        assert!(err.contains("line 2"), "{err}");
        assert!(err.contains("not a price"), "{err}");
    }

    #[test]
    fn blank_and_total_rows_are_skipped_rather_than_fatal() {
        // Real exports end with a totals line or a trailing blank.
        let csv = "Prefix,Destination,Rate\n39,Italy,0.0085\n\n,TOTAL,1.23\n";
        let r = parse(csv).unwrap();
        assert_eq!(r.len(), 1);
    }

    #[test]
    fn a_repeated_prefix_does_not_become_two_rows() {
        // `(provider, prefix)` is the primary key, so a duplicate would abort the whole
        // transaction at the second insert. Last one wins, and the count stays honest.
        let csv = "Prefix,Destination,Rate\n39,Italy,0.0085\n39,Italy revised,0.0090\n";
        let r = parse(csv).unwrap();
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].cost_per_minute, "0.0090".parse::<Decimal>().unwrap());
    }

    #[test]
    fn an_exact_header_beats_a_containing_one() {
        // "Rate Centre" contains "rate". If it won, every price would be unparseable — or
        // worse, parse to something.
        let csv = "Prefix,Rate Centre,Rate\n39,Milan,0.0085\n";
        let r = parse(csv).unwrap();
        assert_eq!(r[0].cost_per_minute, "0.0085".parse::<Decimal>().unwrap());
    }

    #[test]
    fn a_european_deck_prices_the_decimal_comma_correctly() {
        // The delimiter heuristic already says WHY these exist: a European CSV is
        // semicolon-delimited *because the comma is the decimal separator*. So the comma
        // in `0,0085` is a decimal point, not decoration — dropping it reads the rate as
        // 85/min instead of 0.0085/min. Four orders of magnitude, no error, discovered
        // on the invoice. Exactly the failure this file exists to refuse.
        let csv = "Prefix;Destination;Rate\n39;Italy;0,0085\n3932;Italy Mobile;0,0410\n";
        let r = parse(csv).unwrap();
        assert_eq!(r[0].cost_per_minute, "0.0085".parse::<Decimal>().unwrap());
        assert_eq!(r[1].cost_per_minute, "0.0410".parse::<Decimal>().unwrap());
    }

    #[test]
    fn a_decimal_comma_survives_inside_a_comma_delimited_deck() {
        // Quoted, so the field itself still parses — but the value is European.
        let csv = "Prefix,Destination,Rate\n39,Italy,\"0,0085\"\n";
        let r = parse(csv).unwrap();
        assert_eq!(r[0].cost_per_minute, "0.0085".parse::<Decimal>().unwrap());
    }

    #[test]
    fn a_grouping_separator_is_not_mistaken_for_a_decimal_point() {
        // Both conventions for the same number. The LAST separator is the decimal one;
        // anything before it groups digits.
        let csv = "Prefix;Destination;Rate\n\
                   1;US thousands;1,234.56\n\
                   44;UK european;1.234,56\n";
        let r = parse(csv).unwrap();
        let by: HashMap<_, _> = r
            .iter()
            .map(|x| (x.prefix.as_str(), x.cost_per_minute))
            .collect();
        assert_eq!(by["1"], "1234.56".parse::<Decimal>().unwrap());
        assert_eq!(by["44"], "1234.56".parse::<Decimal>().unwrap());
    }

    #[test]
    fn the_target_line_names_the_database_without_leaking_the_password() {
        // Printed on the path that is about to DELETE, so it has to be readable AND safe
        // to paste into a chat or an issue.
        let t = redacted_target("postgresql://user:s3cret@aws-1-eu-central-1.pooler.supabase.com:5432/postgres");
        assert_eq!(t, "aws-1-eu-central-1.pooler.supabase.com:5432/postgres");
        assert!(!t.contains("s3cret"), "the password must never be printed: {t}");
        assert!(!t.contains("user"), "nor the username: {t}");
    }

    #[test]
    fn the_target_line_survives_a_url_it_cannot_understand() {
        // It must not panic here of all places. Something recognisable beats an abort.
        assert_eq!(redacted_target("localhost/voxtranslate_voip_test"), "localhost/voxtranslate_voip_test");
        assert_eq!(redacted_target(""), "");
        assert_eq!(
            redacted_target("postgres://h/db?sslmode=require"),
            "h/db",
            "query parameters are noise here"
        );
    }
}
