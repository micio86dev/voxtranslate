//! Dashboard Help Assistant relay engine.
//!
//! Bridges a dashboard WebSocket client to OpenAI Realtime (`v1/realtime`) for
//! full-duplex voice Q&A grounded by a STATIC product-knowledge prompt (no RAG).
//!
//! ## Session lifecycle
//!
//! 1. Acquire a semaphore permit (capacity cap from `HelpAssistantConfig::max_sessions`).
//! 2. Send `session.update` with static dashboard help instructions.
//! 3. Open the upstream OpenAI Realtime WS.
//! 4. Bidirectional relay loop:
//!    - Browser binary frames (PCM16) → `input_audio_buffer.append` to OpenAI.
//!    - Browser text `{"type":"stop"}` → close upstream and begin flush.
//!    - OpenAI events → forwarded as text frames to the browser.
//!    - Every 10 s: emit `cost_tick` to browser; deduct credits.
//! 5. On close: deduct final credits, persist interaction with
//!    `interaction_kind = 'help_assistant'` (NO re-embed — help answers are
//!    not knowledge-base content).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{interval, Instant, MissedTickBehavior};
use uuid::Uuid;

use crate::business::credits;
use crate::config::HelpAssistantConfig;
use crate::db::Pool;
use crate::engine::help_assistant_client::{
    self, build_cost_tick_json, build_session_end_json, format_cost_display, ha_audio_append_json,
    open_ha_session, HaEvent, HaSink, HaSource,
};
use tokio_tungstenite::tungstenite::Message as TungMessage;

/// How often credits are deducted and a `cost_tick` is emitted to the client.
const TICK_SECS: u64 = 10;

/// Error JSON sent when the semaphore is full.
pub fn capacity_full_error() -> String {
    serde_json::json!({
        "type": "error",
        "code": "capacity_full",
        "message": "Help assistant is at capacity. Please try again in a moment.",
    })
    .to_string()
}

/// Error JSON sent when org credits are exhausted.
fn credits_exhausted_error(balance: i32, required: i32) -> String {
    serde_json::json!({
        "type": "error",
        "code": "credits_exhausted",
        "message": "Insufficient credits to continue the help assistant session.",
        "balance": balance,
        "required": required,
    })
    .to_string()
}

/// Dependencies injected by the route handler into the relay.
///
/// Notably lighter than `voice_assistant::RelayDeps` — no embedder, no RAG chunks,
/// no project/member scope. The static prompt eliminates all of those.
pub struct RelayDeps {
    pub config: HelpAssistantConfig,
    pub semaphore: Arc<Semaphore>,
    pub pool: Pool,
    pub org_id: Uuid,
    pub user_id: Uuid,
}

/// Try to acquire a semaphore permit for a new help-assistant session.
///
/// Returns `Ok(permit)` if capacity is available, or `Err(())` when the cap is
/// reached. The `Err` path sends a `capacity_full` error to the client.
pub async fn try_acquire(semaphore: &Arc<Semaphore>) -> Result<OwnedSemaphorePermit, ()> {
    semaphore.clone().try_acquire_owned().map_err(|_| ())
}

