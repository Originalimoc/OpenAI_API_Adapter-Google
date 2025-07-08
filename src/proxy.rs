use crate::app_state::AppState;
use crate::transformers::{transform_google_stream_to_openai, transform_google_to_openai, transform_openai_to_google};
use crate::utils::extract_api_key;
use actix_web::{error::{ErrorInternalServerError, ErrorBadRequest}, web, Error, HttpMessage, HttpRequest, HttpResponse};
use awc::Client;
use futures_util::stream::{StreamExt, TryStreamExt};
use serde_json::Value;

/// Thinking will be enabled by name if matched by thinking_enabled_models
pub struct ThinkingConfig {
    pub enabled: bool,
    pub budget: Option<i64>,
}

pub async fn reverse_proxy(
    req: HttpRequest,
    body_data: web::Bytes,
    data: web::Data<AppState>,
    client: web::Data<Client>,
) -> Result<HttpResponse, Error> {
    if !req.path().starts_with("/v1/chat/completions") {
        return Ok(HttpResponse::NotFound().body("Not Found"));
    }

    log::info!("Got request: {}", String::from_utf8_lossy(&body_data));

    let json_body: Value = serde_json::from_slice(&body_data)
        .map_err(|_| ErrorInternalServerError("Failed to parse JSON body"))?;

    let is_stream = json_body.get("stream").and_then(|v| v.as_bool()).unwrap_or(false);
    let api_key = extract_api_key(&req)
        .ok_or_else(|| ErrorBadRequest("No API key provided"))?;

    let model_name_in_request = json_body["model"].as_str()
        .ok_or_else(|| ErrorBadRequest("Model not found in request"))?;
    let no_thought_process = model_name_in_request.ends_with("-no-thought-process");
    let model_name = if no_thought_process {
        model_name_in_request.trim_end_matches("-no-thought-process")
    } else {
        model_name_in_request
    };

    let thinking_enabled_models = ["gemini-2.0-flash-thinking", "gemini-2.5"];
    let mut must_think_models = std::collections::HashMap::new();
    must_think_models.insert("gemini-2.5-pro", 128); // 128 is minimal settable
    let thinking_enabled = thinking_enabled_models.iter().any(|thinking_enabled_model| model_name_in_request.contains(thinking_enabled_model));
    let thinking_budget_in_request: Option<i64> = json_body
        .get("reasoning_effort")
        .and_then(|v| v.as_str())
        .map(|s| match s {
            "low" => 1_024,
            "medium" => 8_192,
            "high" => 24_576,
            "none" => {
                let found_min_budget = must_think_models
                    .iter()
                    .find_map(|(must_think_model_base_name, &min_budget)| {
                        if model_name_in_request.contains(must_think_model_base_name) {
                            Some(min_budget as i64)
                        } else {
                            None
                        }
                    });
                found_min_budget.unwrap_or(0)
            },
            _ => 1024, // Value set but not supported mode, assuming a low effort
        });
    let thinking_config = ThinkingConfig {
        enabled: thinking_enabled,
        budget: thinking_budget_in_request,
    };

    // Transform the OpenAI request to Google's format
    let google_body = transform_openai_to_google(&json_body, &client, &api_key, &thinking_config).await;

    let google_body_str = serde_json::to_string(&google_body)
        .map_err(|_| ErrorInternalServerError("Failed to serialize Google body"))?;

    log::info!("Converted request: {google_body_str}");

    let google_base_url = data.upstream_url.clone();
    let google_url = if is_stream {
        format!("{google_base_url}/models/{model_name}:streamGenerateContent?alt=sse&key={api_key}")
    } else {
        format!("{google_base_url}/models/{model_name}:generateContent?key={api_key}")
    };

    let forward_req = client.post(&google_url)
        .insert_header(("Content-Type", "application/json"));

    log::info!("Forwarding request to: {google_url}");

    match forward_req.timeout(std::time::Duration::from_secs(600)).send_body(google_body_str).await {
        Ok(mut upstream_response) => {
            let status: awc::http::StatusCode = upstream_response.status();
            let mut response = HttpResponse::build(status);

            for (name, value) in upstream_response.headers().iter() {
                if name.as_str().contains("content-encoding") {
                    log::debug!("Not copied: content-encoding = {}", value.to_str().unwrap_or("PARSE HEADER VALUE ERROR"));
                } else {
                    response.insert_header((name.clone(), value.clone()));
                }
            }

            if upstream_response.content_type().contains("text/event-stream") {
                let mut prev_is_thought = false;
                let up_stream = upstream_response.into_stream()
                .map_err(|e| {
                    log::error!("Error in stream: {e:?}");
                    ErrorInternalServerError("Error processing stream")
                })
                .map(move |result| {
                    result.and_then(|bytes| {
                        let (transformed, last_is_thought) = transform_google_stream_to_openai(Ok(bytes), no_thought_process, prev_is_thought, data.markdown_thought);
                        prev_is_thought = last_is_thought;
                        transformed
                    })
                });
                Ok(response.streaming(up_stream))
            } else {
                let body = upstream_response.body().await?;
                log::info!("Got reply from Google: {}", String::from_utf8_lossy(&body));

                let google_response: Value = serde_json::from_slice(&body)
                    .map_err(|_| ErrorInternalServerError("Failed to parse Google response"))?;
                
                // Transform the Google response back to OpenAI format
                let (openai_response, _) = transform_google_to_openai(&google_response, false, no_thought_process, false, data.markdown_thought);
                if let Some(openai_response) = openai_response {
                    log::info!("Replied to client: {openai_response}");
                    Ok(response.json(openai_response))
                } else {
                    log::error!("Non stream mode but no choices available. Replied 503 to client");
                    Ok(HttpResponse::ServiceUnavailable().body("Failed to connect to upstream server."))
                }
            }
        }
        Err(err) => {
            log::error!("Failed to forward request: {err:?}");
            Ok(HttpResponse::InternalServerError().body("Failed to connect to upstream server."))
        }
    }
}
