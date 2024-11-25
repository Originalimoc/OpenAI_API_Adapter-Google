use actix_web::{error::*, web::Bytes, Error};
use serde_json::{json, Value};
use awc::Client;
use base64::Engine;
use base64::engine::general_purpose::STANDARD;

// Extract MIME type and decode base64 to Vec<u8>
fn decode_base64_and_get_mime_type(encoded_data: &str) -> Result<(String, Vec<u8>), Error> {
    // Validate the input and ensure it starts with "data:"
    if !encoded_data.starts_with("data:") {
        return Err(ErrorBadRequest("Invalid data URI"));
    }

    // Find the position of the first comma to separate metadata and data
    let comma_pos = encoded_data.find(',').ok_or_else(|| ErrorBadRequest("Invalid data URI format"))?;

    // Extract metadata (everything before the comma) and Base64 data (after the comma)
    let metadata = &encoded_data[5..comma_pos];
    let base64_data = &encoded_data[comma_pos + 1..];

    // Split the metadata by ';' to retrieve the MIME type and encoding
    let mut parts = metadata.split(';');
    let mime_type = parts.next().ok_or_else(|| ErrorBadRequest("Missing MIME type"))?.to_string();
    let encoding = parts.next().ok_or_else(|| ErrorBadRequest("Missing encoding"))?;

    // Verify that the encoding is Base64
    if encoding != "base64" {
        return Err(ErrorBadRequest("Unsupported encoding format"));
    }

    // Decode the Base64 data
    let decoded_data = STANDARD.decode(base64_data).map_err(|_| ErrorBadRequest("Failed to decode Base64 data"))?;

    Ok((mime_type, decoded_data))
}

// Generic function for uploading files to Google
async fn upload_to_google(
    client: &Client,
    api_key: &str,
    metadata: Value,
    file_data: Vec<u8>,
    mime_type: &str,
) -> Result<String, Error> {
    let upload_url = format!("https://generativelanguage.googleapis.com/upload/v1beta/files?key={}", api_key);

    // Initiate upload
    let meta_response = client.post(&upload_url)
        .insert_header(("X-Goog-Upload-Protocol", "resumable"))
        .insert_header(("X-Goog-Upload-Command", "start"))
        .insert_header(("X-Goog-Upload-Header-Content-Length", file_data.len().to_string()))
        .insert_header(("X-Goog-Upload-Header-Content-Type", mime_type))
        .insert_header(("Content-Type", "application/json"))
        .send_body(metadata.to_string())
        .await
        .map_err(|_| ErrorInternalServerError("Failed to initiate upload"))?;

    let upload_url = meta_response.headers()
        .get("X-Goog-Upload-URL")
        .and_then(|h| h.to_str().ok())
        .ok_or(ErrorInternalServerError("Failed to get upload URL"))?;

    // Upload file
    let response = client.post(upload_url)
        .insert_header(("Content-Length", file_data.len().to_string()))
        .insert_header(("X-Goog-Upload-Offset", "0"))
        .insert_header(("X-Goog-Upload-Command", "upload, finalize"))
        .send_body(file_data)
        .await
        .map_err(|_| ErrorInternalServerError("Failed to upload file"))?
        .json::<Value>()
        .await
        .map_err(|_| ErrorInternalServerError("Failed to parse JSON response"))?;

    let file_uri = response["file"]["uri"].as_str()
        .ok_or(ErrorInternalServerError("File URI not returned"))?;
    Ok(file_uri.to_string())
}

// Public function to upload base64 encoded image
pub async fn upload_base64_image_to_google(
    client: &Client,
    api_key: &str,
    base64_data: &str,
) -> Result<(String, String), Error> {
    let (mime_type, image_data) = decode_base64_and_get_mime_type(base64_data)?;
    let metadata = json!({"file": {"display_name": "uploaded_image"}});
    upload_to_google(client, api_key, metadata, image_data, &mime_type).await.map(|r| (mime_type, r))
}

