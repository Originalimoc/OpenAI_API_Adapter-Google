#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    pub upstream_url: String,
    pub markdown_thought: bool,
}

impl AppState {
    pub fn new(upstream_url: String, markdown_thought: bool) -> Self {
        Self {
            upstream_url,
            markdown_thought,
        }
    }
}
