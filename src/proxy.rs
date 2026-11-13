use crate::app_state::AppState;
use crate::transformers::{
    extract_PCM_or_audio_from_google, extract_images_from_google_response,
    finalize_google_sse_stream, map_openai_voice, size_to_aspect_ratio,
    transform_google_sse_event, transform_google_to_openai, transform_openai_to_google,
    wrap_pcm_in_wav, StreamTransformerState,
};
use crate::utils::extract_api_key;
use actix_web::{
    error::{ErrorBadRequest, ErrorInternalServerError},
    web::{self, Bytes},
    Error, HttpMessage, HttpRequest, HttpResponse,
};
use awc::Client;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use futures_util::stream::StreamExt;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Thinking will be enabled by name if matched by thinking_enabled_models
/// Represents the resolved `generationConfig.thinkingConfig` for Gemini 2.5 (`thinkingBudget`)
/// and Gemini 3 / Gemini 4 (`thinkingLevel`).
#[derive(Debug, Clone)]
pub struct ThinkingConfig {
    pub enabled: bool,
    pub include_thoughts: bool,
    /// Used for Gemini 3 and Gemini 4 models (`"MINIMAL" | "LOW" | "MEDIUM" | "HIGH"`).
    pub thinking_level: Option<String>,
    /// Used for Gemini 2.0 / 2.5 models (e.g. `24576` for high, `8192` for medium, `1024` for low, `-1` for dynamic).
    pub budget: Option<i64>,
}

fn build_thinking_config(
    model_name: &str,
    no_thought_process: bool,
    request_effort: Option<&str>,
    default_effort: &str,
    cli_include_thoughts: bool,
) -> ThinkingConfig {
    let lower_model = model_name.to_ascii_lowercase();

    // Models that reject `thinkingConfig` with 400 INVALID_ARGUMENT
    let rejects_thinking = lower_model.contains("-tts")
        || lower_model.contains("-image")
        || lower_model.contains("banana")
        || lower_model.contains("-transcribe")
        || lower_model.starts_with("imagen-")
        || lower_model.starts_with("veo-");

    let is_gemini_3 = lower_model.contains("gemini-3");
    let is_gemini_4 = lower_model.contains("gemini-4");
    let is_gemini_2_5 =
        lower_model.contains("gemini-2.5") || lower_model.contains("gemini-2.0-flash-thinking");

    if rejects_thinking || !(is_gemini_3 || is_gemini_4 || is_gemini_2_5) {
        return ThinkingConfig {
            enabled: false,
            include_thoughts: false,
            thinking_level: None,
            budget: None,
        };
    }

    let effort = request_effort
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .unwrap_or(default_effort)
        .to_ascii_lowercase();

    let include_thoughts = cli_include_thoughts && !no_thought_process;

    // Gemini 3 and Gemini 4 use official `thinkingLevel` (thinkingBudget is deprecated/rejected)
    if is_gemini_3 || is_gemini_4 {
        let level: Option<&str> = match effort.as_str() {
            "high" | "xhigh" | "max" => Some("HIGH"),
            "medium" | "med" => {
                // `gemini-3-pro` (non-3.1) only supports LOW and HIGH; promote medium -> HIGH
                if lower_model.starts_with("gemini-3-pro") {
                    Some("HIGH")
                } else {
                    Some("MEDIUM")
                }
            }
            "low" => Some("LOW"),
            "minimal" | "min" | "none" | "off" | "disable" => {
                // Gemini 4 does not support MINIMAL (minimum supported tier is LOW)
                if is_gemini_4 || lower_model.contains("-pro") {
                    Some("LOW")
                } else {
                    Some("MINIMAL")
                }
            }
            "dynamic" | "auto" => None, // Omit thinkingLevel to let Gemini use dynamic default
            _ => Some("HIGH"),          // Default to official HIGH level
        };

        return ThinkingConfig {
            enabled: true,
            include_thoughts,
            thinking_level: level.map(|s| s.to_string()),
            budget: None,
        };
    }

    // Gemini 2.5 / 2.0-flash-thinking uses `thinkingBudget`
    let budget: Option<i64> = match effort.as_str() {
        "high" | "xhigh" | "max" => Some(24_576),
        "medium" | "med" => Some(8_192),
        "low" => Some(1_024),
        "minimal" | "min" | "none" | "off" | "disable" => {
            if lower_model.contains("gemini-2.5-pro") {
                Some(128) // 128 is minimum settable budget on 2.5 Pro
            } else {
                Some(0)
            }
        }
        "dynamic" | "auto" => Some(-1),
        _ => Some(24_576),
    };

    ThinkingConfig {
        enabled: true,
        include_thoughts,
        thinking_level: None,
        budget,
    }
}

