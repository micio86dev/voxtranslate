//! RED tests for `help_assistant_client` — written BEFORE the implementation.
//!
//! These reference `voxtranslate_server::engine::help_assistant_client` which does
//! not exist yet. They will fail to compile until task 1.6 creates the module.

use base64::Engine as _;
use serde_json::Value;
use voxtranslate_server::engine::help_assistant_client::{
    build_ha_session_update_json, build_help_instructions, ha_audio_append_json,
};

// ---- build_help_instructions -------------------------------------------------

#[test]
fn help_instructions_non_empty() {
    let instructions = build_help_instructions();
    assert!(
        !instructions.is_empty(),
        "build_help_instructions() must return a non-empty string"
    );
}

#[test]
fn help_instructions_mentions_key_features() {
    let instructions = build_help_instructions();
    // The static prompt MUST reference these core dashboard feature areas.
    let keywords = ["project", "member", "voice", "insight"];
    for kw in keywords {
        assert!(
            instructions.to_ascii_lowercase().contains(kw),
            "build_help_instructions() must mention '{}' but does not. Content: {:?}",
            kw,
            &instructions[..instructions.len().min(200)]
        );
    }
}

/// Triangulation: a second set of keywords that should also appear.
#[test]
fn help_instructions_mentions_navigation_features() {
    let instructions = build_help_instructions();
    // Additional features that should appear in the static prompt.
    let keywords = ["credit", "search"];
    for kw in keywords {
        assert!(
            instructions.to_ascii_lowercase().contains(kw),
            "build_help_instructions() must mention '{}' for completeness",
            kw,
        );
    }
}

// ---- build_ha_session_update_json -------------------------------------------

#[test]
fn ha_session_update_has_required_fields() {
    let json_str = build_ha_session_update_json("alloy");
    let v: Value = serde_json::from_str(&json_str).expect("valid JSON");
    assert_eq!(v["type"], "session.update");
    let sess = &v["session"];
    assert_eq!(sess["type"], "realtime");
    let mods = sess["output_modalities"]
        .as_array()
        .expect("output_modalities array");
    let mods: Vec<&str> = mods.iter().filter_map(|m| m.as_str()).collect();
    assert!(
        mods.contains(&"audio"),
        "output_modalities must contain audio"
    );
    assert_eq!(sess["audio"]["output"]["voice"], "alloy");
    assert_eq!(sess["audio"]["input"]["format"]["type"], "audio/pcm");
    assert_eq!(sess["audio"]["input"]["format"]["rate"], 24000);
    assert_eq!(
        sess["audio"]["input"]["transcription"]["model"],
        "gpt-realtime-whisper"
    );
    assert_eq!(sess["audio"]["output"]["format"]["type"], "audio/pcm");
    assert_eq!(sess["audio"]["output"]["format"]["rate"], 24000);
    assert_eq!(
        sess["audio"]["input"]["turn_detection"]["type"],
        "semantic_vad"
    );
    // instructions must be the static help prompt (non-empty).
    let instr = sess["instructions"].as_str().expect("instructions string");
    assert!(
        !instr.is_empty(),
        "instructions in session.update must not be empty"
    );
}

/// Triangulation: different voice is reflected.
#[test]
fn ha_session_update_reflects_voice_param() {
    let json_str = build_ha_session_update_json("echo");
    let v: Value = serde_json::from_str(&json_str).expect("valid JSON");
    assert_eq!(v["session"]["audio"]["output"]["voice"], "echo");
}

// ---- ha_audio_append_json ---------------------------------------------------

#[test]
fn ha_audio_append_encodes_correctly() {
    let pcm: Vec<u8> = vec![10, 20, 30];
    let json_str = ha_audio_append_json(&pcm);
    let v: Value = serde_json::from_str(&json_str).expect("valid JSON");
    assert_eq!(v["type"], "input_audio_buffer.append");
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(v["audio"].as_str().expect("audio field"))
        .expect("valid base64");
    assert_eq!(decoded, pcm, "decoded PCM must match input");
}
