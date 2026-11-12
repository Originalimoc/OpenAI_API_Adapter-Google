mod app_state;
mod cli;
mod proxy;
mod transformers;
mod utils;

use actix_web::{
    web::{self, PayloadConfig},
    App, HttpServer,
};
use clap::Parser;
use cli::Args;
use proxy::reverse_proxy;
use utils::{new_request_client, tls_config};

#[actix_web::main]
async fn main() -> std::io::Result<()> {
    env_logger::init();
    let args = Args::parse();
    let state = app_state::AppState::new(args.upstream_url, args.markdown_thought);
    let tls_client_config = std::sync::Arc::new(tls_config());

    // Startup self-test
    let google_input = serde_json::json!({
        "candidates": [
            {
                "content": {
                    "parts": [
                        {
                            "text": ". **Refine the answer for clarity and to address potential nuance:** Acknowledge the dogs' paws specifically.",
                            "thought": true
                        },
                        {
                            "text": "Each dog has 4 paws, and you have 2 dogs = **8 paws**."
                        }
                    ],
                    "role": "model"
                },
                "finishReason": "STOP",
                "index": 0
            }
        ],
        "usageMetadata": {
            "promptTokenCount": 32,
            "candidatesTokenCount": 47,
            "totalTokenCount": 79
        },
        "modelVersion": "gemini-2.5-flash",
        "created": 1653500834
    });
    let openai_response =
        transformers::transform_google_to_openai(&google_input, false, false, true, false, None);
    log::debug!("{openai_response:?}");

    HttpServer::new(move || {
        let client = new_request_client(tls_client_config.clone());

        App::new()
//          .wrap(actix_web::middleware::Compress::default()) // breaks streaming
            .app_data(PayloadConfig::new(1 << 31)) // for loading big media payloads
            .app_data(web::Data::new(state.clone()))
            .app_data(web::Data::new(client))
            .route("/{path:.*}", web::to(reverse_proxy))
    })
    .bind(format!("0.0.0.0:{}", args.port))?
    .run()
    .await
}