/// Inspects an upstream 400 INVALID_ARGUMENT response body and strips the rejected parameter(s)
/// from `google_body["generationConfig"]`. Returns `true` if any parameter was removed.
fn strip_rejected_params_on_400(google_body: &mut Value, err_text: &str) -> bool {
    let Some(cfg) = google_body
        .get_mut("generationConfig")
        .and_then(Value::as_object_mut)
    else {
        return false;
    };

    let mut modified = false;

    if err_text.contains("Penalty is not enabled")
        || err_text.contains("presencePenalty")
        || err_text.contains("frequencyPenalty")
        || err_text.contains("presence_penalty")
        || err_text.contains("frequency_penalty")
    {
        modified |= cfg.remove("presencePenalty").is_some();
        modified |= cfg.remove("frequencyPenalty").is_some();
    }

    if err_text.contains("Thinking is not enabled")
        || err_text.contains("Thinking level is not supported")
        || err_text.contains("thinking_budget")
        || err_text.contains("thinking_level")
        || err_text.contains("thinkingConfig")
    {
        modified |= cfg.remove("thinkingConfig").is_some();
    }

    if err_text.contains("JSON mode is not enabled")
        || err_text.contains("response_mime_type")
        || err_text.contains("response_schema")
        || err_text.contains("responseMimeType")
    {
        modified |= cfg.remove("responseMimeType").is_some();
        modified |= cfg.remove("responseSchema").is_some();
    }

    if err_text.contains("Stop sequences")
        || err_text.contains("stop_sequences")
        || err_text.contains("stopSequences")
    {
        modified |= cfg.remove("stopSequences").is_some();
    }

    if err_text.contains("candidate_count") || err_text.contains("candidateCount") {
        modified |= cfg.remove("candidateCount").is_some();
    }

    modified
}

