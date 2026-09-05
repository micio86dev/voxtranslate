//! Does this Model Studio region actually serve the models we dial?
//!
//! A key issued in a region without our models authenticates perfectly and then fails at
//! `session.update` with *"Access to model denied"* — the failure is late, per-session,
//! and looks like a client bug. US (Virginia) is the documented example: 89 models, none
//! of them realtime (`docs/pricing-standard-qwen.md`). So the region has to be checked
//! against its catalogue BEFORE it is pointed at production, not discovered by a speaker.
//!
//! Two models must be present, not one — the tier's translate model
//! ([`QwenConfig::model`]) and the realtime ASR model ([`QwenConfig::asr_model`]) that
//! backs `input_audio_transcription` in both session shapes. A region carrying only the
//! first gives translated audio with no original captions and no webinars at all.
//!
//! The URL derivation and the verdict are pure functions, tested here. The one impure
//! part is the GET. Run it through the `qwen-catalogue` binary:
//!
//! ```text
//! cargo run -p voxtranslate-server --bin qwen-catalogue
//! cargo run -p voxtranslate-server --bin qwen-catalogue -- --fallback
//! ```

use crate::config::QwenConfig;

/// Path the OpenAI-compatible model catalogue lives at on every Model Studio region.
const CATALOGUE_PATH: &str = "/compatible-mode/v1/models";

/// What a catalogue check found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    /// The catalogue URL that was queried — the point of the whole exercise is knowing
    /// WHICH host answered, so it is part of the result rather than only a log line.
    pub url: String,
    /// Models we need there, in the order they were asked for.
    pub required: Vec<String>,
    /// Of those, the ones the region does not serve. Empty ⇒ the region is usable.
    pub missing: Vec<String>,
    /// How many models the region serves in total. A large number with a non-empty
    /// `missing` is the Virginia signature: a healthy region, wrong catalogue.
    pub available: usize,
}

impl Report {
    /// Whether this region can serve the Standard tier.
    pub fn ok(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Turn a realtime WebSocket endpoint into that region's REST catalogue URL.
///
/// The endpoint is the only region identity we hold, and it is a `wss://` URL with a
/// realtime path; the catalogue is the same host over HTTPS at [`CATALOGUE_PATH`]. Any
/// query string is dropped — `?model=` belongs to the realtime dial, not to this GET.
pub fn catalogue_url(endpoint: &str, workspace_id: Option<&str>) -> Result<String, String> {
    let substituted = if endpoint.contains("{workspace}") {
        let ws = workspace_id
            .map(str::trim)
            .filter(|w| !w.is_empty())
            .ok_or(
                "endpoint carries a {workspace} placeholder but no workspace id is configured \
             (set QWEN_WORKSPACE_ID)",
            )?;
        endpoint.replace("{workspace}", ws)
    } else {
        endpoint.to_string()
    };

    // The realtime endpoint is a WebSocket URL; the catalogue is plain HTTP on the same
    // host. Map the scheme rather than assuming https, so a local/ws:// test double is
    // reachable too.
    let over_http = match substituted.split_once("://") {
        Some(("wss", rest)) => format!("https://{rest}"),
        Some(("ws", rest)) => format!("http://{rest}"),
        Some(("https", _)) | Some(("http", _)) => substituted.clone(),
        _ => {
            return Err(format!(
                "endpoint is not a ws/wss/http/https URL: {endpoint}"
            ))
        }
    };

    let mut url = url::Url::parse(&over_http).map_err(|e| format!("unparsable endpoint: {e}"))?;
    if url.host_str().is_none_or(str::is_empty) {
        return Err(format!("endpoint has no host: {endpoint}"));
    }
    url.set_path(CATALOGUE_PATH);
    url.set_query(None);
    Ok(url.to_string())
}

/// The models this config needs the region to serve — translate model AND realtime ASR.
pub fn required_models(config: &QwenConfig) -> Vec<String> {
    vec![config.model.clone(), config.asr_model.clone()]
}

/// Model ids out of an OpenAI-compatible `/v1/models` body.
///
/// Returns an error rather than an empty list when `data` is absent: "this region serves
/// zero models" and "we could not read the answer" must not look the same, because the
/// first is a stop-the-migration verdict and the second is a bad URL.
pub fn parse_models(body: &str) -> Result<Vec<String>, String> {
    let v: serde_json::Value =
        serde_json::from_str(body).map_err(|e| format!("catalogue is not JSON: {e}"))?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .ok_or_else(|| format!("catalogue has no `data` array: {}", truncate(body, 300)))?;
    Ok(data
        .iter()
        .filter_map(|m| m.get("id").and_then(|i| i.as_str()))
        .map(str::to_string)
        .collect())
}

/// Which of `required` the region does not serve.
///
/// Matches on the exact id. Model Studio also publishes dated snapshot ids
/// (`…-2025-09-22`) and those are NOT interchangeable with the alias at the wire level,
/// so a prefix match here would report a usable region that then fails at
/// `session.update` — precisely the late failure this module exists to move earlier.
pub fn missing_models(available: &[String], required: &[String]) -> Vec<String> {
    required
        .iter()
        .filter(|r| !available.iter().any(|a| a == *r))
        .cloned()
        .collect()
}

/// GET the catalogue and report on it. The only function here that touches the network.
pub async fn check(
    endpoint: &str,
    api_key: &str,
    workspace_id: Option<&str>,
    required: &[String],
) -> Result<Report, String> {
    let url = catalogue_url(endpoint, workspace_id)?;
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))?;

