use crate::app_state::AppState;
use crate::proxy::ThinkingConfig;
use actix_web::{error::*, Error};
use awc::Client;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

const FALLBACK_THOUGHT_SIGNATURE: &str = "skip_thought_signature_validator";

pub struct StreamTransformerState {
    pub stream_id: String,
    pub created: u64,
    pub model: String,
    pub role_sent: bool,
    pub reasoning_sent: bool,
    pub prev_is_thought: bool,
    pub no_thought_process: bool,
    pub md_thought: bool,
    pub next_tool_call_index: usize,
    pub emitted_tool_call_ids: Vec<String>,
    pub saw_tool_calls: bool,
    pub finish_reason: Option<String>,
    pub final_usage: Option<Value>,
    pub accumulated_thought: String,
    pub accumulated_answer: String,
    pub latest_thought_signature: Option<String>,
}

impl StreamTransformerState {
    pub fn new(initial_model: String, no_thought_process: bool, md_thought: bool) -> Self {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        Self {
            stream_id: format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
            created: now,
            model: initial_model,
            role_sent: false,
            reasoning_sent: false,
            prev_is_thought: false,
            no_thought_process,
            md_thought,
            next_tool_call_index: 0,
            emitted_tool_call_ids: Vec::new(),
            saw_tool_calls: false,
            finish_reason: None,
            final_usage: None,
            accumulated_thought: String::new(),
            accumulated_answer: String::new(),
            latest_thought_signature: None,
        }
    }
}

pub fn canonicalize_json(val: &Value) -> Value {
    match val {
        Value::Object(map) => {
            let mut sorted_keys: Vec<&String> = map.keys().collect();
            sorted_keys.sort();
            let mut clean = Map::new();
            for k in sorted_keys {
                if let Some(v) = map.get(k) {
                    clean.insert(k.clone(), canonicalize_json(v));
                }
            }
            Value::Object(clean)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_json).collect()),
        other => other.clone(),
    }
}

fn decode_base64_and_get_mime_type(encoded_data: &str) -> Result<(String, Vec<u8>), Error> {
    if !encoded_data.starts_with("data:") {
        return Err(ErrorBadRequest("Invalid data URI"));
    }

    let comma_pos = encoded_data
        .find(',')
        .ok_or_else(|| ErrorBadRequest("Invalid data URI format"))?;

    let metadata = &encoded_data[5..comma_pos];
    let base64_data = &encoded_data[comma_pos + 1..];

    let mut parts = metadata.split(';');
    let mime_type = parts
        .next()
        .ok_or_else(|| ErrorBadRequest("Missing MIME type"))?
        .to_string();
    let encoding = parts
        .next()
        .ok_or_else(|| ErrorBadRequest("Missing encoding"))?;

    if encoding != "base64" {
        return Err(ErrorBadRequest("Unsupported encoding format"));
    }

    let decoded_data = STANDARD
        .decode(base64_data)
        .map_err(|_| ErrorBadRequest("Failed to decode Base64 data"))?;

    Ok((mime_type, decoded_data))
}

/// Proactively strips unsupported `generationConfig` fields for models that reject them with 400 INVALID_ARGUMENT.
pub fn apply_generation_config_ignore_filter(generation_config: &mut Value, model_name: &str) {
    let Some(cfg) = generation_config.as_object_mut() else {
        return;
    };

    let lower = model_name.to_ascii_lowercase();

    let is_gemini_3_or_4 = lower.contains("gemini-3") || lower.contains("gemini-4");
    let is_thinking_model = lower.contains("thinking");
    let is_image_model = lower.contains("-image") || lower.contains("banana");
    let is_tts_model = lower.contains("-tts");
    let is_transcribe_model = lower.contains("-transcribe");
    let is_gemma_or_learnlm = lower.contains("gemma") || lower.contains("learnlm");

    // 1. Models that reject `presencePenalty` / `frequencyPenalty` ("Penalty is not enabled for this model")
    if is_gemini_3_or_4
        || is_thinking_model
        || is_image_model
        || is_tts_model
        || is_transcribe_model
        || is_gemma_or_learnlm
    {
        if cfg.remove("presencePenalty").is_some() || cfg.remove("frequencyPenalty").is_some() {
            log::debug!(
                "Ignore filter: stripped presencePenalty/frequencyPenalty for model '{model_name}'"
            );
        }
    }

    // 2. Models that reject `responseMimeType` / `responseSchema` ("JSON mode is not enabled for this model")
    //    and `thinkingConfig` ("Thinking is not enabled for this model")
    if is_image_model || is_tts_model || is_transcribe_model {
        if cfg.remove("responseMimeType").is_some() || cfg.remove("responseSchema").is_some() {
            log::debug!(
                "Ignore filter: stripped responseMimeType/responseSchema for model '{model_name}'"
            );
        }
        if cfg.remove("thinkingConfig").is_some() {
            log::debug!("Ignore filter: stripped thinkingConfig for model '{model_name}'");
        }
    }

    // 3. TTS models only accept AUDIO modality + speechConfig and reject stopSequences / candidateCount > 1
    if is_tts_model {
        cfg.remove("stopSequences");
        cfg.remove("candidateCount");
        cfg.remove("topK");
    }
}

async fn upload_to_google(
    client: &Client,
    api_key: &str,
    metadata: Value,
    file_data: Vec<u8>,
    mime_type: &str,
) -> Result<String, Error> {
    let upload_url =
        format!("https://generativelanguage.googleapis.com/upload/v1beta/files?key={api_key}");

    let meta_response = client
        .post(&upload_url)
        .insert_header(("X-Goog-Upload-Protocol", "resumable"))
        .insert_header(("X-Goog-Upload-Command", "start"))
        .insert_header((
            "X-Goog-Upload-Header-Content-Length",
            file_data.len().to_string(),
        ))
        .insert_header(("X-Goog-Upload-Header-Content-Type", mime_type))
        .insert_header(("Content-Type", "application/json"))
        .send_body(metadata.to_string())
        .await
        .map_err(|_| ErrorInternalServerError("Failed to initiate upload"))?;

    let upload_url = meta_response
        .headers()
        .get("X-Goog-Upload-URL")
        .and_then(|h| h.to_str().ok())
        .ok_or(ErrorInternalServerError("Failed to get upload URL"))?;

    let response = client
        .post(upload_url)
        .insert_header(("Content-Length", file_data.len().to_string()))
        .insert_header(("X-Goog-Upload-Offset", "0"))
        .insert_header(("X-Goog-Upload-Command", "upload, finalize"))
        .send_body(file_data)
        .await
        .map_err(|_| ErrorInternalServerError("Failed to upload file"))?
        .json::<Value>()
        .await
        .map_err(|_| ErrorInternalServerError("Failed to parse JSON response"))?;

    let file_uri = response["file"]["uri"]
        .as_str()
        .ok_or(ErrorInternalServerError("File URI not returned"))?;
    Ok(file_uri.to_string())
}

pub async fn upload_base64_image_to_google(
    client: &Client,
    api_key: &str,
    base64_data: &str,
    app_state: Option<&AppState>,
) -> Result<(String, String), Error> {
    if let Some(st) = app_state {
        if let Some(cached) = st.get_cached_file(base64_data) {
            log::debug!("Reusing cached image file_uri: {}", cached.1);
            return Ok(cached);
        }
    }
    let (mime_type, image_data) = decode_base64_and_get_mime_type(base64_data)?;
    let metadata = json!({"file": {"display_name": "uploaded_image"}});
    let uri = upload_to_google(client, api_key, metadata, image_data, &mime_type).await?;
    if let Some(st) = app_state {
        st.record_cached_file(base64_data, mime_type.clone(), uri.clone());
    }
    Ok((mime_type, uri))
}

pub async fn upload_base64_audio_to_google(
    client: &Client,
    api_key: &str,
    base64_data: &str,
    app_state: Option<&AppState>,
) -> Result<(String, String), Error> {
    if let Some(st) = app_state {
        if let Some(cached) = st.get_cached_file(base64_data) {
            log::debug!("Reusing cached audio file_uri: {}", cached.1);
            return Ok(cached);
        }
    }
    let (mime_type, audio_data) = decode_base64_and_get_mime_type(base64_data)?;
    let metadata = json!({"file": {"display_name": "uploaded_audio"}});
    let uri = upload_to_google(client, api_key, metadata, audio_data, &mime_type).await?;
    if let Some(st) = app_state {
        st.record_cached_file(base64_data, mime_type.clone(), uri.clone());
    }
    Ok((mime_type, uri))
}

