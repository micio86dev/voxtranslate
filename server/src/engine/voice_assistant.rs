//! B2B Voice Assistant relay engine.
//!
//! Bridges a dashboard WebSocket client to OpenAI Realtime (`v1/realtime`) for
//! full-duplex voice Q&A grounded by RAG over the org's transcript embeddings.
//!
//! ## Session lifecycle
//!
//! 1. Acquire a semaphore permit (capacity cap from [`VoiceAssistantConfig::max_sessions`]).
//! 2. Embed a context query → KNN over `transcript_embeddings` → inject into `session.update`.
//! 3. Open the upstream OpenAI Realtime WS and send `session.update`.
//! 4. Bidirectional relay loop:
//!    - Browser binary frames (PCM16) → `input_audio_buffer.append` to OpenAI.
//!    - Browser text `{"type":"stop"}` → close upstream and begin flush.
//!    - OpenAI events → forwarded as text frames to the browser.
//!    - Every 10 s: emit `cost_tick` to browser; deduct credits.
//! 5. On close: deduct final credits, persist interaction, best-effort re-embed.
//!
//! ## Cost transparency
//!
//! Every 10 seconds the server emits:
//! ```json
//! { "type": "cost_tick", "duration_s": 10, "credits_so_far": 38, "cost_display": "$0.38" }
//! ```
//! On session close:
//! ```json
//! { "type": "session_end", "duration_s": 72, "credits_used": 76, "cost_display": "$0.76" }
//! ```
//!
//! If credits run out mid-session, the relay closes with:
//! ```json
//! { "type": "error", "code": "credits_exhausted", "message": "..." }
//! ```

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocket};
use base64::Engine as _;
use futures::{SinkExt as _, StreamExt as _};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tokio::time::{interval, Instant, MissedTickBehavior};
use uuid::Uuid;

use crate::business::credits;
use crate::config::VoiceAssistantConfig;
use crate::db::Pool;
use crate::embeddings::OpenAiEmbeddings;
use crate::engine::voice_assistant_client::{
    self, build_cost_tick_json, build_session_end_json, format_cost_display, open_va_session,
    parse_realtime_event, va_audio_append_json, VaEvent, VaSink, VaSource,
};
use tokio_tungstenite::tungstenite::Message as TungMessage;

/// How often credits are deducted and a `cost_tick` is emitted to the client.
const TICK_SECS: u64 = 10;

/// Error JSON sent when the semaphore is full.
pub fn capacity_full_error() -> String {
    serde_json::json!({
        "type": "error",
        "code": "capacity_full",
        "message": "Voice assistant is at capacity. Please try again in a moment.",
    })
    .to_string()
}

/// Error JSON sent when org credits are exhausted.
fn credits_exhausted_error(balance: i32, required: i32) -> String {
    serde_json::json!({
        "type": "error",
        "code": "credits_exhausted",
        "message": "Insufficient credits to continue the voice assistant session.",
        "balance": balance,
        "required": required,
    })
    .to_string()
}

/// Dependencies injected by the route handler into the relay.
pub struct RelayDeps {
    pub config: VoiceAssistantConfig,
    pub semaphore: Arc<Semaphore>,
    pub pool: Pool,
    pub embedder: OpenAiEmbeddings,
    pub org_id: Uuid,
    pub user_id: Uuid,
    pub project_id: Option<Uuid>,
    pub member_id: Option<Uuid>,
    pub org_name: String,
    pub project_name: Option<String>,
    pub member_name: Option<String>,
    /// Top-K RAG chunks retrieved before the session starts. Empty means no
    /// prior transcriptions — the assistant will say so.
    pub rag_chunks: Vec<String>,
}

/// Try to acquire a semaphore permit for a new voice-assistant session.
///
/// Returns `Some(permit)` if capacity is available, `None` when the cap is
/// reached — the caller then sends a `capacity_full` error to the client.
///
/// `Option`, not `Result<_, ()>`: there is exactly one way this fails and it
/// carries no information, which is what an `Option` says and a unit error only
/// implies. Callers already matched on it as presence/absence.
pub async fn try_acquire(semaphore: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    semaphore.clone().try_acquire_owned().ok()
}

