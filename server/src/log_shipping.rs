//! Optional Better Stack Logs shipping (spec 0063, issue #69). Railway has no native
//! log drains, so to forward our canonical JSON logs (spec 0050) to Better Stack Logs
//! the server ships them itself: an env-gated `tracing` JSON `fmt` layer serialises each
//! (filtered) event and hands the line to a background task that batches lines and POSTs
//! them as NDJSON to the Better Stack ingest endpoint.
//!
//! **Opt-in / off by default.** The layer only exists when `BETTERSTACK_SOURCE_TOKEN` is
//! set, so local/dev and any deploy without the token are completely unaffected — no
//! channel, no task, no extra serialisation (the free tier has a volume cap; the owner
//! enables shipping deliberately). Shipping is **best-effort and never blocks request
//! handling**: the writer `try_send`s onto a bounded channel and drops lines on overflow
//! rather than applying network backpressure to the logging hot path. The shipper logs
//! its own failures with `eprintln!` (not `tracing`) so a failed POST can't feed itself.

use std::io;

use tokio::sync::mpsc;
use tracing::Subscriber;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::Layer;

/// Better Stack's default HTTP ingest host; override per-source with `BETTERSTACK_INGEST_URL`.
const DEFAULT_INGEST_URL: &str = "https://in.logs.betterstack.com";
/// Bounded queue between the logging hot path and the async flusher (drop-on-full).
const CHANNEL_CAPACITY: usize = 2048;
/// Max lines coalesced into a single ingest POST.
const MAX_BATCH: usize = 100;

/// Build the Better Stack shipping layer if `BETTERSTACK_SOURCE_TOKEN` is set, spawning
/// the background flush task. Returns `None` (a no-op layer slot) otherwise, so callers
/// can unconditionally `.with(log_shipping::layer())`.
///
/// Must be called from within a Tokio runtime (it `tokio::spawn`s the flusher) — which is
/// the case in `observability::init_tracing`, run inside `serve()`.
pub fn layer<S>() -> Option<Box<dyn Layer<S> + Send + Sync>>
where
    S: Subscriber + for<'a> LookupSpan<'a>,
{
    let token = std::env::var("BETTERSTACK_SOURCE_TOKEN")
        .ok()
        .filter(|s| !s.is_empty())?;
    let url = std::env::var("BETTERSTACK_INGEST_URL")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| DEFAULT_INGEST_URL.to_string());

    let (tx, rx) = mpsc::channel::<String>(CHANNEL_CAPACITY);
    tokio::spawn(flush_loop(rx, url, token));

    let layer = tracing_subscriber::fmt::layer()
        .json()
        .flatten_event(true)
        .with_current_span(true)
        .with_span_list(false)
        .with_writer(ChannelMakeWriter { tx })
        .boxed();
    Some(layer)
}

/// Drain the channel, coalesce bursts up to `MAX_BATCH`, and POST each batch as NDJSON.
/// Ends when every `Sender` is dropped (process shutdown).
async fn flush_loop(mut rx: mpsc::Receiver<String>, url: String, token: String) {
    let client = reqwest::Client::new();
    let mut batch: Vec<String> = Vec::with_capacity(MAX_BATCH);
    while let Some(first) = rx.recv().await {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(line) => batch.push(line),
                Err(_) => break,
            }
        }
        let body = ndjson(&batch);
        batch.clear();
        if let Err(e) = post(&client, &url, &token, body).await {
            eprintln!("betterstack logs: ingest request failed: {e}");
        }
    }
}

/// POST one NDJSON batch to the Better Stack source. Non-2xx is surfaced on stderr (not
/// via `tracing`, to avoid recursively shipping the shipper's own logs).
async fn post(
    client: &reqwest::Client,
    url: &str,
    token: &str,
    body: String,
) -> Result<(), reqwest::Error> {
    let res = client
        .post(url)
        .header(reqwest::header::AUTHORIZATION, format!("Bearer {token}"))
        .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
        .body(body)
        .send()
        .await?;
    if !res.status().is_success() {
        eprintln!("betterstack logs: ingest returned HTTP {}", res.status());
    }
    Ok(())
}

/// Concatenate already-newline-terminated JSON lines into one NDJSON body, terminating
/// any line that lacks a trailing newline.
fn ndjson(lines: &[String]) -> String {
    let mut body = String::with_capacity(lines.iter().map(|l| l.len() + 1).sum());
    for l in lines {
        body.push_str(l);
        if !l.ends_with('\n') {
            body.push('\n');
        }
    }
    body
}