pub async fn reverse_proxy(
    req: HttpRequest,
    body_data: web::Bytes,
    data: web::Data<AppState>,
    client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    let path = req.path();

    if path.starts_with("/v1/images/generations") {
        return handle_images_generations(req, body_data, data, client).await;
    }
    if path.starts_with("/v1/audio/speech") {
        return handle_audio_speech(req, body_data, data, client).await;
    }
    if path.starts_with("/v1/videos/generations") {
        return handle_videos_generations(req, body_data, data, client).await;
    }
    if !path.starts_with("/v1/chat/completions") {
        return Ok(HttpResponse::NotFound().body("Not Found"));
    }

    log::info!("Got chat request: {}", String::from_utf8_lossy(&body_data));

    let json_body: Value = serde_json::from_slice(&body_data)
        .map_err(|_| ErrorInternalServerError("Failed to parse JSON body"))?;

    let is_stream = json_body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let api_key = extract_api_key(&req).ok_or_else(|| ErrorBadRequest("No API key provided"))?;

    let model_name_in_request = json_body["model"]
        .as_str()
        .ok_or_else(|| ErrorBadRequest("Model not found in request"))?;
    let no_thought_process = model_name_in_request.ends_with("-no-thought-process");
    let model_name = if no_thought_process {
        model_name_in_request.trim_end_matches("-no-thought-process")
    } else {
        model_name_in_request
    };

    if model_name.starts_with("veo-") {
        return handle_veo_in_chat(model_name, &json_body, is_stream, &api_key, &data, &client)
            .await;
    }

    // Check incoming OpenAI `reasoning_effort` or `reasoning.effort`
    let request_reasoning_effort = json_body
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .or_else(|| json_body.pointer("/reasoning/effort").and_then(|v| v.as_str()));

    let thinking_config = build_thinking_config(
        model_name,
        no_thought_process,
        request_reasoning_effort,
        &data.default_reasoning_effort,
        data.include_thoughts,
    );

    let effective_hide_thoughts = no_thought_process || !data.include_thoughts;

    let mut google_body = transform_openai_to_google(
        &json_body,
        &client,
        &api_key,
        &thinking_config,
        model_name,
        Some(data.get_ref()),
    )
    .await;

    let google_base_url = data.upstream_url.clone();
    let google_url = if is_stream {
        format!("{google_base_url}/models/{model_name}:streamGenerateContent?alt=sse&key={api_key}")
    } else {
        format!("{google_base_url}/models/{model_name}:generateContent?key={api_key}")
    };

    log::info!("Forwarding request to: {google_url}");

    for attempt in 0..2 {
        let google_body_str = serde_json::to_string(&google_body)
            .map_err(|_| ErrorInternalServerError("Failed to serialize Google body"))?;

        log::info!("Converted request (attempt {}): {google_body_str}", attempt + 1);

        let forward_req = client
            .post(&google_url)
            .insert_header(("Content-Type", "application/json"));

        match forward_req
            .timeout(Duration::from_secs(600))
            .send_body(google_body_str)
            .await
        {
            Ok(mut upstream_response) => {
                let status: awc::http::StatusCode = upstream_response.status();

                if !status.is_success() {
                    let err_body = upstream_response.body().limit(10 * 1024 * 1024).await?;
                    let err_text = String::from_utf8_lossy(&err_body);

                    // If 400 Bad Request is caused by an unsupported parameter on this model,
                    // strip the rejected parameter from `generationConfig` and retry once.
                    if status == awc::http::StatusCode::BAD_REQUEST
                        && attempt == 0
                        && strip_rejected_params_on_400(&mut google_body, &err_text)
                    {
                        log::warn!(
                            "Upstream 400 due to unsupported parameter on '{model_name}'; stripped offending field and retrying: {err_text}"
                        );
                        continue;
                    }

                    log::error!("Upstream error ({}): {}", status, err_text);
                    return Ok(HttpResponse::build(status)
                        .content_type("application/json")
                        .body(err_body));
                }

                if upstream_response.content_type().contains("text/event-stream") {
                    let state_clone = data.get_ref().clone();
                    let md_thought = data.markdown_thought;
                    let requested_model = model_name_in_request.to_string();

                    let up_stream = async_stream_from_upstream(
                        upstream_response,
                        requested_model,
                        effective_hide_thoughts,
                        md_thought,
                        state_clone,
                    );

                    return Ok(HttpResponse::build(status)
                        .insert_header(("Content-Type", "text/event-stream; charset=utf-8"))
                        .insert_header(("Cache-Control", "no-cache"))
                        .insert_header(("Connection", "keep-alive"))
                        .insert_header(("X-Accel-Buffering", "no"))
                        .streaming(up_stream));
                } else {
                    let mut response = HttpResponse::build(status);
                    for (name, value) in upstream_response.headers().iter() {
                        let lower = name.as_str().to_ascii_lowercase();
                        if !lower.contains("content-encoding")
                            && !lower.contains("content-length")
                            && !lower.contains("transfer-encoding")
                        {
                            response.insert_header((name.clone(), value.clone()));
                        }
                    }

                    let body = upstream_response.body().limit(100 * 1024 * 1024).await?;
                    log::info!("Got reply from Google: {}", String::from_utf8_lossy(&body));

                    let google_response: Value = serde_json::from_slice(&body)
                        .map_err(|_| ErrorInternalServerError("Failed to parse Google response"))?;

                    let (openai_response, _) = transform_google_to_openai(
                        &google_response,
                        false,
                        effective_hide_thoughts,
                        false,
                        data.markdown_thought,
                        Some(data.get_ref()),
                    );
                    if let Some(openai_response) = openai_response {
                        log::info!("Replied to client: {openai_response}");
                        return Ok(response.json(openai_response));
                    } else {
                        log::error!(
                            "Non stream mode but no choices available. Replied 503 to client"
                        );
                        return Ok(HttpResponse::ServiceUnavailable()
                            .body("No choices returned from upstream server."));
                    }
                }
            }
            Err(err) => {
                log::error!("Failed to forward request: {err:?}");
                return Ok(HttpResponse::InternalServerError()
                    .body("Failed to connect to upstream server."));
            }
        }
    }

    Ok(HttpResponse::InternalServerError().body("Exhausted upstream retry attempts."))
}

