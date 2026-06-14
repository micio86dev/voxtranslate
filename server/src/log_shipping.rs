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
            let _ = self.tx.try_send(line);
        }
    }
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
}