/// `MakeWriter` that hands each formatted event to the flush channel.
struct ChannelMakeWriter {
    tx: mpsc::Sender<String>,
}

impl<'a> MakeWriter<'a> for ChannelMakeWriter {
    type Writer = ChannelWriter;
    fn make_writer(&'a self) -> Self::Writer {
        ChannelWriter {
            tx: self.tx.clone(),
            buf: Vec::new(),
        }
    }
}

/// Buffers one formatted event and forwards it on drop. `try_send` never blocks the
/// logging path: on a full (or closed) channel the line is silently dropped.
struct ChannelWriter {
    tx: mpsc::Sender<String>,
    buf: Vec<u8>,
}

impl io::Write for ChannelWriter {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buf.extend_from_slice(data);
        Ok(data.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Drop for ChannelWriter {
    fn drop(&mut self) {
        if self.buf.is_empty() {
            return;
        }
        if let Ok(line) = String::from_utf8(std::mem::take(&mut self.buf)) {
            let _ = self.tx.try_send(redact_ip(&line));
        }
    }
}

/// Blunt the client IP on its way OUT of our infrastructure.
///
/// The canonical log records the full address (`observability.rs`), and that has to stay:
/// the same value backs rate limiting, guest quotas and admin auth, so truncating it at
/// the source would silently widen those buckets to whole subnets. What changes is what
/// LEAVES — an IP is personal data under GDPR, and Better Stack Logs is a third-party
/// processor outside our own infrastructure.
///
/// The last IPv4 octet (and the host half of an IPv6 address) is dropped, which is the
/// conventional anonymisation: enough to keep network, ASN and rough geography for
/// debugging, not enough to single out a subscriber line.
///
/// String-level on purpose: the line is already serialised JSON at this point, and
/// re-parsing every log event to edit one field would put a serde round-trip on a path
/// that must never slow the logger down.
fn redact_ip(line: &str) -> String {
    let mut out = String::with_capacity(line.len());
    let mut rest = line;
    // The canonical log writes the address as `"ip":"…"` (see `observability::client_ip`).
    while let Some(at) = rest.find(IP_FIELD) {
        let (before, tail) = rest.split_at(at + IP_FIELD.len());
        out.push_str(before);
        match tail.find('"') {
            Some(end) => {
                out.push_str(&truncate_ip(&tail[..end]));
                rest = &tail[end..];
            }
            // Unterminated value — emit the remainder untouched rather than corrupt it.
            None => {
                rest = tail;
                break;
            }
        }
    }
    out.push_str(rest);
    out
}

/// The `"ip":"` prefix the canonical JSON log writes before the address.
const IP_FIELD: &str = "\"ip\":\"";

/// Drop the identifying tail of one address. Unrecognised values are replaced outright:
/// a value we cannot parse is a value we cannot promise is anonymised.
fn truncate_ip(ip: &str) -> String {
    if ip.is_empty() {
        return String::new();
    }
    if let Some((head, last)) = ip.rsplit_once('.') {
        // IPv4: keep the /24. Every octet is validated, the last one included — without
        // that check `1.2.3.999` would be mistaken for an address and kept as `1.2.3.0`,
        // reporting a redaction that never happened.
        let octets_ok = head.split('.').count() == 3
            && head.split('.').all(|o| o.parse::<u8>().is_ok())
            && last.parse::<u8>().is_ok();
        if octets_ok {
            return format!("{head}.0");
        }
    }
    if ip.contains(':') {
        // IPv6: keep the /48 routing prefix, drop the rest.
        let head: Vec<&str> = ip.split(':').take(3).collect();
        if head.len() == 3 && head.iter().all(|g| u16::from_str_radix(g, 16).is_ok()) {
            return format!("{}::", head.join(":"));
        }
    }
    "redacted".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    #[test]
    fn channel_writer_forwards_complete_line_on_drop() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let mw = ChannelMakeWriter { tx };
        {
            let mut w = mw.make_writer();
            w.write_all(b"{\"msg\":\"hi\"}\n").unwrap();
        } // drop → send
        assert_eq!(rx.try_recv().unwrap(), "{\"msg\":\"hi\"}\n");
    }

    #[test]
    fn channel_writer_drops_when_full_without_blocking() {
        // Capacity 1: the first event buffers, the rest are dropped — no panic, no block.
        let (tx, mut rx) = mpsc::channel::<String>(1);
        let mw = ChannelMakeWriter { tx };
        for _ in 0..3 {
            let mut w = mw.make_writer();
            w.write_all(b"line\n").unwrap();
        }
        assert_eq!(rx.try_recv().unwrap(), "line\n");
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn empty_event_is_not_sent() {
        let (tx, mut rx) = mpsc::channel::<String>(4);
        let mw = ChannelMakeWriter { tx };
        drop(mw.make_writer()); // nothing written
        assert!(rx.try_recv().is_err());
    }

    #[test]
    fn ndjson_terminates_every_line() {
        let lines = vec!["{\"a\":1}\n".to_string(), "{\"b\":2}".to_string()];
        assert_eq!(ndjson(&lines), "{\"a\":1}\n{\"b\":2}\n");
    }

    // ── IP redaction on the way out ──────────────────────────────────────────────
    //
    // The full address stays in the canonical log — rate limiting, guest quotas and
    // admin auth all key on it. These tests pin what LEAVES for Better Stack.

    #[test]
    fn keeps_the_ipv4_network_and_drops_the_host() {
        let line = r#"{"level":"info","span":{"ip":"89.106.7.67","method":"GET"}}"#;
        let out = redact_ip(line);
        assert!(out.contains(r#""ip":"89.106.7.0""#), "{out}");
        assert!(!out.contains("89.106.7.67"), "full address survived: {out}");
        // Everything else must be byte-identical.
        assert!(out.contains(r#""method":"GET""#));
        assert!(out.contains(r#""level":"info""#));
    }

    #[test]
    fn keeps_the_ipv6_routing_prefix_only() {
        let line = r#"{"span":{"ip":"2001:0db8:85a3:1319:8a2e:0370:7344"}}"#;
        let out = redact_ip(line);
        assert!(out.contains(r#""ip":"2001:0db8:85a3::""#), "{out}");
        assert!(!out.contains("8a2e"), "host half survived: {out}");
    }

    #[test]
    fn redacts_every_occurrence_in_one_line() {
        // A batched or nested event can carry the field more than once; missing the
        // second one would leak exactly as badly as missing the first.
        let line = r#"{"a":{"ip":"1.2.3.4"},"b":{"ip":"5.6.7.8"}}"#;
        let out = redact_ip(line);
        assert!(out.contains(r#""ip":"1.2.3.0""#));
        assert!(out.contains(r#""ip":"5.6.7.0""#));
        assert!(!out.contains("1.2.3.4"));
        assert!(!out.contains("5.6.7.8"));
    }

    #[test]
    fn replaces_anything_it_cannot_parse_rather_than_passing_it_through() {
        // A value we cannot parse is a value we cannot promise is anonymised.
        let out = redact_ip(r#"{"ip":"not-an-address"}"#);
        assert!(out.contains(r#""ip":"redacted""#), "{out}");
        assert!(!out.contains("not-an-address"));
        // 999 is not an octet: this must NOT be treated as IPv4 and kept.
        let out = redact_ip(r#"{"ip":"1.2.3.999"}"#);
        assert!(out.contains(r#""ip":"redacted""#), "{out}");
    }

    #[test]
    fn leaves_a_line_without_an_ip_completely_untouched() {
        let line = r#"{"level":"warn","message":"qwen: PRIMARY region refused"}"#;
        assert_eq!(redact_ip(line), line);
    }

    #[test]
    fn an_empty_or_unterminated_value_does_not_corrupt_the_line() {
        assert!(redact_ip(r#"{"ip":""}"#).contains(r#""ip":"""#));
        // Truncated JSON must come out as-is, not mangled.
        let broken = r#"{"ip":"1.2.3"#;
        assert_eq!(redact_ip(broken), broken);
    }

    #[test]
    fn the_writer_redacts_before_the_line_reaches_the_channel() {
        // End to end: whatever the logger formatted, the channel must never see a full
        // address — this is the assertion that actually protects the shipping path.
        let (tx, mut rx) = mpsc::channel::<String>(4);
        {
            let mut w = ChannelWriter {
                tx,
                buf: Vec::new(),
            };
            w.write_all(br#"{"span":{"ip":"203.0.113.42"}}"#).unwrap();
        }
        let got = rx.try_recv().expect("line forwarded");
        assert!(got.contains(r#""ip":"203.0.113.0""#), "{got}");
        assert!(!got.contains("203.0.113.42"));
    }
}
