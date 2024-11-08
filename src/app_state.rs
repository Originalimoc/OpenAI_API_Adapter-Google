#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    pub upstream_url: String,
}

impl AppState {
    pub fn new(upstream_url: String) -> Self {
        Self {
            upstream_url,
        }
    }
}