// Public function to upload base64 encoded audio
pub async fn upload_base64_audio_to_google(
    client: &Client,
    api_key: &str,
    base64_data: &str,
) -> Result<(String, String), Error> {
    let (mime_type, audio_data) = decode_base64_and_get_mime_type(base64_data)?;
    let metadata = json!({"file": {"display_name": "uploaded_audio"}});
    upload_to_google(client, api_key, metadata, audio_data, &mime_type).await.map(|r| (mime_type, r))
}

pub fn transform_google_stream_to_openai(data: Result<Bytes, Error>) -> Result<Bytes, Error> {
    data.and_then(|bytes| {
        let input = String::from_utf8_lossy(&bytes);
        log::info!("Got streaming data: {}", input);

        let events: Vec<&str> = input.split("\n\n").filter(|s| !s.is_empty()).collect();

        let mut output = Vec::new();

        for event in events {
            if event.trim() == "data: [DONE]" {
                output.push("data: [DONE]\n\n".to_string());
            } else if let Some(json_str) = event.strip_prefix("data: ") {
                match serde_json::from_str::<Value>(json_str) {
                    Ok(json) => {
                        let openai_chunk = transform_google_to_openai(&json, true);
                        let transformed_event = format!(
                            "data: {}\n\n",
                            serde_json::to_string(&openai_chunk)
                                .map_err(|_| ErrorInternalServerError("Failed to serialize JSON"))?
                        );
                        output.push(transformed_event);
                    },
                    Err(e) => log::error!("Failed to parse JSON: {}", e),
                }
            }
        }

        Ok(Bytes::from(output.join("")))
    })
}


// This function should be updated to match the new requirements:
pub fn transform_google_to_openai(body: &Value, stream_mode: bool) -> Value {
    let message_type = if stream_mode { "delta" } else { "message" };
    let mut openai_response = json!({
        "id": format!("chatcmpl-{}", uuid::Uuid::new_v4().to_string().replace("-", "")),
        "object": if stream_mode { "chat.completion.chunk" } else { "chat.completion" },
        "created": body.get("created").cloned().unwrap_or(json!(0)),
        "model": body.get("modelVersion").cloned().unwrap_or(json!("")),
        "choices": []
    });

    if let Some(usage_metadata) = body.get("usageMetadata") {
        if let (Some(prompt_token_count), Some(candidates_token_count), Some(total_token_count)) = (
            usage_metadata.get("promptTokenCount"),
            usage_metadata.get("candidatesTokenCount"),
            usage_metadata.get("totalTokenCount")
        ) {
            openai_response["usage"] = json!({
                "prompt_tokens": prompt_token_count,
                "completion_tokens": candidates_token_count,
                "total_tokens": total_token_count
            });
        }
    }

    if let Some(candidates) = body.get("candidates").and_then(Value::as_array) {
        for (index, candidate) in candidates.iter().enumerate() {
            if let Some(content) = candidate.get("content") {
                if let Some(parts) = content.get("parts").and_then(Value::as_array) {
                    if let Some(text) = parts.first().and_then(|part| part.get("text").and_then(|t| t.as_str())) {
                        // Construct the message object dynamically
                        let mut message = json!({
                            "content": text
                        });
                        
                        if let Some(role) = parts.first().and_then(|part| part.get("role").and_then(|r| r.as_str())) {
                            let role = if role == "model" { "assistant" } else { role };
                            message["role"] = json!(role);
                        }

                        openai_response["choices"].as_array_mut().unwrap().push(json!({
                            message_type: message,
                            "finish_reason": candidate.get("finishReason").and_then(|r| r.as_str()).map(|r| r.to_lowercase()),
                            "index": index
                        }));
                    }
                }
            }
        }
    }
    openai_response
}

