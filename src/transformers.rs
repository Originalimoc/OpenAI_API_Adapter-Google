use actix_web::{error::ErrorInternalServerError, web::Bytes, Error};
use serde_json::{json, Value};

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

pub fn transform_openai_to_google(body: &Value) -> Value {
    let empty = vec![];
    let messages = body.get("messages").and_then(Value::as_array).unwrap_or(&empty);

    let contents = messages.iter().map(|msg| {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("user");
        let content = msg.get("content").and_then(|c| c.as_str()).unwrap_or("");
        json!({
            "role": role,
            "parts": [{ "text": content }]
        })
    }).collect::<Vec<_>>();

    // Extract configuration from OpenAI body if available, or use defaults
    let temperature = body.get("temperature").and_then(|t| t.as_f64()).unwrap_or(1.0);
    let _presence_penalty = body.get("presence_penalty").and_then(|p| p.as_f64()).unwrap_or(0.0);
    let _frequency_penalty = body.get("frequency_penalty").and_then(|f| f.as_f64()).unwrap_or(0.0);
    let max_tokens = body.get("max_tokens").or_else(|| body.get("max_completion_tokens")).and_then(|m| m.as_i64()).unwrap_or(8192);
    let top_p = body.get("top_p").and_then(|p| p.as_f64()).unwrap_or(1.0);

    // Extract system instruction if available
    let system_instruction = messages.iter().find(|msg| {
        msg.get("role").and_then(|r| r.as_str()) == Some("system")
    }).and_then(|system_msg| system_msg.get("content").and_then(|c| c.as_str()));

    let mut result = json!({
        "contents": contents,
        "generationConfig": {
            "temperature": temperature,
			// Penalty currently not supported
            // "presencePenalty": presence_penalty,
            // "frequencyPenalty": frequency_penalty,
            "maxOutputTokens": max_tokens,
            "topP": top_p
        },
    });
	if let Some(instruction) = system_instruction {
		result["systemInstruction"] = json!({
			"parts": [{"text": instruction}],
			"role": "system"
		})
	};
	result
}