/// Extracts the next complete SSE event block from a raw byte buffer (`\n\n` or `\r\n\r\n`)
/// without ever splitting multi-byte UTF-8 sequences across TCP chunks.
fn extract_next_sse_event(buf: &mut Vec<u8>) -> Option<String> {
    let mut split_idx = None;
    let mut delim_len = 0;

    for i in 0..buf.len() {
        if i + 1 < buf.len() && buf[i] == b'\n' && buf[i + 1] == b'\n' {
            split_idx = Some(i);
            delim_len = 2;
            break;
        }
        if i + 3 < buf.len()
            && buf[i] == b'\r'
            && buf[i + 1] == b'\n'
            && buf[i + 2] == b'\r'
            && buf[i + 3] == b'\n'
        {
            split_idx = Some(i);
            delim_len = 4;
            break;
        }
    }

    if let Some(idx) = split_idx {
        let event_bytes: Vec<u8> = buf.drain(..idx).collect();
        buf.drain(..delim_len);
        Some(String::from_utf8_lossy(&event_bytes).into_owned())
    } else {
        None
    }
}

fn async_stream_from_upstream<S>(
    upstream: S,
    initial_model: String,
    no_thought_process: bool,
    md_thought: bool,
    app_state: AppState,
) -> impl futures_util::Stream<Item = Result<Bytes, Error>>
where
    S: futures_util::Stream<Item = Result<Bytes, awc::error::PayloadError>> + Unpin + 'static,
{
    struct LoopState<St> {
        upstream: St,
        raw_buffer: Vec<u8>,
        done_sent: bool,
        tx_state: StreamTransformerState,
        app_state: AppState,
    }

    let init = LoopState {
        upstream,
        raw_buffer: Vec::with_capacity(8192),
        done_sent: false,
        tx_state: StreamTransformerState::new(initial_model, no_thought_process, md_thought),
        app_state,
    };

    futures_util::stream::unfold(init, |mut st| async move {
        loop {
            match st.upstream.next().await {
                Some(Ok(bytes)) => {
                    st.raw_buffer.extend_from_slice(&bytes);

                    let mut out_events = String::new();
                    while let Some(event_block) = extract_next_sse_event(&mut st.raw_buffer) {
                        log::debug!("SSE event block: {event_block}");
                        if let Some(chunk_sse) = transform_google_sse_event(
                            &event_block,
                            &mut st.tx_state,
                            Some(&st.app_state),
                        ) {
                            out_events.push_str(&chunk_sse);
                        }
                    }

                    if !out_events.is_empty() {
                        return Some((Ok(Bytes::from(out_events)), st));
                    }
                }
                Some(Err(e)) => {
                    log::error!("Error in upstream stream: {e:?}");
                    return Some((
                        Err(ErrorInternalServerError("Error processing upstream stream")),
                        st,
                    ));
                }
                None => {
                    if st.done_sent {
                        return None;
                    }
                    st.done_sent = true;
                    let mut final_out = String::new();

                    if !st.raw_buffer.is_empty() {
                        let tail = String::from_utf8_lossy(&st.raw_buffer).into_owned();
                        st.raw_buffer.clear();
                        if !tail.trim().is_empty() {
                            if let Some(chunk_sse) = transform_google_sse_event(
                                &tail,
                                &mut st.tx_state,
                                Some(&st.app_state),
                            ) {
                                final_out.push_str(&chunk_sse);
                            }
                        }
                    }

                    final_out.push_str(&finalize_google_sse_stream(
                        &mut st.tx_state,
                        Some(&st.app_state),
                    ));
                    return Some((Ok(Bytes::from(final_out)), st));
                }
            }
        }
    })
}