pub fn map_openai_voice(voice: &str) -> &str {
    match voice {
        "alloy" => "Puck",
        "echo" => "Charon",
        "fable" => "Fenrir",
        "onyx" => "Orus",
        "nova" => "Kore",
        "shimmer" => "Aoede",
        "coral" => "Leda",
        "sage" => "Zephyr",
        other => other,
    }
}

pub fn size_to_aspect_ratio(size: &str) -> String {
    match size {
        "1024x1024" | "512x512" | "256x256" | "1:1" => "1:1".to_string(),
        "1792x1024" | "1536x1024" | "16:9" => "16:9".to_string(),
        "1024x1792" | "1024x1536" | "9:16" => "9:16".to_string(),
        "4:3" => "4:3".to_string(),
        "3:4" => "3:4".to_string(),
        other if other.contains(':') => other.to_string(),
        _ => "1:1".to_string(),
    }
}

pub fn wrap_pcm_in_wav(
    pcm_data: &[u8],
    sample_rate: u32,
    channels: u16,
    bits_per_sample: u16,
) -> Vec<u8> {
    let byte_rate = sample_rate * u32::from(channels) * u32::from(bits_per_sample) / 8;
    let block_align = channels * bits_per_sample / 8;
    let data_len = pcm_data.len() as u32;
    let riff_len = 36 + data_len;

    let mut wav = Vec::with_capacity(44 + pcm_data.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&riff_len.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes());
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    wav.extend_from_slice(pcm_data);
    wav
}

pub fn extract_images_from_google_response(
    body: &Value,
) -> Vec<(String, String, Option<String>)> {
    let mut results = Vec::new();
    let mut accompanying_text: Option<String> = None;

    if let Some(candidates) = body.get("candidates").and_then(Value::as_array) {
        for candidate in candidates {
            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for part in parts {
                    let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
                    if is_thought {
                        continue;
                    }
                    if let Some(t) = part.get("text").and_then(Value::as_str) {
                        accompanying_text = Some(t.to_string());
                    }
                    if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data"))
                    {
                        let mime = inline
                            .get("mimeType")
                            .or_else(|| inline.get("mime_type"))
                            .and_then(Value::as_str)
                            .unwrap_or("image/png");
                        if mime.starts_with("image/") {
                            if let Some(b64) = inline.get("data").and_then(Value::as_str) {
                                results.push((
                                    mime.to_string(),
                                    b64.to_string(),
                                    accompanying_text.clone(),
                                ));
                            }
                        }
                    }
                }
            }
        }
    }
    results
}

#[allow(non_snake_case)]
pub fn extract_PCM_or_audio_from_google(body: &Value) -> Option<(String, Vec<u8>)> {
    let candidates = body.get("candidates")?.as_array()?;
    for candidate in candidates {
        if let Some(parts) = candidate.pointer("/content/parts").and_then(Value::as_array) {
            for part in parts {
                if let Some(inline) = part.get("inlineData").or_else(|| part.get("inline_data")) {
                    let mime = inline
                        .get("mimeType")
                        .or_else(|| inline.get("mime_type"))
                        .and_then(Value::as_str)
                        .unwrap_or("audio/L16");
                    if mime.starts_with("audio/") {
                        if let Some(b64) = inline.get("data").and_then(Value::as_str) {
                            if let Ok(bytes) = STANDARD.decode(b64) {
                                return Some((mime.to_string(), bytes));
                            }
                        }
                    }
                }
            }
        }
    }
    None
}

