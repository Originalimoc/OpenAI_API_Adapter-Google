use actix_web::HttpRequest;
use awc::{Client, Connector};
use rustls::ClientConfig;
use rustls_platform_verifier::BuilderVerifierExt;
use std::sync::Arc;

pub fn tls_config() -> ClientConfig {
    let arc_crypto_provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let mut cc = ClientConfig::builder_with_provider(arc_crypto_provider)
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_platform_verifier()
        .unwrap()
        .with_no_client_auth();
    cc.alpn_protocols = vec![b"h2".to_vec(), b"http/1.1".to_vec()];
    log::debug!("TLS configuration created");
    cc
}

pub fn new_request_client(connector_default_config: Arc<ClientConfig>) -> Client {
    let connector = Connector::new()
        .rustls_0_23(connector_default_config)
        .max_http_version(awc::http::Version::HTTP_2);

    let request_client = Client::builder()
        .connector(connector)
        .max_http_version(awc::http::Version::HTTP_2)
        .finish();
    log::debug!("Request client created");
    request_client
}

pub fn extract_api_key(req: &HttpRequest) -> Option<String> {
    req.headers()
        .get("Authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|auth| {
            // Use strip_prefix instead of starts_with and manual slicing
            auth.strip_prefix("Bearer ").map(|stripped| stripped.to_string())
        })
        .or_else(|| {
            req.uri()
                .query()
                .and_then(|q| q.split('=').nth(1).map(String::from))
        })
}