/// POST /v1/images/generations -> Supports both `imagen-*` (:predict) and `gemini-*` (:generateContent)
async fn handle_images_generations(
    req: HttpRequest,
    body_data: web::Bytes,
    data: web::Data<AppState>,
    client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    let api_key = extract_api_key(&req).ok_or_else(|| ErrorBadRequest("No API key provided"))?;
    let json_body: Value = serde_json::from_slice(&body_data)
        .map_err(|_| ErrorBadRequest("Failed to parse JSON body"))?;

    let prompt = json_body["prompt"]
        .as_str()
        .ok_or_else(|| ErrorBadRequest("Missing prompt"))?;
    let raw_model = json_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-2.5-flash-image-preview");
    let model = if raw_model.starts_with("dall-e") || raw_model == "gpt-image-1" {
        "gemini-2.5-flash-image-preview"
    } else {
        raw_model
    };

    let n = json_body.get("n").and_then(|v| v.as_i64()).unwrap_or(1);
    let aspect_ratio = json_body
        .get("aspect_ratio")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json_body
                .get("size")
                .and_then(|v| v.as_str())
                .map(size_to_aspect_ratio)
        })
        .unwrap_or_else(|| "1:1".to_string());
    let response_format = json_body
        .get("response_format")
        .and_then(|v| v.as_str())
        .unwrap_or("b64_json");

    let base_url = &data.upstream_url;

    if model.starts_with("imagen-") {
        let url = format!("{base_url}/models/{model}:predict?key={api_key}");
        let payload = json!({
            "instances": [{ "prompt": prompt }],
            "parameters": {
                "sampleCount": n,
                "aspectRatio": aspect_ratio
            }
        });

        let mut upstream = client
            .post(&url)
            .insert_header(("Content-Type", "application/json"))
            .timeout(Duration::from_secs(300))
            .send_body(payload.to_string())
            .await
            .map_err(|e| ErrorInternalServerError(format!("Imagen request failed: {e}")))?;

        let status = upstream.status();
        let body = upstream.body().limit(50 * 1024 * 1024).await?;
        if !status.is_success() {
            return Ok(HttpResponse::build(status).body(body));
        }

        let resp_json: Value = serde_json::from_slice(&body)
            .map_err(|_| ErrorInternalServerError("Invalid JSON from Imagen"))?;

        let mut data_list = Vec::new();
        if let Some(preds) = resp_json.get("predictions").and_then(|p| p.as_array()) {
            for pred in preds {
                if let Some(b64) = pred.get("bytesBase64Encoded").and_then(|b| b.as_str()) {
                    let mime = pred
                        .get("mimeType")
                        .and_then(|m| m.as_str())
                        .unwrap_or("image/png");
                    if response_format == "url" {
                        data_list.push(json!({ "url": format!("data:{mime};base64,{b64}") }));
                    } else {
                        data_list.push(json!({ "b64_json": b64 }));
                    }
                }
            }
        }

        Ok(HttpResponse::Ok().json(json!({
            "created": now_unix(),
            "data": data_list
        })))
    } else {
        let url = format!("{base_url}/models/{model}:generateContent?key={api_key}");
        let mut image_config = json!({ "aspectRatio": aspect_ratio });
        if let Some(image_size) = json_body
            .get("image_size")
            .or_else(|| json_body.get("quality"))
            .and_then(|v| v.as_str())
        {
            if matches!(image_size, "1K" | "2K" | "4K") {
                image_config["imageSize"] = json!(image_size);
            }
        }

        let payload = json!({
            "contents": [{
                "role": "user",
                "parts": [{ "text": prompt }]
            }],
            "generationConfig": {
                "responseModalities": ["TEXT", "IMAGE"],
                "imageConfig": image_config
            }
        });

        let mut upstream = client
            .post(&url)
            .insert_header(("Content-Type", "application/json"))
            .timeout(Duration::from_secs(300))
            .send_body(payload.to_string())
            .await
            .map_err(|e| ErrorInternalServerError(format!("Gemini image request failed: {e}")))?;

        let status = upstream.status();
        let body = upstream.body().limit(50 * 1024 * 1024).await?;
        if !status.is_success() {
            return Ok(HttpResponse::build(status).body(body));
        }

        let resp_json: Value = serde_json::from_slice(&body)
            .map_err(|_| ErrorInternalServerError("Invalid JSON from Gemini image endpoint"))?;

        let images = extract_images_from_google_response(&resp_json);
        let data_list: Vec<Value> = images
            .into_iter()
            .map(|(mime, b64, revised)| {
                let mut item = if response_format == "url" {
                    json!({ "url": format!("data:{mime};base64,{b64}") })
                } else {
                    json!({ "b64_json": b64 })
                };
                if let Some(rp) = revised {
                    item["revised_prompt"] = json!(rp);
                }
                item
            })
            .collect();

        Ok(HttpResponse::Ok().json(json!({
            "created": now_unix(),
            "data": data_list
        })))
    }
}