fn build_usage_json(usage_metadata: &Value) -> Value {
    let raw_prompt_tokens = usage_metadata
        .get("promptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let tool_use_prompt_tokens = usage_metadata
        .get("toolUsePromptTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let prompt_tokens = raw_prompt_tokens + tool_use_prompt_tokens;

    let cached_tokens = usage_metadata
        .get("cachedContentTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let uncached_tokens = (prompt_tokens - cached_tokens).max(0);

    let candidates_tokens = usage_metadata
        .get("candidatesTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let thoughts_tokens = usage_metadata
        .get("thoughtsTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let completion_tokens = candidates_tokens + thoughts_tokens;

    let total_tokens = usage_metadata
        .get("totalTokenCount")
        .and_then(Value::as_i64)
        .unwrap_or(prompt_tokens + completion_tokens);

    let hit_rate = if prompt_tokens > 0 {
        (cached_tokens as f64 / prompt_tokens as f64) * 100.0
    } else {
        0.0
    };

    log::info!(
        "Token Usage: total={} | input={} (cache_hit={} [{:.1}%], cache_miss={}) | output={} (text={}, thought={})",
        total_tokens,
        prompt_tokens,
        cached_tokens,
        hit_rate,
        uncached_tokens,
        completion_tokens,
        candidates_tokens,
        thoughts_tokens
    );

    json!({
        "prompt_tokens": prompt_tokens,
        "completion_tokens": completion_tokens,
        "total_tokens": total_tokens,
        "prompt_cache_hit_tokens": cached_tokens,
        "prompt_cache_miss_tokens": uncached_tokens,
        "cached_tokens": cached_tokens,
        "cache_read_input_tokens": cached_tokens,
        "prompt_tokens_details": {
            "cached_tokens": cached_tokens,
            "uncached_tokens": uncached_tokens,
            "audio_tokens": 0
        },
        "completion_tokens_details": {
            "reasoning_tokens": thoughts_tokens,
            "text_tokens": candidates_tokens,
            "accepted_prediction_tokens": 0,
            "rejected_prediction_tokens": 0
        }
    })
}

/// Stateful SSE event processor for a single Google `data: {...}` block
pub fn transform_google_sse_event(
    event_block: &str,
    state: &mut StreamTransformerState,
    app_state: Option<&AppState>,
) -> Option<String> {
    let trimmed = event_block.trim();
    if trimmed.is_empty() || trimmed == "data: [DONE]" {
        return None;
    }

    let json_payload: String = trimmed
        .lines()
        .filter_map(|line| {
            let line = line.trim_start();
            line.strip_prefix("data:").map(|s| s.trim())
        })
        .filter(|s| !s.is_empty() && *s != "[DONE]")
        .collect::<Vec<&str>>()
        .join("\n");

    if json_payload.is_empty() {
        return None;
    }

    let body: Value = match serde_json::from_str(&json_payload) {
        Ok(v) => v,
        Err(e) => {
            log::error!("Failed to parse SSE JSON chunk: {e}, raw: {json_payload}");
            return None;
        }
    };

    if let Some(mv) = body.get("modelVersion").and_then(Value::as_str) {
        if !mv.is_empty() {
            state.model = if state.no_thought_process {
                format!("{mv}-no-thought-process")
            } else {
                mv.to_string()
            };
        }
    }

    if let Some(usage_metadata) = body.get("usageMetadata") {
        state.final_usage = Some(build_usage_json(usage_metadata));
    }

    let mut out_sse = String::new();

    if let Some(candidates) = body.get("candidates").and_then(Value::as_array) {
        for candidate in candidates {
            if let Some(fr) = candidate.get("finishReason").and_then(Value::as_str) {
                let mapped = match fr {
                    "STOP" => "stop".to_string(),
                    "MAX_TOKENS" => "length".to_string(),
                    other => other.to_lowercase(),
                };
                state.finish_reason = Some(mapped);
            }

            if let Some(parts) = candidate
                .pointer("/content/parts")
                .and_then(Value::as_array)
            {
                for p in parts {
                    if let Some(sig) = p
                        .get("thoughtSignature")
                        .or_else(|| p.get("thought_signature"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                    {
                        state.latest_thought_signature = Some(sig.to_string());
                    }
                }

                let mut thought_chunk_str = String::new();
                let mut answer_chunk_str = String::new();
                let mut md_segments: Vec<(String, bool)> = Vec::new();
                let mut tool_calls: Vec<Value> = Vec::new();
                let mut audio_output: Option<Value> = None;

                for part in parts {
                    let is_thought = part.get("thought").and_then(Value::as_bool) == Some(true);
                    let part_sig = part
                        .get("thoughtSignature")
                        .or_else(|| part.get("thought_signature"))
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string())
                        .or_else(|| state.latest_thought_signature.clone());

                    if let Some(text) = part.get("text").and_then(Value::as_str) {
                        if !text.is_empty() {
                            if is_thought {
                                state.accumulated_thought.push_str(text);
                                if !state.no_thought_process {
                                    thought_chunk_str.push_str(text);
                                    md_segments.push((text.to_string(), true));
                                }
                            } else {
                                state.accumulated_answer.push_str(text);
                                answer_chunk_str.push_str(text);
                                md_segments.push((text.to_string(), false));
                            }
                        }
                    }

                    if let Some(fc) = part
                        .get("functionCall")
                        .or_else(|| part.get("function_call"))
                    {
                        if let Some(name) = fc.get("name").and_then(Value::as_str) {
                            let raw_args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                            let canonical_args_val = if raw_args.is_string() {
                                serde_json::from_str::<Value>(raw_args.as_str().unwrap_or("{}"))
                                    .map(|v| canonicalize_json(&v))
                                    .unwrap_or_else(|_| json!({}))
                            } else {
                                canonicalize_json(&raw_args)
                            };
                            let args_str = serde_json::to_string(&canonical_args_val)
                                .unwrap_or_else(|_| "{}".to_string());

                            let call_id = format!("call_{}", uuid::Uuid::new_v4().simple());
                            let is_first_fc_in_turn = state.emitted_tool_call_ids.is_empty();
                            let sig_for_this_fc = if is_first_fc_in_turn {
                                part_sig.clone()
                            } else {
                                None
                            };
                            let thought_for_this_fc = if is_first_fc_in_turn
                                && !state.accumulated_thought.is_empty()
                            {
                                Some(state.accumulated_thought.clone())
                            } else {
                                None
                            };

                            if let Some(st) = app_state {
                                st.record_tool_call(
                                    call_id.clone(),
                                    name.to_string(),
                                    args_str.clone(),
                                    sig_for_this_fc.clone(),
                                    thought_for_this_fc,
                                );
                            }
                            state.emitted_tool_call_ids.push(call_id.clone());
                            state.saw_tool_calls = true;

                            let tc_index = state.next_tool_call_index;
                            state.next_tool_call_index += 1;

                            let mut tc_obj = json!({
                                "index": tc_index,
                                "id": call_id,
                                "type": "function",
                                "function": {
                                    "name": name,
                                    "arguments": args_str
                                }
                            });

                            if let Some(sig) = sig_for_this_fc {
                                tc_obj["extra_content"] = json!({
                                    "google": { "thought_signature": sig }
                                });
                            }

                            tool_calls.push(tc_obj);
                        }
                    }

                    if let Some(inline) =
                        part.get("inlineData").or_else(|| part.get("inline_data"))
                    {
                        let mime = inline
                            .get("mimeType")
                            .or_else(|| inline.get("mime_type"))
                            .and_then(Value::as_str)
                            .unwrap_or("application/octet-stream");
                        if let Some(b64_data) = inline.get("data").and_then(Value::as_str) {
                            if mime.starts_with("image/") {
                                if !(state.no_thought_process && is_thought) {
                                    let md_img =
                                        format!("\n![image](data:{mime};base64,{b64_data})\n");
                                    if is_thought {
                                        thought_chunk_str.push_str(&md_img);
                                    } else {
                                        state.accumulated_answer.push_str(&md_img);
                                        answer_chunk_str.push_str(&md_img);
                                    }
                                    md_segments.push((md_img, is_thought));
                                }
                            } else if mime.starts_with("audio/") {
                                let wav_b64 = if mime.contains("L16") || mime.contains("pcm") {
                                    if let Ok(raw_pcm) = STANDARD.decode(b64_data) {
                                        let wav = wrap_pcm_in_wav(&raw_pcm, 24000, 1, 16);
                                        STANDARD.encode(wav)
                                    } else {
                                        b64_data.to_string()
                                    }
                                } else {
                                    b64_data.to_string()
                                };
                                audio_output = Some(json!({
                                    "id": format!("audio_{}", uuid::Uuid::new_v4().simple()),
                                    "data": wav_b64,
                                    "expires_at": state.created + 3600,
                                    "transcript": ""
                                }));
                            }
                        }
                    }
                }

                if state.md_thought {
                    // Legacy `--markdown-thought` mode: emit `## Thought process` in `delta.content`
                    let mut combined_md = String::new();
                    for (seg, seg_is_thought) in &md_segments {
                        if !state.prev_is_thought && *seg_is_thought {
                            combined_md.push_str("## Thought process\n");
                            state.prev_is_thought = true;
                        } else if state.prev_is_thought && !*seg_is_thought {
                            combined_md.push_str("\n## Answer after Thoughts\n");
                            state.prev_is_thought = false;
                        }
                        combined_md.push_str(seg);
                    }
                    if state.prev_is_thought && !tool_calls.is_empty() {
                        combined_md.push_str("\n## Answer after Thoughts\n");
                        state.prev_is_thought = false;
                    }
                    if !combined_md.is_empty() || audio_output.is_some() {
                        let mut delta = json!({});
                        if !state.role_sent {
                            delta["role"] = json!("assistant");
                            state.role_sent = true;
                        }
                        if !combined_md.is_empty() {
                            delta["content"] = json!(combined_md);
                        }
                        if let Some(aud) = audio_output.take() {
                            delta["audio"] = aud;
                        }
                        let chunk = json!({
                            "id": state.stream_id,
                            "object": "chat.completion.chunk",
                            "created": state.created,
                            "model": state.model,
                            "choices": [{ "index": 0, "delta": delta, "finish_reason": Value::Null }]
                        });
                        if let Ok(s) = serde_json::to_string(&chunk) {
                            out_sse.push_str(&format!("data: {s}\n\n"));
                        }
                    }
                } else {
                    // Default mode: emit native `delta.reasoning_content` for DSH UI & DeepSeek compatibility
                    if !thought_chunk_str.is_empty() && !state.no_thought_process {
                        let mut r_delta = json!({
                            "reasoning_content": thought_chunk_str
                        });
                        if !state.role_sent {
                            r_delta["role"] = json!("assistant");
                            state.role_sent = true;
                        }
                        state.reasoning_sent = true;

                        let r_chunk = json!({
                            "id": state.stream_id,
                            "object": "chat.completion.chunk",
                            "created": state.created,
                            "model": state.model,
                            "choices": [{
                                "index": 0,
                                "delta": r_delta,
                                "finish_reason": Value::Null
                            }]
                        });
                        if let Ok(s) = serde_json::to_string(&r_chunk) {
                            out_sse.push_str(&format!("data: {s}\n\n"));
                        }
                    }

                    // Emit normal answer text in `delta.content`
                    if !answer_chunk_str.is_empty() || audio_output.is_some() {
                        let mut c_delta = json!({});
                        if !state.role_sent {
                            c_delta["role"] = json!("assistant");
                            state.role_sent = true;
                        }
                        if !answer_chunk_str.is_empty() {
                            c_delta["content"] = json!(answer_chunk_str);
                        }
                        if let Some(aud) = audio_output.take() {
                            c_delta["audio"] = aud;
                        }

                        let c_chunk = json!({
                            "id": state.stream_id,
                            "object": "chat.completion.chunk",
                            "created": state.created,
                            "model": state.model,
                            "choices": [{
                                "index": 0,
                                "delta": c_delta,
                                "finish_reason": Value::Null
                            }]
                        });
                        if let Ok(s) = serde_json::to_string(&c_chunk) {
                            out_sse.push_str(&format!("data: {s}\n\n"));
                        }
                    }
                }

                // Emit `tool_calls` in its own dedicated SSE chunk
                if !tool_calls.is_empty() {
                    // Ensure DSH sees `reasoning_content` (even if "") on every tool-call turn
                    if !state.md_thought && !state.no_thought_process && !state.reasoning_sent {
                        let mut empty_r_delta = json!({ "reasoning_content": "" });
                        if !state.role_sent {
                            empty_r_delta["role"] = json!("assistant");
                            state.role_sent = true;
                        }
                        state.reasoning_sent = true;
                        let empty_r_chunk = json!({
                            "id": state.stream_id,
                            "object": "chat.completion.chunk",
                            "created": state.created,
                            "model": state.model,
                            "choices": [{
                                "index": 0,
                                "delta": empty_r_delta,
                                "finish_reason": Value::Null
                            }]
                        });
                        if let Ok(s) = serde_json::to_string(&empty_r_chunk) {
                            out_sse.push_str(&format!("data: {s}\n\n"));
                        }
                    }

                    let mut tc_delta = json!({
                        "tool_calls": tool_calls
                    });
                    if !state.role_sent {
                        tc_delta["role"] = json!("assistant");
                        state.role_sent = true;
                    }

                    let tc_chunk = json!({
                        "id": state.stream_id,
                        "object": "chat.completion.chunk",
                        "created": state.created,
                        "model": state.model,
                        "choices": [{
                            "index": 0,
                            "delta": tc_delta,
                            "finish_reason": Value::Null
                        }]
                    });

                    if let Ok(s) = serde_json::to_string(&tc_chunk) {
                        out_sse.push_str(&format!("data: {s}\n\n"));
                    }
                }
            }
        }
    }

    if out_sse.is_empty() {
        None
    } else {
        Some(out_sse)
    }
}

pub fn finalize_google_sse_stream(
    state: &mut StreamTransformerState,
    app_state: Option<&AppState>,
) -> String {
    let mut final_out = String::new();

    if let Some(st) = app_state {
        let thought_opt = if state.accumulated_thought.is_empty() {
            None
        } else {
            Some(state.accumulated_thought.clone())
        };
        if let Some(first_tc_id) = state.emitted_tool_call_ids.first() {
            st.update_tool_call_meta(
                first_tc_id,
                state.latest_thought_signature.clone(),
                thought_opt,
            );
        } else if !state.accumulated_answer.is_empty() {
            st.record_text_turn(
                &state.accumulated_answer,
                state.latest_thought_signature.clone(),
                thought_opt,
            );
        }
    }

    if state.md_thought && state.prev_is_thought && !state.no_thought_process {
        let mut delta = json!({ "content": "\n## Answer after Thoughts\n" });
        if !state.role_sent {
            delta["role"] = json!("assistant");
            state.role_sent = true;
        }
        let close_chunk = json!({
            "id": state.stream_id,
            "object": "chat.completion.chunk",
            "created": state.created,
            "model": state.model,
            "choices": [{
                "index": 0,
                "delta": delta,
                "finish_reason": Value::Null
            }]
        });
        if let Ok(s) = serde_json::to_string(&close_chunk) {
            final_out.push_str(&format!("data: {s}\n\n"));
        }
        state.prev_is_thought = false;
    }

    let final_finish_reason = if state.saw_tool_calls {
        "tool_calls".to_string()
    } else {
        state
            .finish_reason
            .clone()
            .unwrap_or_else(|| "stop".to_string())
    };

    let mut stop_chunk = json!({
        "id": state.stream_id,
        "object": "chat.completion.chunk",
        "created": state.created,
        "model": state.model,
        "choices": [{
            "index": 0,
            "delta": {},
            "finish_reason": final_finish_reason
        }]
    });

    if let Some(usage) = state.final_usage.take() {
        stop_chunk["usage"] = usage;
    }

    if let Ok(s) = serde_json::to_string(&stop_chunk) {
        final_out.push_str(&format!("data: {s}\n\n"));
    }

    final_out.push_str("data: [DONE]\n\n");
    final_out
}

pub fn transform_google_to_openai(
    body: &Value,
    stream_mode: bool,
    no_thought_process: bool,
    prev_thought: bool,
    md_thought: bool,
    app_state: Option<&AppState>,
) -> (Option<Value>, bool) {
    let mut last_contains_thought = prev_thought;
    let mut empty_choices = true;
    let message_type = if stream_mode { "delta" } else { "message" };

    let converted_model_name = {
        let mut model_name = body.get("modelVersion").cloned().unwrap_or(json!(""));
        if no_thought_process && model_name.is_string() {
            model_name = json!(format!(
                "{}-no-thought-process",
                model_name.as_str().unwrap()
            ));
        }
        model_name
    };

    let created_ts = body.get("created").cloned().unwrap_or_else(|| {
        json!(SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0))
    });

    let mut openai_response = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().simple()),
        "object": if stream_mode { "chat.completion.chunk" } else { "chat.completion" },
        "created": created_ts,
        "model": converted_model_name,
        "choices": []
    });

    if let Some(usage_metadata) = body.get("usageMetadata") {
        openai_response["usage"] = build_usage_json(usage_metadata);
    }

    if let Some(candidates) = body.get("candidates").and_then(Value::as_array) {
        for (index, candidate) in candidates.iter().enumerate() {
            if let Some(content) = candidate.get("content") {
                if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                    let candidate_thought_sig: Option<String> = parts.iter().find_map(|p| {
                        p.get("thoughtSignature")
                            .or_else(|| p.get("thought_signature"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                    });

                    let mut text_segments: Vec<(String, bool)> = Vec::new();
                    let mut raw_thought_acc = String::new();
                    let mut raw_answer_acc = String::new();
                    let mut tool_calls: Vec<Value> = Vec::new();
                    let mut audio_output: Option<Value> = None;

                    for part in parts {
                        let is_thought =
                            part.get("thought").and_then(Value::as_bool) == Some(true);
                        let thought_sig = part
                            .get("thoughtSignature")
                            .or_else(|| part.get("thought_signature"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .map(|s| s.to_string())
                            .or_else(|| candidate_thought_sig.clone());

                        if let Some(text) = part.get("text").and_then(Value::as_str) {
                            if !text.is_empty() {
                                if is_thought {
                                    raw_thought_acc.push_str(text);
                                } else {
                                    raw_answer_acc.push_str(text);
                                }
                                if !(no_thought_process && is_thought) {
                                    text_segments.push((text.to_string(), is_thought));
                                }
                            }
                        }

                        if let Some(fc) =
                            part.get("functionCall").or_else(|| part.get("function_call"))
                        {
                            if let Some(name) = fc.get("name").and_then(Value::as_str) {
                                let raw_args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                                let canonical_args_val = if raw_args.is_string() {
                                    serde_json::from_str::<Value>(raw_args.as_str().unwrap_or("{}"))
                                        .map(|v| canonicalize_json(&v))
                                        .unwrap_or_else(|_| json!({}))
                                } else {
                                    canonicalize_json(&raw_args)
                                };
                                let args_str = serde_json::to_string(&canonical_args_val)
                                    .unwrap_or_else(|_| "{}".to_string());

                                let call_id =
                                    format!("call_{}", uuid::Uuid::new_v4().simple());
                                let tc_index = tool_calls.len();
                                let sig_for_this_fc = if tc_index == 0 {
                                    thought_sig.clone()
                                } else {
                                    None
                                };
                                let thought_for_this_fc = if tc_index == 0
                                    && !raw_thought_acc.is_empty()
                                {
                                    Some(raw_thought_acc.clone())
                                } else {
                                    None
                                };

                                if let Some(state) = app_state {
                                    state.record_tool_call(
                                        call_id.clone(),
                                        name.to_string(),
                                        args_str.clone(),
                                        sig_for_this_fc.clone(),
                                        thought_for_this_fc,
                                    );
                                }

                                let mut tc_obj = if stream_mode {
                                    json!({
                                        "index": tc_index,
                                        "id": call_id,
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": args_str
                                        }
                                    })
                                } else {
                                    json!({
                                        "id": call_id,
                                        "type": "function",
                                        "function": {
                                            "name": name,
                                            "arguments": args_str
                                        }
                                    })
                                };

                                if let Some(sig) = sig_for_this_fc {
                                    tc_obj["extra_content"] = json!({
                                        "google": { "thought_signature": sig }
                                    });
                                }

                                tool_calls.push(tc_obj);
                            }
                        }

                        if let Some(inline) =
                            part.get("inlineData").or_else(|| part.get("inline_data"))
                        {
                            let mime = inline
                                .get("mimeType")
                                .or_else(|| inline.get("mime_type"))
                                .and_then(Value::as_str)
                                .unwrap_or("application/octet-stream");
                            if let Some(b64_data) = inline.get("data").and_then(Value::as_str) {
                                if mime.starts_with("image/") {
                                    if !(no_thought_process && is_thought) {
                                        let md_img =
                                            format!("\n![image](data:{mime};base64,{b64_data})\n");
                                        if is_thought {
                                            raw_thought_acc.push_str(&md_img);
                                        } else {
                                            raw_answer_acc.push_str(&md_img);
                                        }
                                        text_segments.push((md_img, is_thought));
                                    }
                                } else if mime.starts_with("audio/") {
                                    let wav_b64 = if mime.contains("L16") || mime.contains("pcm") {
                                        if let Ok(raw_pcm) = STANDARD.decode(b64_data) {
                                            let wav = wrap_pcm_in_wav(&raw_pcm, 24000, 1, 16);
                                            STANDARD.encode(wav)
                                        } else {
                                            b64_data.to_string()
                                        }
                                    } else {
                                        b64_data.to_string()
                                    };
                                    audio_output = Some(json!({
                                        "id": format!("audio_{}", uuid::Uuid::new_v4().simple()),
                                        "data": wav_b64,
                                        "expires_at": created_ts.as_u64().unwrap_or(0) + 3600,
                                        "transcript": ""
                                    }));
                                }
                            }
                        }
                    }

                    if tool_calls.is_empty() && !raw_answer_acc.is_empty() {
                        if let Some(st) = app_state {
                            let thought_opt = if raw_thought_acc.is_empty() {
                                None
                            } else {
                                Some(raw_thought_acc.clone())
                            };
                            st.record_text_turn(
                                &raw_answer_acc,
                                candidate_thought_sig.clone(),
                                thought_opt,
                            );
                        }
                    }

                    let mut message = json!({});
                    let role = content
                        .get("role")
                        .and_then(Value::as_str)
                        .unwrap_or("model");
                    let mapped_role = if role == "model" { "assistant" } else { role };
                    message["role"] = json!(mapped_role);

                    if md_thought {
                        let mut combined_text = String::new();
                        let mut current_thought_state = prev_thought;
                        if no_thought_process {
                            for (seg, _) in &text_segments {
                                combined_text.push_str(seg);
                            }
                        } else {
                            for (seg, seg_is_thought) in &text_segments {
                                if !current_thought_state && *seg_is_thought {
                                    combined_text.push_str("## Thought process\n");
                                    current_thought_state = true;
                                } else if current_thought_state && !*seg_is_thought {
                                    combined_text.push_str("\n## Answer after Thoughts\n");
                                    current_thought_state = false;
                                }
                                combined_text.push_str(seg);
                            }
                            if current_thought_state && (!stream_mode || !tool_calls.is_empty()) {
                                combined_text.push_str("\n## Answer after Thoughts\n");
                                current_thought_state = false;
                            }
                        }
                        last_contains_thought = current_thought_state;
                        if combined_text.is_empty()
                            && tool_calls.is_empty()
                            && audio_output.is_none()
                        {
                            continue;
                        }
                        if !combined_text.is_empty() {
                            message["content"] = json!(combined_text);
                        } else if !stream_mode && !tool_calls.is_empty() {
                            message["content"] = Value::Null;
                        }
                    } else {
                        // Default mode: emit native `reasoning_content` and keep `content` clean
                        if raw_answer_acc.is_empty()
                            && raw_thought_acc.is_empty()
                            && tool_calls.is_empty()
                            && audio_output.is_none()
                        {
                            continue;
                        }
                        if !no_thought_process
                            && (!raw_thought_acc.is_empty() || !tool_calls.is_empty())
                        {
                            message["reasoning_content"] = json!(raw_thought_acc);
                        }
                        if !raw_answer_acc.is_empty() {
                            message["content"] = json!(raw_answer_acc);
                        } else if !stream_mode {
                            message["content"] = Value::Null;
                        }
                        last_contains_thought = false;
                    }

                    if !tool_calls.is_empty() {
                        message["tool_calls"] = json!(tool_calls);
                    }
                    if let Some(aud) = audio_output {
                        message["audio"] = aud;
                    }

                    let finish_reason = if !tool_calls.is_empty() {
                        Some("tool_calls".to_string())
                    } else {
                        candidate
                            .get("finishReason")
                            .and_then(Value::as_str)
                            .map(|r| match r {
                                "STOP" => "stop".to_string(),
                                "MAX_TOKENS" => "length".to_string(),
                                other => other.to_lowercase(),
                            })
                    };

                    openai_response["choices"].as_array_mut().unwrap().push(json!({
                        message_type: message,
                        "finish_reason": finish_reason,
                        "index": index
                    }));

                    empty_choices = false;
                }
            }
        }
    }

    if empty_choices {
        (None, last_contains_thought)
    } else {
        (Some(openai_response), last_contains_thought)
    }
}

/// Fallback extractor for any legacy `<think>...</think>` or `## Thought process` blocks in existing chat history
fn extract_thought_and_answer(text: &str) -> (Option<String>, String) {
    if let Some(start) = text.find("<think>") {
        let after_start = start + "<think>".len();
        let content_start = text[after_start..]
            .strip_prefix('\n')
            .map(|_| after_start + 1)
            .unwrap_or(after_start);

        if let Some(end_rel) = text[content_start..].find("</think>") {
            let end_idx = content_start + end_rel;
            let mut thought_slice = &text[content_start..end_idx];
            if let Some(stripped) = thought_slice.strip_suffix('\n') {
                thought_slice = stripped;
            }
            let after_close = end_idx + "</think>".len();
            let rest_start = text[after_close..]
                .strip_prefix('\n')
                .map(|_| after_close + 1)
                .unwrap_or(after_close);

            let mut remaining = String::new();
            remaining.push_str(&text[..start]);
            remaining.push_str(&text[rest_start..]);
            return (Some(thought_slice.to_string()), remaining);
        } else {
            let thought_slice = &text[content_start..];
            return (Some(thought_slice.to_string()), text[..start].to_string());
        }
    }

    if let Some(start) = text.find("## Thought process\n") {
        let content_start = start + "## Thought process\n".len();
        if let Some(end_rel) = text[content_start..].find("\n## Answer after Thoughts\n") {
            let end_idx = content_start + end_rel;
            let thought_slice = &text[content_start..end_idx];
            let rest_start = end_idx + "\n## Answer after Thoughts\n".len();
            let mut remaining = String::new();
            remaining.push_str(&text[..start]);
            remaining.push_str(&text[rest_start..]);
            return (Some(thought_slice.to_string()), remaining);
        }
    }

    (None, text.to_string())
}

fn sanitize_gemini_schema(schema: &Value) -> Value {
    let mut defs = Map::new();
    if let Value::Object(root_map) = schema {
        if let Some(Value::Object(d)) = root_map
            .get("$defs")
            .or_else(|| root_map.get("definitions"))
        {
            defs = d.clone();
        }
    }
    let sanitized = sanitize_schema_inner(schema, &defs, 0);
    canonicalize_json(&sanitized)
}

fn sanitize_schema_inner(schema: &Value, defs: &Map<String, Value>, depth: usize) -> Value {
    if depth > 16 {
        return json!({ "type": "object" });
    }

    let map = match schema {
        Value::Object(m) => m,
        _ => return json!({ "type": "string" }),
    };

    if let Some(ref_str) = map.get("$ref").and_then(Value::as_str) {
        let ref_name = ref_str
            .strip_prefix("#/$defs/")
            .or_else(|| ref_str.strip_prefix("#/definitions/"));
        if let Some(name) = ref_name {
            if let Some(target) = defs.get(name) {
                let mut resolved = sanitize_schema_inner(target, defs, depth + 1);
                if let (Some(desc), Some(obj)) = (
                    map.get("description").and_then(Value::as_str),
                    resolved.as_object_mut(),
                ) {
                    obj.insert("description".to_string(), json!(desc));
                }
                return resolved;
            }
        }
        return json!({ "type": "object" });
    }

    if let Some(Value::Array(all_of)) = map.get("allOf") {
        let mut merged = Map::new();
        for (k, v) in map {
            if k != "allOf" {
                merged.insert(k.clone(), v.clone());
            }
        }
        for sub in all_of {
            if let Value::Object(sub_map) = sub {
                for (k, v) in sub_map {
                    merged.entry(k.clone()).or_insert_with(|| v.clone());
                }
            }
        }
        return sanitize_schema_inner(&Value::Object(merged), defs, depth + 1);
    }

    let mut clean = Map::new();
    let mut is_nullable = map.get("nullable").and_then(Value::as_bool).unwrap_or(false);

    if let Some(Value::Array(variants)) = map.get("anyOf").or_else(|| map.get("oneOf")) {
        let mut non_null_variants = Vec::new();
        for v in variants {
            let is_null_branch = v.get("type").and_then(Value::as_str) == Some("null")
                || v.get("const").map(Value::is_null).unwrap_or(false);
            if is_null_branch {
                is_nullable = true;
            } else {
                non_null_variants.push(sanitize_schema_inner(v, defs, depth + 1));
            }
        }

        if non_null_variants.len() == 1 {
            if let Value::Object(single_map) = non_null_variants.remove(0) {
                clean = single_map;
            }
        } else if !non_null_variants.is_empty() {
            clean.insert("anyOf".to_string(), Value::Array(non_null_variants));
        }
    }

    if let Some(type_val) = map.get("type") {
        match type_val {
            Value::Array(types_arr) => {
                let mut primary_type = None;
                for t in types_arr {
                    if t.as_str() == Some("null") {
                        is_nullable = true;
                    } else if primary_type.is_none() {
                        primary_type = t.as_str();
                    }
                }
                if let Some(pt) = primary_type {
                    clean.insert("type".to_string(), json!(pt));
                }
            }
            Value::String(t) => {
                if t.eq_ignore_ascii_case("null") {
                    is_nullable = true;
                    clean.insert("type".to_string(), json!("string"));
                } else {
                    clean.insert("type".to_string(), json!(t));
                }
            }
            _ => {}
        }
    }

    if let Some(const_val) = map.get("const") {
        if const_val.is_null() {
            is_nullable = true;
        } else {
            let s = match const_val {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            };
            clean.insert("enum".to_string(), json!([s]));
            clean.insert("type".to_string(), json!("string"));
        }
    } else if let Some(Value::Array(enum_arr)) = map.get("enum") {
        let mut str_enums = Vec::new();
        for item in enum_arr {
            match item {
                Value::Null => is_nullable = true,
                Value::String(s) => str_enums.push(json!(s)),
                other => str_enums.push(json!(other.to_string())),
            }
        }
        if !str_enums.is_empty() {
            clean.insert("enum".to_string(), Value::Array(str_enums));
            clean.insert("type".to_string(), json!("string"));
        }
    }

    if let Some(Value::Object(props)) = map.get("properties") {
        let mut clean_props = Map::new();
        for (prop_name, prop_val) in props {
            clean_props.insert(
                prop_name.clone(),
                sanitize_schema_inner(prop_val, defs, depth + 1),
            );
        }
        if !clean_props.is_empty() {
            if let Some(Value::Array(req_arr)) = map.get("required") {
                let valid_req: Vec<Value> = req_arr
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|k| clean_props.contains_key(*k))
                    .map(|k| json!(k))
                    .collect();
                if !valid_req.is_empty() {
                    clean.insert("required".to_string(), Value::Array(valid_req));
                }
            }
            clean.insert("properties".to_string(), Value::Object(clean_props));
        }
    }

    if let Some(items_val) = map.get("items").or_else(|| map.get("prefixItems")) {
        match items_val {
            Value::Object(_) => {
                clean.insert(
                    "items".to_string(),
                    sanitize_schema_inner(items_val, defs, depth + 1),
                );
            }
            Value::Array(arr) if !arr.is_empty() => {
                if arr.len() == 1 {
                    clean.insert(
                        "items".to_string(),
                        sanitize_schema_inner(&arr[0], defs, depth + 1),
                    );
                } else {
                    let sub: Vec<Value> = arr
                        .iter()
                        .map(|v| sanitize_schema_inner(v, defs, depth + 1))
                        .collect();
                    clean.insert("items".to_string(), json!({ "anyOf": sub }));
                }
            }
            _ => {}
        }
    }

    const ALLOWED_SCALARS: &[&str] = &[
        "format",
        "title",
        "description",
        "maxItems",
        "minItems",
        "minProperties",
        "maxProperties",
        "minimum",
        "maximum",
        "minLength",
        "maxLength",
        "pattern",
        "example",
        "default",
        "propertyOrdering",
    ];

    for &key in ALLOWED_SCALARS {
        if let Some(val) = map.get(key) {
            if key == "format" {
                if let Some(fmt_str) = val.as_str() {
                    if !matches!(
                        fmt_str,
                        "enum" | "date-time" | "int32" | "int64" | "float" | "double"
                    ) {
                        continue;
                    }
                }
            }
            clean.entry(key.to_string()).or_insert_with(|| val.clone());
        }
    }

    if is_nullable {
        clean.insert("nullable".to_string(), Value::Bool(true));
    }

    if !clean.contains_key("type") && !clean.contains_key("anyOf") {
        if clean.contains_key("properties") {
            clean.insert("type".to_string(), json!("object"));
        } else if clean.contains_key("items") {
            clean.insert("type".to_string(), json!("array"));
        } else {
            clean.insert("type".to_string(), json!("string"));
        }
    }

    if clean.get("type").and_then(Value::as_str) == Some("array") && !clean.contains_key("items") {
        clean.insert("items".to_string(), json!({ "type": "string" }));
    }

    Value::Object(clean)
}

pub async fn transform_openai_to_google(
    body: &Value,
    client: &Client,
    api_key: &str,
    thinking_config: &ThinkingConfig,
    model_name: &str,
    app_state: Option<&AppState>,
) -> Value {
    if let Some(st) = app_state {
        st.sync_all_from_disk();
    }

    let empty = vec![];
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .unwrap_or(&empty);

    let mut local_tool_names: HashMap<String, String> = HashMap::new();
    for msg in messages {
        if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for tc in tool_calls {
                if let (Some(id), Some(name)) = (
                    tc.get("id").and_then(Value::as_str),
                    tc.pointer("/function/name").and_then(Value::as_str),
                ) {
                    local_tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }

    let mut system_parts: Vec<Value> = Vec::new();
    let mut contents: Vec<Value> = Vec::new();
    let mut seen_non_system = false;

    for (msg_idx, msg) in messages.iter().enumerate() {
        let raw_role = msg.get("role").and_then(Value::as_str).unwrap_or("user");

        if (raw_role == "system" || raw_role == "developer") && !seen_non_system {
            if let Some(text) = msg.get("content").and_then(Value::as_str) {
                if !text.is_empty() {
                    system_parts.push(json!({ "text": text }));
                }
            } else if let Some(arr) = msg.get("content").and_then(Value::as_array) {
                for p in arr {
                    if let Some(t) = p.get("text").and_then(Value::as_str) {
                        if !t.is_empty() {
                            system_parts.push(json!({ "text": t }));
                        }
                    }
                }
            }
            continue;
        }
        seen_non_system = true;

        if raw_role == "tool" || raw_role == "function" {
            let tool_call_id = msg
                .get("tool_call_id")
                .and_then(Value::as_str)
                .unwrap_or("");
            let func_name = msg
                .get("name")
                .and_then(Value::as_str)
                .map(|s| s.to_string())
                .or_else(|| local_tool_names.get(tool_call_id).cloned())
                .or_else(|| app_state.and_then(|st| st.get_tool_name_by_id(tool_call_id)))
                .unwrap_or_else(|| "tool_response".to_string());

            let content_val = msg.get("content").unwrap_or(&Value::Null);
            let raw_str = match content_val {
                Value::String(s) => s.clone(),
                Value::Array(arr) => arr
                    .iter()
                    .filter_map(|p| p.get("text").and_then(Value::as_str))
                    .collect::<Vec<&str>>()
                    .join("\n"),
                other => other.to_string(),
            };

            let parsed_response = match serde_json::from_str::<Value>(&raw_str) {
                Ok(Value::Object(map)) => canonicalize_json(&Value::Object(map)),
                Ok(other_json) => json!({ "result": canonicalize_json(&other_json) }),
                Err(_) => json!({ "result": raw_str }),
            };

            let func_resp_part = json!({
                "functionResponse": {
                    "name": func_name,
                    "response": parsed_response
                }
            });

            if let Some(last_content) = contents.last_mut() {
                if last_content.get("role").and_then(Value::as_str) == Some("user") {
                    if let Some(parts_arr) =
                        last_content.get_mut("parts").and_then(Value::as_array_mut)
                    {
                        parts_arr.push(func_resp_part);
                        continue;
                    }
                }
            }

            contents.push(json!({
                "role": "user",
                "parts": [func_resp_part]
            }));
            continue;
        }

        let google_role = if raw_role == "assistant" {
            "model"
        } else {
            "user"
        };

        let client_echoed_reasoning = msg
            .get("reasoning_content")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string());

        let mut fallback_thought: Option<String> = client_echoed_reasoning;
        let mut cached_gemini_thought: Option<String> = None;
        let mut answer_text_accumulator = String::new();
        let mut parts_vec: Vec<Value> = Vec::new();

        let content = msg.get("content").unwrap_or(&Value::Null);
        match content {
            Value::String(text) => {
                if google_role == "model" {
                    let (t_opt, clean_ans) = extract_thought_and_answer(text);
                    if fallback_thought.is_none() {
                        fallback_thought = t_opt;
                    }
                    if !clean_ans.is_empty() {
                        answer_text_accumulator.push_str(&clean_ans);
                        parts_vec.push(json!({ "text": clean_ans }));
                    }
                } else if !text.is_empty() {
                    parts_vec.push(json!({ "text": text }));
                }
            }
            Value::Array(content_parts) => {
                for part in content_parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                if google_role == "model" {
                                    let (t_opt, clean_ans) = extract_thought_and_answer(text);
                                    if fallback_thought.is_none() {
                                        fallback_thought = t_opt;
                                    }
                                    if !clean_ans.is_empty() {
                                        answer_text_accumulator.push_str(&clean_ans);
                                        parts_vec.push(json!({ "text": clean_ans }));
                                    }
                                } else if !text.is_empty() {
                                    parts_vec.push(json!({ "text": text }));
                                }
                            }
                        }
                        Some("image_url") => {
                            if let Some(image_url) = part.get("image_url") {
                                let url_or_b64 = image_url["url"].as_str().unwrap_or("");
                                if url_or_b64.starts_with("data:") {
                                    match upload_base64_image_to_google(
                                        client,
                                        api_key,
                                        url_or_b64,
                                        app_state,
                                    )
                                    .await
                                    {
                                        Ok((mime, uploaded_uri)) => {
                                            parts_vec.push(json!({
                                                "file_data": {
                                                    "mime_type": mime,
                                                    "file_uri": uploaded_uri
                                                }
                                            }));
                                        }
                                        Err(e) => log::error!("Error uploading image: {e}"),
                                    }
                                }
                            }
                        }
                        Some("input_audio") => {
                            if let Some(audio) = part.get("input_audio") {
                                let raw_data = audio["data"].as_str().unwrap_or("");
                                let fmt = audio["format"].as_str().unwrap_or("wav");
                                let data_uri = if raw_data.starts_with("data:") {
                                    raw_data.to_string()
                                } else {
                                    format!("data:audio/{fmt};base64,{raw_data}")
                                };
                                if let Ok((mime, uploaded_uri)) = upload_base64_audio_to_google(
                                    client,
                                    api_key,
                                    &data_uri,
                                    app_state,
                                )
                                .await
                                {
                                    parts_vec.push(json!({
                                        "file_data": {
                                            "mime_type": mime,
                                            "file_uri": uploaded_uri
                                        }
                                    }));
                                } else {
                                    log::error!("Error uploading audio");
                                }
                            }
                        }
                        _ => log::warn!("Unknown content part type in message"),
                    }
                }
            }
            _ => {}
        }

        let has_tool_calls = msg
            .get("tool_calls")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            .unwrap_or(false)
            || msg.get("function_call").is_some();

        if let Some(tool_calls) = msg.get("tool_calls").and_then(Value::as_array) {
            for (tc_idx, tc) in tool_calls.iter().enumerate() {
                let tc_id = tc.get("id").and_then(Value::as_str).unwrap_or("");
                if let Some(func) = tc.get("function") {
                    if let Some(name) = func.get("name").and_then(Value::as_str) {
                        let args_str = func
                            .get("arguments")
                            .and_then(Value::as_str)
                            .unwrap_or("{}");
                        let args_json = serde_json::from_str::<Value>(args_str)
                            .map(|v| canonicalize_json(&v))
                            .unwrap_or_else(|_| json!({}));
                        let canonical_args_str = serde_json::to_string(&args_json)
                            .unwrap_or_else(|_| "{}".to_string());

                        let client_sig = tc
                            .pointer("/extra_content/google/thought_signature")
                            .or_else(|| tc.get("thought_signature"))
                            .and_then(Value::as_str)
                            .filter(|s| !s.is_empty())
                            .unwrap_or(FALLBACK_THOUGHT_SIGNATURE);

                        let pinned_meta = app_state.map(|st| {
                            st.get_or_pin_tool_call(
                                tc_id,
                                msg_idx,
                                tc_idx,
                                name,
                                &canonical_args_str,
                                client_sig,
                                if tc_idx == 0 {
                                    fallback_thought.clone()
                                } else {
                                    None
                                },
                            )
                        });

                        // PRIORITY 1: Always prefer the exact `thought_text` recorded from Gemini in AppState
                        if tc_idx == 0 {
                            cached_gemini_thought =
                                pinned_meta.as_ref().and_then(|m| m.thought_text.clone());
                        }

                        let mut fc_part = json!({
                            "functionCall": {
                                "name": name,
                                "args": args_json
                            }
                        });

                        if tc_idx == 0 {
                            let sig = pinned_meta
                                .and_then(|m| m.thought_signature)
                                .filter(|s| !s.is_empty())
                                .unwrap_or_else(|| client_sig.to_string());
                            fc_part["thoughtSignature"] = json!(sig);
                        }

                        parts_vec.push(fc_part);
                    }
                }
            }
        } else if let Some(fc) = msg.get("function_call") {
            if let Some(name) = fc.get("name").and_then(Value::as_str) {
                let args_str = fc.get("arguments").and_then(Value::as_str).unwrap_or("{}");
                let args_json = serde_json::from_str::<Value>(args_str)
                    .map(|v| canonicalize_json(&v))
                    .unwrap_or_else(|_| json!({}));
                parts_vec.push(json!({
                    "functionCall": {
                        "name": name,
                        "args": args_json
                    },
                    "thoughtSignature": FALLBACK_THOUGHT_SIGNATURE
                }));
            }
        }

        if google_role == "model" && !has_tool_calls && !answer_text_accumulator.is_empty() {
            if let Some(text_meta) =
                app_state.and_then(|st| st.get_text_turn(&answer_text_accumulator))
            {
                cached_gemini_thought = text_meta.thought_text;
                if let Some(sig) = text_meta.thought_signature {
                    if let Some(last_part) = parts_vec.last_mut() {
                        last_part["thoughtSignature"] = json!(sig);
                    }
                }
            }
        }

        if google_role == "model" {
            let final_thought = cached_gemini_thought.or(fallback_thought);
            if let Some(thought_str) = final_thought.filter(|s| !s.is_empty()) {
                parts_vec.insert(
                    0,
                    json!({
                        "text": thought_str,
                        "thought": true
                    }),
                );
            }
        }

        if parts_vec.is_empty() {
            continue;
        }

        if google_role == "user" {
            if let Some(last_content) = contents.last_mut() {
                if last_content.get("role").and_then(Value::as_str) == Some("user") {
                    if let Some(existing_parts) =
                        last_content.get_mut("parts").and_then(Value::as_array_mut)
                    {
                        existing_parts.extend(parts_vec);
                        continue;
                    }
                }
            }
        }

        contents.push(json!({
            "role": google_role,
            "parts": parts_vec
        }));
    }

    if let Some(st) = app_state {
        st.verify_prefix_stability(&contents);
    }

    let temperature = body.get("temperature").and_then(Value::as_f64);
    let max_tokens = body
        .get("max_tokens")
        .or_else(|| body.get("max_completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(8192);
    let top_p = body.get("top_p").and_then(Value::as_f64);
    // Only extract penalties if explicitly non-zero in the OpenAI request
    let presence_penalty = body
        .get("presence_penalty")
        .and_then(Value::as_f64)
        .filter(|v| v.abs() > f64::EPSILON);
    let frequency_penalty = body
        .get("frequency_penalty")
        .and_then(Value::as_f64)
        .filter(|v| v.abs() > f64::EPSILON);
    let n_candidates = body
        .get("n")
        .and_then(Value::as_i64)
        .filter(|&n| n > 1);
    let stop_sequences = match body.get("stop") {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
            .collect::<Vec<String>>(),
        Some(Value::String(s)) if !s.is_empty() => vec![s.to_string()],
        _ => Vec::new(),
    };

    let categories = [
        "HARM_CATEGORY_HARASSMENT",
        "HARM_CATEGORY_HATE_SPEECH",
        "HARM_CATEGORY_SEXUALLY_EXPLICIT",
        "HARM_CATEGORY_DANGEROUS_CONTENT",
        "HARM_CATEGORY_CIVIC_INTEGRITY",
    ];

    let safety_settings: Vec<Value> = categories
        .iter()
        .map(|category| {
            json!({
                "category": category,
                "threshold": "BLOCK_NONE"
            })
        })
        .collect();

    // Only populate fields that are actually set instead of sending dummy defaults
    let mut generation_config = json!({
        "maxOutputTokens": max_tokens
    });

    if let Some(temp) = temperature {
        generation_config["temperature"] = json!(temp);
    }
    if let Some(tp) = top_p {
        generation_config["topP"] = json!(tp);
    }
    if !stop_sequences.is_empty() {
        generation_config["stopSequences"] = json!(stop_sequences);
    }
    if let Some(n) = n_candidates {
        generation_config["candidateCount"] = json!(n);
    }
    if let Some(pp) = presence_penalty {
        generation_config["presencePenalty"] = json!(pp);
    }
    if let Some(fp) = frequency_penalty {
        generation_config["frequencyPenalty"] = json!(fp);
    }

    if let Some(rf) = body.get("response_format") {
        match rf.get("type").and_then(Value::as_str) {
            Some("json_object") => {
                generation_config["responseMimeType"] = json!("application/json");
            }
            Some("json_schema") => {
                generation_config["responseMimeType"] = json!("application/json");
                if let Some(schema) = rf.pointer("/json_schema/schema") {
                    generation_config["responseSchema"] = sanitize_gemini_schema(schema);
                }
            }
            _ => {}
        }
    }

    let mut wants_audio = model_name.contains("-tts");
    let mut wants_image = model_name.contains("-image") || model_name.contains("banana");

    if let Some(mods) = body.get("modalities").and_then(Value::as_array) {
        let mut google_mods = Vec::new();
        for m in mods {
            if let Some(m_str) = m.as_str() {
                match m_str.to_ascii_lowercase().as_str() {
                    "text" => google_mods.push(json!("TEXT")),
                    "image" => {
                        wants_image = true;
                        google_mods.push(json!("IMAGE"));
                    }
                    "audio" => {
                        wants_audio = true;
                        google_mods.push(json!("AUDIO"));
                    }
                    _ => {}
                }
            }
        }
        if !google_mods.is_empty() {
            generation_config["responseModalities"] = json!(google_mods);
        }
    } else if wants_audio {
        generation_config["responseModalities"] = json!(["AUDIO"]);
    } else if wants_image {
        generation_config["responseModalities"] = json!(["TEXT", "IMAGE"]);
    }

    if wants_audio || body.get("audio").is_some() {
        if model_name.contains("-tts") {
            generation_config["responseModalities"] = json!(["AUDIO"]);
        }
        let voice_name = body
            .pointer("/audio/voice")
            .and_then(Value::as_str)
            .map(map_openai_voice)
            .unwrap_or("Kore");
        generation_config["speechConfig"] = json!({
            "voiceConfig": {
                "prebuiltVoiceConfig": {
                    "voiceName": voice_name
                }
            }
        });
    }

    if wants_image {
        if let Some(img_cfg) = body.get("image_config") {
            generation_config["imageConfig"] = img_cfg.clone();
        } else if let Some(size) = body.get("size").and_then(Value::as_str) {
            generation_config["imageConfig"] = json!({
                "aspectRatio": size_to_aspect_ratio(size)
            });
        }
    }

    if thinking_config.enabled {
        let mut tc = json!({
            "includeThoughts": thinking_config.include_thoughts
        });
        if let Some(ref level) = thinking_config.thinking_level {
            tc["thinkingLevel"] = json!(level);
        } else if let Some(budget) = thinking_config.budget {
            tc["thinkingBudget"] = json!(budget);
        }
        generation_config["thinkingConfig"] = tc;
    }

    // Apply the model-capability ignore filter before finalizing generationConfig
    apply_generation_config_ignore_filter(&mut generation_config, model_name);

    log::debug!(
        "Model: {}, final generationConfig: {}",
        model_name,
        generation_config
    );

    let mut result = json!({
        "contents": contents,
        "safetySettings": safety_settings,
        "generationConfig": generation_config,
    });

    if !system_parts.is_empty() {
        result["systemInstruction"] = json!({
            "parts": system_parts,
        });
    }

    let mut func_declarations: Vec<Value> = Vec::new();
    let mut google_tools: Vec<Value> = Vec::new();

    if let Some(tools) = body.get("tools").and_then(Value::as_array) {
        for tool in tools {
            match tool.get("type").and_then(Value::as_str) {
                Some("function") => {
                    if let Some(func) = tool.get("function") {
                        if let Some(name) = func.get("name").and_then(Value::as_str) {
                            let mut decl = json!({ "name": name });
                            if let Some(desc) = func.get("description").and_then(Value::as_str) {
                                decl["description"] = json!(desc);
                            }
                            if let Some(params) = func.get("parameters") {
                                let sanitized = sanitize_gemini_schema(params);
                                if sanitized.as_object().map(|o| !o.is_empty()).unwrap_or(false) {
                                    decl["parameters"] = sanitized;
                                }
                            }
                            func_declarations.push(decl);
                        }
                    }
                }
                Some("google_search") => {
                    google_tools.push(json!({ "googleSearch": {} }));
                }
                Some("code_execution") => {
                    google_tools.push(json!({ "codeExecution": {} }));
                }
                _ => {}
            }
        }
    } else if let Some(functions) = body.get("functions").and_then(Value::as_array) {
        for func in functions {
            if let Some(name) = func.get("name").and_then(Value::as_str) {
                let mut decl = json!({ "name": name });
                if let Some(desc) = func.get("description").and_then(Value::as_str) {
                    decl["description"] = json!(desc);
                }
                if let Some(params) = func.get("parameters") {
                    decl["parameters"] = sanitize_gemini_schema(params);
                }
                func_declarations.push(decl);
            }
        }
    }

    if !func_declarations.is_empty() {
        google_tools.insert(0, json!({ "functionDeclarations": func_declarations }));
    }
    if !google_tools.is_empty() {
        result["tools"] = json!(google_tools);
    }

    if let Some(tool_choice) = body.get("tool_choice") {
        match tool_choice {
            Value::String(mode_str) => {
                let mode = match mode_str.as_str() {
                    "none" => Some("NONE"),
                    "auto" => Some("AUTO"),
                    "required" | "any" => Some("ANY"),
                    _ => None,
                };
                if let Some(m) = mode {
                    result["toolConfig"] = json!({
                        "functionCallingConfig": { "mode": m }
                    });
                }
            }
            Value::Object(obj) => {
                if let Some(func_name) = tool_choice
                    .pointer("/function/name")
                    .or_else(|| obj.get("name"))
                    .and_then(Value::as_str)
                {
                    result["toolConfig"] = json!({
                        "functionCallingConfig": {
                            "mode": "ANY",
                            "allowedFunctionNames": [func_name]
                        }
                    });
                }
            }
            _ => {}
        }
    }

    result
}