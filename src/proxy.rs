use crate::app_state::AppState;
use crate::transformers::{transform_google_stream_to_openai, transform_google_to_openai, transform_openai_to_google};
use crate::utils::extract_api_key;
use actix_web::{error::{ErrorInternalServerError, ErrorBadRequest}, web, Error, HttpMessage, HttpRequest, HttpResponse};
use awc::Client;
use futures_util::stream::{StreamExt, TryStreamExt};
use serde_json::Value;

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

    let model_name = json_body["model"].as_str()
        .ok_or_else(|| ErrorBadRequest("Model not found in request"))?;

    // Transform the OpenAI request to Google's format
    let google_body = transform_openai_to_google(&json_body, &client, &api_key).await;

    let google_body_str = serde_json::to_string(&google_body)
        .map_err(|_| ErrorInternalServerError("Failed to serialize Google body"))?;

    log::info!("Converted request: {}", google_body_str);

    let google_base_url = data.upstream_url.clone();
    let google_url = if is_stream {
        format!("{}/models/{}:streamGenerateContent?alt=sse&key={}", google_base_url, model_name, api_key)
    } else {
        format!("{}/models/{}:generateContent?key={}", google_base_url, model_name, api_key)
    };

    let forward_req = client.post(&google_url)
        .insert_header(("Content-Type", "application/json"));

    log::info!("Forwarding request to: {}", google_url);

    match forward_req.send_body(google_body_str).await {
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
                let up_stream = upstream_response.into_stream().map_err(|e| {
                    log::error!("Error in stream: {:?}", e);
                    ErrorInternalServerError("Error processing stream")
                }).map(|result| {
                    result.and_then(|bytes| {
                        transform_google_stream_to_openai(Ok(bytes))
                    })
                });
                Ok(response.streaming(up_stream))
            } else {
                let body = upstream_response.body().await?;
                log::info!("Got reply from Google: {}", String::from_utf8_lossy(&body));

                let google_response: Value = serde_json::from_slice(&body)
                    .map_err(|_| ErrorInternalServerError("Failed to parse Google response"))?;
                
                // Transform the Google response back to OpenAI format
                let openai_response = transform_google_to_openai(&google_response, false);
                log::info!("Replied to client: {}", openai_response);

                Ok(response.json(openai_response))
            }
        }
        Err(err) => {
            log::error!("Failed to forward request: {:?}", err);
            Ok(HttpResponse::InternalServerError().body("Failed to connect to upstream server."))
        }
    }
}