/// POST /v1/audio/speech -> Maps OpenAI TTS request to Gemini TTS (`responseModalities: ["AUDIO"]`)
async fn handle_audio_speech(
    req: HttpRequest,
    body_data: web::Bytes,
    data: web::Data<AppState>,
    client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    let api_key = extract_api_key(&req).ok_or_else(|| ErrorBadRequest("No API key provided"))?;
    let json_body: Value = serde_json::from_slice(&body_data)
        .map_err(|_| ErrorBadRequest("Failed to parse JSON body"))?;

    let input_text = json_body["input"]
        .as_str()
        .ok_or_else(|| ErrorBadRequest("Missing 'input' text"))?;
    let raw_model = json_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("gemini-2.5-flash-preview-tts");
    let model = if raw_model.starts_with("tts-") || raw_model.contains("gpt-") {
        "gemini-2.5-flash-preview-tts"
    } else {
        raw_model
    };

    let voice = json_body
        .get("voice")
        .and_then(|v| v.as_str())
        .map(map_openai_voice)
        .unwrap_or("Kore");
    let response_format = json_body
        .get("response_format")
        .and_then(|v| v.as_str())
        .unwrap_or("wav");

    let url = format!("{}/models/{model}:generateContent?key={api_key}", data.upstream_url);
    let payload = json!({
        "contents": [{
            "role": "user",
            "parts": [{ "text": input_text }]
        }],
        "generationConfig": {
            "responseModalities": ["AUDIO"],
            "speechConfig": {
                "voiceConfig": {
                    "prebuiltVoiceConfig": {
                        "voiceName": voice
                    }
                }
            }
        }
    });

    let mut upstream = client
        .post(&url)
        .insert_header(("Content-Type", "application/json"))
        .timeout(Duration::from_secs(300))
        .send_body(payload.to_string())
        .await
        .map_err(|e| ErrorInternalServerError(format!("Gemini TTS request failed: {e}")))?;

    let status = upstream.status();
    let body = upstream.body().limit(50 * 1024 * 1024).await?;
    if !status.is_success() {
        return Ok(HttpResponse::build(status).body(body));
    }

    let resp_json: Value = serde_json::from_slice(&body)
        .map_err(|_| ErrorInternalServerError("Invalid JSON from Gemini TTS"))?;

    if let Some((mime, raw_audio_bytes)) = extract_PCM_or_audio_from_google(&resp_json) {
        if response_format == "pcm" {
            return Ok(HttpResponse::Ok()
                .content_type("audio/pcm")
                .body(raw_audio_bytes));
        }
        let wav_bytes = if mime.contains("L16") || mime.contains("pcm") {
            wrap_pcm_in_wav(&raw_audio_bytes, 24000, 1, 16)
        } else {
            raw_audio_bytes
        };
        Ok(HttpResponse::Ok()
            .content_type("audio/wav")
            .body(wav_bytes))
    } else {
        Ok(HttpResponse::InternalServerError().body("No audio returned from upstream model"))
    }
}