/// Run the full relay for one help-assistant session over an Axum WebSocket.
///
/// Called by the route handler after auth, subscription, and credit checks pass.
/// On semaphore-full, sends the `capacity_full` error and closes the WS before
/// touching OpenAI (mirrors `voice_assistant::run_relay` behavior).
pub async fn run_relay(mut browser_ws: WebSocket, deps: RelayDeps) {
    // 1. Semaphore — must acquire before contacting OpenAI.
    let permit = match try_acquire(&deps.semaphore).await {
        Ok(p) => p,
        Err(()) => {
            let _ = browser_ws
                .send(WsMessage::Text(capacity_full_error().into()))
                .await;
            let _ = browser_ws.close().await;
            tracing::warn!(org_id = %deps.org_id, "help_assistant: capacity_full");
            return;
        }
    };

    // 2. Build session.update with the static help instructions.
    let ha_voice = std::env::var("HELP_ASSISTANT_VOICE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "alloy".to_string());
    let session_update = help_assistant_client::build_ha_session_update_json(&ha_voice);

    // 3. Open OpenAI Realtime upstream.
    let (mut oa_sink, mut oa_source) = match open_ha_session(&deps.config).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(org_id = %deps.org_id, "help_assistant: upstream open failed: {e}");
            let _ = browser_ws
                .send(WsMessage::Text(
                    serde_json::json!({
                        "type": "error",
                        "code": "upstream_error",
                        "message": "Failed to connect to help assistant backend."
                    })
                    .to_string()
                    .into(),
                ))
                .await;
            let _ = browser_ws.close().await;
            return;
        }
    };

    if let Err(e) = oa_sink.send(TungMessage::text(session_update)).await {
        tracing::error!(org_id = %deps.org_id, "help_assistant: session.update failed: {e}");
        let _ = browser_ws.close().await;
        return;
    }

    tracing::info!(
        org_id = %deps.org_id,
        user_id = %deps.user_id,
        "help_assistant: session started"
    );

    // 4. Relay loop.
    let (user_transcript, ai_response, duration_s, credits_used) =
        relay_loop(&mut browser_ws, &mut oa_sink, &mut oa_source, &deps, permit).await;

    // 5. Session end — emit final summary to browser.
    let cost_display = format_cost_display(credits_used);
    let _ = browser_ws
        .send(WsMessage::Text(
            build_session_end_json(duration_s, credits_used, &cost_display).into(),
        ))
        .await;
    let _ = browser_ws.close().await;

    // 6. Persist on a background task — NO re-embed (help answers are not
    //    knowledge-base content; they are product guidance only).
    if !user_transcript.is_empty() || !ai_response.is_empty() {
        let pool = deps.pool.clone();
        let org_id = deps.org_id;
        let user_id = deps.user_id;
        let duration_secs = duration_s as i32;
        let user_txt = user_transcript.clone();
        let ai_txt = ai_response.clone();

        tokio::spawn(async move {
            persist_interaction(
                &pool,
                PersistArgs {
                    org_id,
                    user_id,
                    user_transcript: user_txt,
                    ai_response: ai_txt,
                    duration_seconds: duration_secs,
                    credits_used,
                },
            )
            .await;
        });
    }
}

