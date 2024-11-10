mod app_state;
mod cli;
mod proxy;
mod transformers;
mod utils;

use actix_web::{web::{self, PayloadConfig}, App, HttpServer};
use cli::Args;
use clap::Parser;
use proxy::reverse_proxy;
use utils::{new_request_client, tls_config};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    let args = Args::parse();
    let state = app_state::AppState::new(args.upstream_url);
    let tls_client_config = std::sync::Arc::new(tls_config());

    // Test
    let google_input = serde_json::json!( {
        "candidates": [
            {
                "content": {
                    "parts": [
                        { "text": "That's a fun question! \n\nEach dog has 4 paws, and you have 2 dogs. \n\nSo there are 4 paws x 2 dogs = **8 paws** in your house. 🐶🐾 \n" }
                    ],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0,
                "safetyRatings": [
                    { "category": "HARM_CATEGORY_SEXUALLY_EXPLICIT", "probability": "NEGLIGIBLE" },
                    { "category": "HARM_CATEGORY_HATE_SPEECH", "probability": "NEGLIGIBLE" },
                    { "category": "HARM_CATEGORY_HARASSMENT", "probability": "NEGLIGIBLE" },
                    { "category": "HARM_CATEGORY_DANGEROUS_CONTENT", "probability": "NEGLIGIBLE" }
                ]
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 32,
            "candidatesTokenCount": 47,
            "totalTokenCount": 79
        },
        "modelVersion": "gemini-1.5-flash-001",
        "created": 1653500834
    });
    let openai_response = transformers::transform_google_to_openai(&google_input, false);
    log::debug!("{}", openai_response);

    HttpServer::new(move || {
        let client = new_request_client(tls_client_config.clone());

        App::new()
            .app_data(PayloadConfig::new(1 << 31))
//            .wrap(actix_web::middleware::Compress::default())
            .app_data(web::Data::new(state.clone()))
            .app_data(web::Data::new(client))
            .route("/{path:.*}", web::to(reverse_proxy))
    })
    .bind(format!("0.0.0.0:{}", args.port))?
    .run()
    .await
}