/// POST /v1/videos/generations -> Maps to Veo `:predictLongRunning` and polls until completion
async fn handle_videos_generations(
    req: HttpRequest,
    body_data: web::Bytes,
    data: web::Data<AppState>,
    client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    let api_key = extract_api_key(&req).ok_or_else(|| ErrorBadRequest("No API key provided"))?;
    let json_body: Value = serde_json::from_slice(&body_data)
        .map_err(|_| ErrorBadRequest("Failed to parse JSON body"))?;

    let prompt = json_body["prompt"]
        .as_str()
        .ok_or_else(|| ErrorBadRequest("Missing prompt"))?;
    let model = json_body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("veo-3.1-generate-001");
    let aspect_ratio = json_body
        .get("aspect_ratio")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| {
            json_body
                .get("size")
                .and_then(|v| v.as_str())
                .map(size_to_aspect_ratio)
        })
        .unwrap_or_else(|| "16:9".to_string());

    let mut parameters = json!({
        "aspectRatio": aspect_ratio
    });
    if let Some(duration) = json_body
        .get("duration_seconds")
        .or_else(|| json_body.get("seconds"))
    {
        parameters["durationSeconds"] = duration.clone();
    }
    if let Some(resolution) = json_body.get("resolution").and_then(|v| v.as_str()) {
        parameters["resolution"] = json!(resolution);
    }

    let video_result = run_veo_prediction(
        &data.upstream_url,
        model,
        prompt,
        parameters,
        &api_key,
        &client,
    )
    .await?;

    Ok(HttpResponse::Ok().json(video_result))
}

async fn handle_veo_in_chat(
    model_name: &str,
    json_body: &Value,
    is_stream: bool,
    api_key: &str,
    data: &AppState,
    client: &Client,
) -> Result<HttpResponse, Error> {
    let prompt = json_body
        .get("messages")
        .and_then(|m| m.as_array())
        .and_then(|arr| arr.last())
        .and_then(|msg| msg.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("Generate a video");

    let veo_resp = run_veo_prediction(
        &data.upstream_url,
        model_name,
        prompt,
        json!({ "aspectRatio": "16:9" }),
        api_key,
        client,
    )
    .await?;

    let video_url = veo_resp["data"]
        .as_array()
        .and_then(|a| a.first())
        .and_then(|item| item.get("url"))
        .and_then(|u| u.as_str())
        .unwrap_or("");

    let content_md = format!("Generated video:\n\n[Download Video]({video_url})");
    let id = format!("chatcmpl-{}", uuid::Uuid::new_v4().simple());

    if is_stream {
        let chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": now_unix(),
            "model": model_name,
            "choices": [{
                "index": 0,
                "delta": { "role": "assistant", "content": content_md },
                "finish_reason": Value::Null
            }]
        });
        let stop_chunk = json!({
            "id": id,
            "object": "chat.completion.chunk",
            "created": now_unix(),
            "model": model_name,
            "choices": [{
                "index": 0,
                "delta": {},
                "finish_reason": "stop"
            }]
        });
        let body = format!("data: {chunk}\n\ndata: {stop_chunk}\n\ndata: [DONE]\n\n");
        Ok(HttpResponse::Ok()
            .insert_header(("Content-Type", "text/event-stream; charset=utf-8"))
            .body(body))
    } else {
        Ok(HttpResponse::Ok().json(json!({
            "id": id,
            "object": "chat.completion",
            "created": now_unix(),
            "model": model_name,
            "choices": [{
                "index": 0,
                "message": { "role": "assistant", "content": content_md },
                "finish_reason": "stop"
            }]
        })))
    }
}

