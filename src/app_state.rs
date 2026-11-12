use std::collections::HashMap;
use std::sync::{Arc, Mutex};

#[derive(Clone, Debug)]
pub struct ToolCallMeta {
    pub name: String,
    pub thought_signature: Option<String>,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    pub upstream_url: String,
    pub markdown_thought: bool,
    pub tool_call_cache: Arc<Mutex<HashMap<String, ToolCallMeta>>>,
}

impl AppState {
    pub fn new(upstream_url: String, markdown_thought: bool) -> Self {
        Self {
            upstream_url,
            markdown_thought,
            tool_call_cache: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn record_tool_call(&self, id: String, name: String, thought_signature: Option<String>) {
        if let Ok(mut cache) = self.tool_call_cache.lock() {
            if cache.len() > 4096 {
                cache.clear();
            }
            cache.insert(
                id,
                ToolCallMeta {
                    name,
                    thought_signature,
                },
            );
        }
    }

    pub fn update_thought_signature(&self, id: &str, thought_signature: String) {
        if let Ok(mut cache) = self.tool_call_cache.lock() {
            if let Some(meta) = cache.get_mut(id) {
                if meta.thought_signature.is_none() {
                    meta.thought_signature = Some(thought_signature);
                }
            }
        }
    }

    pub fn get_tool_call(&self, id: &str) -> Option<ToolCallMeta> {
        self.tool_call_cache
            .lock()
            .ok()
            .and_then(|cache| cache.get(id).cloned())
    }
}