/// The main bidirectional relay loop. Returns `(user_transcript, ai_response,
/// duration_seconds, credits_used)` when the session ends.
async fn relay_loop(
    browser: &mut WebSocket,
    oa_sink: &mut HaSink,
    oa_source: &mut HaSource,
    deps: &RelayDeps,
    _permit: OwnedSemaphorePermit, // dropped on return → frees semaphore slot
) -> (String, String, u64, i32) {
    let mut user_transcript = String::new();
    let mut ai_response = String::new();
    let start = Instant::now();
    let mut total_credits: i32 = 0;

    let credits_per_tick =
        credits::help_assistant_minute_credits(&deps.config) * (TICK_SECS as i32) / 60;
    let credits_per_tick = credits_per_tick.max(1);

    let mut tick = interval(Duration::from_secs(TICK_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    tick.tick().await; // consume immediate first tick

    loop {
        tokio::select! {
            // ---- Browser → server -------------------------------------------
            browser_msg = browser.recv() => {
                match browser_msg {
                    Some(Ok(WsMessage::Binary(pcm))) => {
                        let json = ha_audio_append_json(&pcm);
                        if let Err(e) = oa_sink.send(TungMessage::text(json)).await {
                            tracing::warn!(org_id = %deps.org_id, "help_assistant: oa send failed: {e}");
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Text(text))) => {
                        let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                        if v.get("type").and_then(|t| t.as_str()) == Some("stop") {
                            tracing::info!(org_id = %deps.org_id, "help_assistant: client stop received");
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        tracing::info!(org_id = %deps.org_id, "help_assistant: browser disconnected");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::warn!(org_id = %deps.org_id, "help_assistant: browser ws error: {e}");
                        break;
                    }
                }
            }

            // ---- OpenAI → browser -------------------------------------------
            oa_msg = oa_source.next() => {
                match oa_msg {
                    Some(Ok(TungMessage::Text(text))) => {
                        match crate::engine::help_assistant_client::parse_realtime_event(text.as_str()) {
                            HaEvent::AudioDelta(pcm) => {
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&pcm);
                                let msg = serde_json::json!({
                                    "type": "answer_audio",
                                    "pcm16_b64": b64,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::TextDelta(d) => {
                                ai_response.push_str(&d);
                                let msg = serde_json::json!({
                                    "type": "transcript",
                                    "role": "assistant",
                                    "delta": d,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::TranscriptDelta(d) => {
                                user_transcript.push_str(&d);
                                let msg = serde_json::json!({
                                    "type": "transcript",
                                    "role": "user",
                                    "delta": d,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::SpeechStarted => {
                                let msg = serde_json::json!({"type": "speech_started"}).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::SpeechStopped => {
                                let msg = serde_json::json!({"type": "speech_stopped"}).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::ResponseDone => {
                                let msg = serde_json::json!({"type": "response_done"}).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::Error(e) => {
                                tracing::warn!(org_id = %deps.org_id, "help_assistant: openai error: {e}");
                                let msg = serde_json::json!({
                                    "type": "error",
                                    "code": "openai_error",
                                    "message": e,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            HaEvent::Other => {}
                        }
                    }
                    Some(Ok(TungMessage::Close(_))) | None => {
                        tracing::info!(org_id = %deps.org_id, "help_assistant: openai closed upstream");
                        break;
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::warn!(org_id = %deps.org_id, "help_assistant: openai ws error: {e}");
                        break;
                    }
                }
            }

            // ---- Credit tick ------------------------------------------------
            _ = tick.tick() => {
                let elapsed_s = start.elapsed().as_secs();
                total_credits += credits_per_tick;

                match credits::deduct_org_credits(
                    &deps.pool,
                    deps.org_id,
                    credits_per_tick,
                    "help_assistant",
                    None,
                    Some(deps.user_id),
                    "help assistant session tick",
                )
                .await
                {
                    Ok(credits::OrgCharge::Insufficient { balance, required }) => {
                        tracing::warn!(
                            org_id = %deps.org_id,
                            balance,
                            required,
                            "help_assistant: credits exhausted — closing"
                        );
                        let _ = browser.send(WsMessage::Text(
                            credits_exhausted_error(balance, required).into(),
                        )).await;
                        break;
                    }
                    Ok(credits::OrgCharge::Charged { balance_after }) => {
                        let cost_display = format_cost_display(total_credits);
                        let tick_msg =
                            build_cost_tick_json(elapsed_s, total_credits, &cost_display);
                        let _ = browser.send(WsMessage::Text(tick_msg.into())).await;
                        tracing::debug!(
                            org_id = %deps.org_id,
                            elapsed_s,
                            total_credits,
                            balance_after,
                            "help_assistant: cost_tick"
                        );
                    }
                    Err(e) => {
                        tracing::error!(org_id = %deps.org_id, "help_assistant: credit deduction db error: {e}");
                        // Fail-open: log and continue.
                    }
                }
            }
        }
    }

    let duration_s = start.elapsed().as_secs();
    (user_transcript, ai_response, duration_s, total_credits)
}

struct PersistArgs {
    org_id: Uuid,
    user_id: Uuid,
    user_transcript: String,
    ai_response: String,
    duration_seconds: i32,
    credits_used: i32,
}

/// Persist a completed help-assistant interaction.
///
/// Uses `interaction_kind = 'help_assistant'` (migration 035 discriminator).
/// Unlike the voice assistant, there is NO re-embed step — help answers are
/// product guidance, not searchable knowledge-base content.
async fn persist_interaction(pool: &Pool, args: PersistArgs) {
    let PersistArgs {
        org_id,
        user_id,
        user_transcript,
        ai_response,
        duration_seconds,
        credits_used,
    } = args;

    let interaction_id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO voice_assistant_interactions
            (org_id, user_id, user_transcript, ai_response,
             duration_seconds, credits_used, interaction_kind)
         VALUES ($1, $2, $3, $4, $5, $6, 'help_assistant')
         RETURNING id",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(user_transcript.as_str())
    .bind(ai_response.as_str())
    .bind(duration_seconds)
    .bind(credits_used)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("help_assistant: persist interaction failed: {e}");
        None
    });

    if let Some(id) = interaction_id {
        tracing::info!(
            %org_id,
            interaction_id = %id,
            "help_assistant: interaction persisted"
        );
    }
}