async fn run_veo_prediction(
    base_url: &str,
    model: &str,
    prompt: &str,
    parameters: Value,
    api_key: &str,
    client: &Client,
) -> Result<Value, Error> {
    let start_url = format!("{base_url}/models/{model}:predictLongRunning?key={api_key}");
    let payload = json!({
        "instances": [{ "prompt": prompt }],
        "parameters": parameters
    });

    let mut start_resp = client
        .post(&start_url)
        .insert_header(("Content-Type", "application/json"))
        .timeout(Duration::from_secs(60))
        .send_body(payload.to_string())
        .await
        .map_err(|e| ErrorInternalServerError(format!("Failed to start Veo job: {e}")))?;

    let start_body = start_resp.body().await?;
    let start_json: Value = serde_json::from_slice(&start_body)
        .map_err(|_| ErrorInternalServerError("Invalid JSON from Veo predictLongRunning"))?;

    let op_name = start_json["name"].as_str().ok_or_else(|| {
        ErrorInternalServerError(format!("Veo did not return operation name: {start_json}"))
    })?;

    let poll_url = format!("{base_url}/{op_name}?key={api_key}");
    for _ in 0..72 {
        actix_web::rt::time::sleep(Duration::from_secs(5)).await;
        let mut poll_resp = client
            .get(&poll_url)
            .timeout(Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| ErrorInternalServerError(format!("Failed polling Veo operation: {e}")))?;

        let poll_body = poll_resp.body().await?;
        let poll_json: Value = serde_json::from_slice(&poll_body)
            .map_err(|_| ErrorInternalServerError("Invalid JSON while polling Veo"))?;

        if poll_json.get("done").and_then(|d| d.as_bool()) == Some(true) {
            let mut videos = Vec::new();
            if let Some(samples) = poll_json
                .pointer("/response/generateVideoResponse/generatedSamples")
                .and_then(|s| s.as_array())
            {
                for sample in samples {
                    if let Some(uri) = sample.pointer("/video/uri").and_then(|u| u.as_str()) {
                        let sep = if uri.contains('?') { "&" } else { "?" };
                        let authenticated_uri = format!("{uri}{sep}key={api_key}");
                        let mut item = json!({ "url": uri });
                        if let Ok(mut dl_resp) = client
                            .get(&authenticated_uri)
                            .timeout(Duration::from_secs(120))
                            .send()
                            .await
                        {
                            if dl_resp.status().is_success() {
                                if let Ok(video_bytes) =
                                    dl_resp.body().limit(100 * 1024 * 1024).await
                                {
                                    item["b64_json"] = json!(STANDARD.encode(&video_bytes));
                                }
                            }
                        }
                        videos.push(item);
                    }
                }
            }
            return Ok(json!({
                "created": now_unix(),
                "operation": op_name,
                "data": videos
            }));
        }
    }

    Err(ErrorInternalServerError("Veo video generation timed out"))
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}