    let mut req = client
        .get(&url)
        .header("Authorization", format!("Bearer {api_key}"))
        .header("User-Agent", "voxtranslate-catalogue/1");
    // Same rule as the realtime dial: the workspace travels in a header unless the host
    // template already carried it.
    if let Some(ws) = workspace_id {
        if !endpoint.contains("{workspace}") && !ws.trim().is_empty() {
            req = req.header("X-DashScope-WorkSpace", ws);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("GET {url} failed: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("reading catalogue body: {e}"))?;
    if !status.is_success() {
        // 401 here is the good outcome of a bad key: it fails NOW, in a script, instead
        // of at the first speaker's first sentence.
        return Err(format!(
            "GET {url} returned {status}: {}",
            truncate(&body, 500)
        ));
    }

    let available = parse_models(&body)?;
    Ok(Report {
        url,
        missing: missing_models(&available, required),
        required: required.to_vec(),
        available: available.len(),
    })
}

/// First `n` chars of `s`, for error messages that must not paste a whole catalogue.
fn truncate(s: &str, n: usize) -> String {
    if s.chars().count() <= n {
        return s.to_string();
    }
    s.chars().take(n).collect::<String>() + "…"
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derives_the_catalogue_url_from_a_realtime_endpoint() {
        assert_eq!(
            catalogue_url("wss://dashscope-intl.aliyuncs.com/api-ws/v1/realtime", None).unwrap(),
            "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models"
        );
        // Frankfurt, the reason this exists.
        assert_eq!(
            catalogue_url(
                "wss://dashscope.eu-central-1.aliyuncs.com/api-ws/v1/realtime",
                None
            )
            .unwrap(),
            "https://dashscope.eu-central-1.aliyuncs.com/compatible-mode/v1/models"
        );
    }

    #[test]
    fn substitutes_the_workspace_host_template() {
        assert_eq!(
            catalogue_url(
                "wss://{workspace}.eu-central-1.maas.aliyuncs.com/api-ws/v1/realtime",
                Some("llm-abc123")
            )
            .unwrap(),
            "https://llm-abc123.eu-central-1.maas.aliyuncs.com/compatible-mode/v1/models"
        );
    }

    #[test]
    fn a_workspace_template_without_a_workspace_is_an_error_not_a_bad_host() {
        // Substituting nothing yields `wss://.eu-central-1…`, which parses and then
        // fails somewhere far away. Refuse it here instead.
        let err = catalogue_url("wss://{workspace}.eu-central-1.maas.aliyuncs.com/x", None)
            .expect_err("must refuse");
        assert!(err.contains("QWEN_WORKSPACE_ID"), "unhelpful error: {err}");
    }

    #[test]
    fn drops_the_realtime_query_string() {
        assert_eq!(
            catalogue_url("wss://host.example/api-ws/v1/realtime?model=whatever", None).unwrap(),
            "https://host.example/compatible-mode/v1/models"
        );
    }

    #[test]
    fn maps_plain_ws_to_http_so_a_local_double_is_reachable() {
        assert_eq!(
            catalogue_url("ws://127.0.0.1:8080/api-ws/v1/realtime", None).unwrap(),
            "http://127.0.0.1:8080/compatible-mode/v1/models"
        );
    }

    #[test]
    fn rejects_something_that_is_not_a_url() {
        assert!(catalogue_url("dashscope-intl.aliyuncs.com", None).is_err());
    }

    #[test]
    fn parses_an_openai_shaped_catalogue() {
        let body = r#"{"object":"list","data":[
            {"id":"qwen3.5-livetranslate-flash-realtime","object":"model"},
            {"id":"qwen3-asr-flash-realtime","object":"model"}
        ]}"#;
        assert_eq!(
            parse_models(body).unwrap(),
            vec![
                "qwen3.5-livetranslate-flash-realtime",
                "qwen3-asr-flash-realtime"
            ]
        );
    }

    #[test]
    fn an_unreadable_catalogue_is_an_error_not_an_empty_region() {
        // "zero models" stops a migration; "I could not read it" means fix the URL.
        assert!(parse_models("<html>404</html>").is_err());
        assert!(parse_models(r#"{"error":{"message":"invalid api key"}}"#).is_err());
        // An explicitly empty list, though, IS a legitimate (and damning) answer.
        assert_eq!(
            parse_models(r#"{"data":[]}"#).unwrap(),
            Vec::<String>::new()
        );
    }

    #[test]
    fn reports_each_missing_model_by_name() {
        let available = vec![
            "qwen3.5-livetranslate-flash-realtime".to_string(),
            "qwen-plus".to_string(),
        ];
        let required = vec![
            "qwen3.5-livetranslate-flash-realtime".to_string(),
            "qwen3-asr-flash-realtime".to_string(),
        ];
        assert_eq!(
            missing_models(&available, &required),
            vec!["qwen3-asr-flash-realtime"],
            "a region serving the translator but not the ASR model must NOT pass"
        );
        assert!(missing_models(&required, &required).is_empty());
    }

    #[test]
    fn a_dated_snapshot_does_not_satisfy_the_alias() {
        // `…-2025-09-22` is a different wire contract, not a synonym. Accepting it would
        // hand back a green report for a region that then denies the session.
        let available = vec!["qwen3-asr-flash-realtime-2025-09-22".to_string()];
        let required = vec!["qwen3-asr-flash-realtime".to_string()];
        assert_eq!(missing_models(&available, &required), required);
    }

    #[test]
    fn required_models_covers_both_catalogue_entries() {
        let c = QwenConfig {
            model: "translate-x".into(),
            asr_model: "asr-y".into(),
            ..Default::default()
        };
        assert_eq!(required_models(&c), vec!["translate-x", "asr-y"]);
    }
}