pub async fn transform_openai_to_google(body: &Value, client: &Client, api_key: &str) -> Value {
    let empty = vec![];
    let messages = body.get("messages").and_then(Value::as_array).unwrap_or(&empty);

    let mut contents = Vec::new();
    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let role = if role == "assistant" { "model" } else { role };
        if role == "system" {
            continue;
        }

        let content = msg.get("content").unwrap_or(&Value::Null);
        let parts = match content {
            Value::String(text) => {
                vec![json!({ "text": text })]
            },
            Value::Array(content_parts) => {
                let mut parts_vec = Vec::new();
                for part in content_parts {
                    match part.get("type").and_then(Value::as_str) {
                        Some("text") => {
                            if let Some(text) = part.get("text").and_then(Value::as_str) {
                                parts_vec.push(json!({ "text": text }));
                            }
                        },
                        Some("image_url") => {
                            if let Some(image_url) = part.get("image_url") {
                                let base64_data = image_url["url"].as_str().unwrap_or("");
                                match upload_base64_image_to_google(client, api_key, base64_data).await {
                                    Ok((mime, uploaded_uri)) => {
                                        log::info!("image uploaded, URI: {}", uploaded_uri);
                                        parts_vec.push(json!({
                                            "file_data": {
                                                "mime_type": mime, // Adjust based on actual MIME type
                                                "file_uri": uploaded_uri
                                            }
                                        }));
                                    },
                                    Err(e) => {
                                        log::error!("Error uploading image: {}", e);
                                    }
                                }
                            }
                        },
                        Some("input_audio") => {
                            if let Some(audio) = part.get("input_audio") {
                                let base64_data = audio["data"].as_str().unwrap_or("");
                                let _format = audio["format"].as_str().unwrap_or("wav");
                                if let Ok((mime, uploaded_uri)) = upload_base64_audio_to_google(client, api_key, base64_data).await {
                                    log::info!("audio uploaded, URI: {}", uploaded_uri);
                                    parts_vec.push(json!({
                                        "file_data": {
                                            "mime_type": mime, // Adjust MIME type as needed
                                            "file_uri": uploaded_uri
                                        }
                                    }));
                                } else {
                                    log::error!("Error uploading audio");
                                }
                            }
                        },
                        _ => {
                            // Handle unknown types or log an error
                            log::warn!("Unknown content type in message");
                        }
                    }
                }
                parts_vec
            },
            _ => Vec::new(),
        };

        contents.push(json!({
            "role": role,
            "parts": parts
        }));
    }

    let temperature = body.get("temperature").and_then(|t| t.as_f64()).unwrap_or(1.0);
    let max_tokens = body.get("max_tokens").or_else(|| body.get("max_completion_tokens")).and_then(|m| m.as_i64()).unwrap_or(8192);
    let top_p = body.get("top_p").and_then(|p| p.as_f64()).unwrap_or(1.0);
    let presence_penalty = body.get("presence_penalty").and_then(|p| p.as_f64()).unwrap_or(0.0);
    let frequency_penalty = body.get("frequency_penalty").and_then(|p| p.as_f64()).unwrap_or(0.0);
    let n_candidates = body.get("n").and_then(|m| m.as_i64()).unwrap_or(1);
    let stop_sequences = match body.get("stop") {
        Some(Value::Array(arr)) => arr.iter()
            .filter_map(|v| v.as_str())
            .map(|s| s.to_string())
            .collect::<Vec<String>>(),
        Some(Value::String(s)) => vec![s.to_string()],
        _ => Vec::new(),
    };

    let system_instruction = messages.iter().find(|msg| {
        msg.get("role").and_then(|r| r.as_str()) == Some("system")
    }).and_then(|system_msg| system_msg.get("content").and_then(|c| c.as_str()));

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

    let mut result = json!({
        "contents": contents,
        "safetySettings": safety_settings,
        "generationConfig": {
            "stopSequences": stop_sequences,
            "candidateCount": n_candidates,
            "maxOutputTokens": max_tokens,
            "temperature": temperature,
            "topP": top_p,
            "presencePenalty": presence_penalty,
            "frequencyPenalty": frequency_penalty
        },
    });
    if let Some(instruction) = system_instruction {
        result["systemInstruction"] = json!({
            "parts": [{"text": instruction}],
            "role": "system"
        });
    }
    result
}