/// Run the full relay for one voice-assistant session over an Axum WebSocket.
///
/// Called by the route handler after auth, role, and credit checks pass.
/// On semaphore-full, sends the `capacity_full` error and closes the WS before
/// touching OpenAI (product decision 5).
pub async fn run_relay(mut browser_ws: WebSocket, deps: RelayDeps) {
    // 1. Semaphore — must acquire before contacting OpenAI.
    let permit = match try_acquire(&deps.semaphore).await {
        Some(p) => p,
        None => {
            let _ = browser_ws
                .send(WsMessage::Text(capacity_full_error().into()))
                .await;
            let _ = browser_ws.close().await;
            tracing::warn!(org_id = %deps.org_id, "voice_assistant: capacity_full");
            return;
        }
    };

    // 2. Build RAG-grounded instructions (already retrieved by the route handler).
    let instructions = voice_assistant_client::build_rag_instructions(
        &deps.rag_chunks,
        &deps.org_name,
        deps.project_name.as_deref(),
        deps.member_name.as_deref(),
    );

    // 3. Open OpenAI Realtime upstream.
    let va_voice = std::env::var("VOICE_ASSISTANT_VOICE")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "alloy".to_string());
    let session_update =
        voice_assistant_client::build_session_update_json(&instructions, &va_voice);

    let (mut oa_sink, mut oa_source) = match open_va_session(&deps.config).await {
        Ok(pair) => pair,
        Err(e) => {
            tracing::error!(org_id = %deps.org_id, "voice_assistant: upstream open failed: {e}");
            let _ = browser_ws
                .send(WsMessage::Text(
                    serde_json::json!({
                        "type": "error",
                        "code": "upstream_error",
                        "message": "Failed to connect to voice assistant backend."
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
        tracing::error!(org_id = %deps.org_id, "voice_assistant: session.update failed: {e}");
        let _ = browser_ws.close().await;
        return;
    }

    tracing::info!(
        org_id = %deps.org_id,
        user_id = %deps.user_id,
        project_id = ?deps.project_id,
        "voice_assistant: session started"
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

    // 6. Persist + re-embed on a background task (non-blocking, best-effort).
    if !user_transcript.is_empty() || !ai_response.is_empty() {
        let pool = deps.pool.clone();
        let embedder = deps.embedder.clone();
        let org_id = deps.org_id;
        let user_id = deps.user_id;
        let project_id = deps.project_id;
        let member_id = deps.member_id;
        let duration_secs = duration_s as i32;
        let user_txt = user_transcript.clone();
        let ai_txt = ai_response.clone();

        tokio::spawn(async move {
            persist_and_reindex(
                &pool,
                &embedder,
                PersistArgs {
                    org_id,
                    user_id,
                    project_id,
                    member_id,
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
    oa_sink: &mut VaSink,
    oa_source: &mut VaSource,
    deps: &RelayDeps,
    _permit: OwnedSemaphorePermit, // dropped on return → frees semaphore slot
) -> (String, String, u64, i32) {
    let mut user_transcript = String::new();
    let mut ai_response = String::new();
    let start = Instant::now();
    let mut total_credits: i32 = 0;

    // Bill the real price per tick, carrying the sub-credit remainder.
    //
    // This used to be `ceil_minute_credits * TICK_SECS / 60` in INTEGER
    // arithmetic, floored at 1 to stop it reaching zero — which treated the
    // symptom. The truncation threw away the ceiling and then some: a 10s tick
    // charged `38 * 10 / 60 = 6` credits, i.e. 36 a minute against the 37.5 the
    // session costs. Neither rounding a whole minute up nor truncating a tick
    // down is right — accumulate and hand over whole credits as they accrue
    // (see `CreditAccumulator`).
    let usd_per_tick = credits::minute_price_usd(deps.config.cost_per_minute, deps.config.markup)
        * rust_decimal::Decimal::from(TICK_SECS)
        / rust_decimal::Decimal::from(60u64);
    let mut credit_meter = credits::CreditAccumulator::default();

    let mut tick = interval(Duration::from_secs(TICK_SECS));
    tick.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // Consume the first tick (fires immediately) — we don't charge at t=0.
    tick.tick().await;

    loop {
        tokio::select! {
            // ---- Browser → server -------------------------------------------
            browser_msg = browser.recv() => {
                match browser_msg {
                    Some(Ok(WsMessage::Binary(pcm))) => {
                        // Forward raw PCM16 to OpenAI as input_audio_buffer.append.
                        let json = va_audio_append_json(&pcm);
                        if let Err(e) = oa_sink.send(TungMessage::text(json)).await {
                            tracing::warn!(org_id = %deps.org_id, "voice_assistant: oa send failed: {e}");
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Text(text))) => {
                        // Control message from browser ({"type":"stop"} etc.)
                        let v: serde_json::Value = serde_json::from_str(text.as_str()).unwrap_or_default();
                        if v.get("type").and_then(|t| t.as_str()) == Some("stop") {
                            tracing::info!(org_id = %deps.org_id, "voice_assistant: client stop received");
                            break;
                        }
                    }
                    Some(Ok(WsMessage::Close(_))) | None => {
                        tracing::info!(org_id = %deps.org_id, "voice_assistant: browser disconnected");
                        break;
                    }
                    Some(Ok(_)) => {} // ping/pong etc.
                    Some(Err(e)) => {
                        tracing::warn!(org_id = %deps.org_id, "voice_assistant: browser ws error: {e}");
                        break;
                    }
                }
            }

            // ---- OpenAI → browser -------------------------------------------
            oa_msg = oa_source.next() => {
                match oa_msg {
                    Some(Ok(TungMessage::Text(text))) => {
                        match parse_realtime_event(text.as_str()) {
                            VaEvent::AudioDelta(pcm) => {
                                // Forward audio to browser as base64-encoded text frame.
                                let b64 = base64::engine::general_purpose::STANDARD.encode(&pcm);
                                let msg = serde_json::json!({
                                    "type": "answer_audio",
                                    "pcm16_b64": b64,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::TextDelta(d) => {
                                ai_response.push_str(&d);
                                let msg = serde_json::json!({
                                    "type": "transcript",
                                    "role": "assistant",
                                    "delta": d,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::TranscriptDelta(d) => {
                                // Input transcription delta (user's words).
                                user_transcript.push_str(&d);
                                let msg = serde_json::json!({
                                    "type": "transcript",
                                    "role": "user",
                                    "delta": d,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::SpeechStarted => {
                                let msg = serde_json::json!({"type": "speech_started"}).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::SpeechStopped => {
                                let msg = serde_json::json!({"type": "speech_stopped"}).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::ResponseDone => {
                                let msg = serde_json::json!({"type": "response_done"}).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::Error(e) => {
                                tracing::warn!(org_id = %deps.org_id, "voice_assistant: openai error: {e}");
                                let msg = serde_json::json!({
                                    "type": "error",
                                    "code": "openai_error",
                                    "message": e,
                                }).to_string();
                                let _ = browser.send(WsMessage::Text(msg.into())).await;
                            }
                            VaEvent::Other => {}
                        }
                    }
                    Some(Ok(TungMessage::Close(_))) | None => {
                        tracing::info!(org_id = %deps.org_id, "voice_assistant: openai closed upstream");
                        break;
                    }
                    Some(Ok(_)) => {} // ping/pong/binary
                    Some(Err(e)) => {
                        tracing::warn!(org_id = %deps.org_id, "voice_assistant: openai ws error: {e}");
                        break;
                    }
                }
            }

            // ---- Credit tick ------------------------------------------------
            _ = tick.tick() => {
                let elapsed_s = start.elapsed().as_secs();
                let credits_per_tick = credit_meter.take(usd_per_tick);
                if credits_per_tick == 0 {
                    continue; // still under a cent; it rolls into the next tick
                }
                total_credits += credits_per_tick;

                // Deduct from org pool.
                match credits::deduct_org_credits(
                    &deps.pool,
                    deps.org_id,
                    credits_per_tick,
                    "voice_assistant",
                    None,
                    Some(deps.user_id),
                    "voice assistant session tick",
                )
                .await
                {
                    Ok(credits::OrgCharge::Insufficient { balance, required }) => {
                        tracing::warn!(
                            org_id = %deps.org_id,
                            balance,
                            required,
                            "voice_assistant: credits exhausted — closing"
                        );
                        let _ = browser.send(WsMessage::Text(
                            credits_exhausted_error(balance, required).into(),
                        )).await;
                        break;
                    }
                    Ok(credits::OrgCharge::Charged { balance_after }) => {
                        // Emit cost_tick to browser.
                        let cost_display = format_cost_display(total_credits);
                        let tick_msg =
                            build_cost_tick_json(elapsed_s, total_credits, &cost_display);
                        let _ = browser.send(WsMessage::Text(tick_msg.into())).await;
                        tracing::debug!(
                            org_id = %deps.org_id,
                            elapsed_s,
                            total_credits,
                            balance_after,
                            "voice_assistant: cost_tick"
                        );
                    }
                    Err(e) => {
                        tracing::error!(org_id = %deps.org_id, "voice_assistant: credit deduction db error: {e}");
                        // Fail-open: log and continue rather than killing the session on a db hiccup.
                    }
                }
            }
        }
    }

    let duration_s = start.elapsed().as_secs();
    (user_transcript, ai_response, duration_s, total_credits)
}

/// Arguments for `persist_and_reindex` — grouped to stay under clippy's argument limit.
struct PersistArgs {
    org_id: Uuid,
    user_id: Uuid,
    project_id: Option<Uuid>,
    member_id: Option<Uuid>,
    user_transcript: String,
    ai_response: String,
    duration_seconds: i32,
    credits_used: i32,
}

/// Persist a completed interaction and best-effort re-embed both sides.
///
/// Called from a spawned background task after the relay loop ends.
/// A failure here is logged but NEVER propagates to the session close path
/// (product decision 3 — re-indexing failures must not surface as session errors).
async fn persist_and_reindex(pool: &Pool, embedder: &OpenAiEmbeddings, args: PersistArgs) {
    let PersistArgs {
        org_id,
        user_id,
        project_id,
        member_id,
        user_transcript,
        ai_response,
        duration_seconds,
        credits_used,
    } = args;
    // Insert the interaction row. `interaction_kind = 'project_qa'` is written
    // explicitly so this insert is unambiguous after migration 035 adds the column.
    let interaction_id: Option<Uuid> = sqlx::query_scalar(
        "INSERT INTO voice_assistant_interactions
            (org_id, user_id, project_id, member_id, user_transcript, ai_response,
             duration_seconds, credits_used, interaction_kind)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, 'project_qa')
         RETURNING id",
    )
    .bind(org_id)
    .bind(user_id)
    .bind(project_id)
    .bind(member_id)
    .bind(user_transcript.as_str())
    .bind(ai_response.as_str())
    .bind(duration_seconds)
    .bind(credits_used)
    .fetch_optional(pool)
    .await
    .unwrap_or_else(|e| {
        tracing::warn!("voice_assistant: persist interaction failed: {e}");
        None
    });

    let Some(interaction_id) = interaction_id else {
        return;
    };

    tracing::info!(
        %org_id,
        %interaction_id,
        "voice_assistant: interaction persisted"
    );

    // Re-embed user_transcript and ai_response into transcript_embeddings
    // with transcript_id = NULL and interaction_id = this interaction's id.
    let texts_to_embed: Vec<(String, &str)> =
        [(user_transcript, "user"), (ai_response, "assistant")]
            .into_iter()
            .filter(|(t, _)| !t.trim().is_empty())
            .collect();

    for (chunk_index, (text, role)) in texts_to_embed.into_iter().enumerate() {
        match embedder.embed_one(&text).await {
            Ok(vec) => {
                let qvec = pgvector::Vector::from(vec);
                // session_id omitted (assistant interactions have no call session →
                // nullable since migration 036); chunk_index is 0=user, 1=assistant.
                if let Err(e) = sqlx::query(
                    "INSERT INTO transcript_embeddings
                        (org_id, project_id, transcript_id, interaction_id,
                         chunk_index, content, speaker_name, embedding)
                     VALUES ($1, $2, NULL, $3, $4, $5, $6, $7)",
                )
                .bind(org_id)
                .bind(project_id)
                .bind(interaction_id)
                .bind(chunk_index as i32)
                .bind(text)
                .bind(role) // speaker_name = "user" or "assistant"
                .bind(qvec)
                .execute(pool)
                .await
                {
                    tracing::warn!(
                        %org_id,
                        %interaction_id,
                        role,
                        "voice_assistant: re-embed insert failed: {e}"
                    );
                }
            }
            Err(e) => {
                tracing::warn!(
                    %org_id,
                    %interaction_id,
                    role,
                    "voice_assistant: embed failed: {e}"
                );
            }
        }
    }
}
