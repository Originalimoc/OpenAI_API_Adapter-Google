use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

const CACHE_FILE_PREFIX: &str = ".gemini_adapter_cache";
const FALLBACK_SIG: &str = "skip_thought_signature_validator";
const FILE_CACHE_TTL_SECS: u64 = 40 * 3600;

#[derive(Clone, Debug)]
pub struct ToolCallMeta {
    pub name: String,
    pub canonical_args: String,
    pub thought_signature: Option<String>,
    pub thought_text: Option<String>,
}

#[derive(Clone, Debug)]
pub struct TextTurnMeta {
    pub thought_signature: Option<String>,
    pub thought_text: Option<String>,
}

#[derive(Clone, Debug)]
pub struct UploadedFileMeta {
    pub mime_type: String,
    pub file_uri: String,
    pub uploaded_at: u64,
}

#[derive(Default)]
pub struct CacheStore {
    pub tool_by_id: HashMap<String, ToolCallMeta>,
    pub tool_by_pos_key: HashMap<String, ToolCallMeta>,
    pub text_turns: HashMap<u64, TextTurnMeta>,
    pub uploaded_files: HashMap<u64, UploadedFileMeta>,
    pub prev_contents_hashes: Vec<u64>,
    pub file_mtimes: HashMap<PathBuf, SystemTime>,
}

#[allow(dead_code)]
#[derive(Clone)]
pub struct AppState {
    pub upstream_url: String,
    pub markdown_thought: bool,
    pub store: Arc<Mutex<CacheStore>>,
    pub own_cache_path: PathBuf,
    pub cache_dir: PathBuf,
}

pub fn hash_str(s: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.len().hash(&mut hasher);
    s.hash(&mut hasher);
    hasher.finish()
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn is_real_signature(sig: &Option<String>) -> bool {
    sig.as_deref()
        .map(|s| !s.is_empty() && s != FALLBACK_SIG)
        .unwrap_or(false)
}

fn merge_tool_meta(existing: &mut ToolCallMeta, incoming: ToolCallMeta) {
    if !is_real_signature(&existing.thought_signature)
        && is_real_signature(&incoming.thought_signature)
    {
        existing.thought_signature = incoming.thought_signature;
    } else if existing.thought_signature.is_none() && incoming.thought_signature.is_some() {
        existing.thought_signature = incoming.thought_signature;
    }

    if existing.thought_text.is_none() && incoming.thought_text.is_some() {
        existing.thought_text = incoming.thought_text;
    }
}

fn merge_text_meta(existing: &mut TextTurnMeta, incoming: TextTurnMeta) {
    if !is_real_signature(&existing.thought_signature)
        && is_real_signature(&incoming.thought_signature)
    {
        existing.thought_signature = incoming.thought_signature;
    } else if existing.thought_signature.is_none() && incoming.thought_signature.is_some() {
        existing.thought_signature = incoming.thought_signature;
    }

    if existing.thought_text.is_none() && incoming.thought_text.is_some() {
        existing.thought_text = incoming.thought_text;
    }
}

impl AppState {
    pub fn new(
        upstream_url: String,
        markdown_thought: bool,
        port: u16,
        cache_id: Option<String>,
    ) -> Self {
        let instance_tag = cache_id.unwrap_or_else(|| {
            let host = std::env::var("HOSTNAME")
                .or_else(|_| std::env::var("COMPUTERNAME"))
                .unwrap_or_else(|_| "local".to_string());
            let clean_host: String = host
                .chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
                .collect();
            if clean_host.is_empty() {
                format!("{port}")
            } else {
                format!("{clean_host}_{port}")
            }
        });

        let cache_dir = PathBuf::from(".");
        let own_cache_path = cache_dir.join(format!("{CACHE_FILE_PREFIX}_{instance_tag}.json"));

        log::info!(
            "Cache initialized: writing to {:?}, reading all {:?}/{CACHE_FILE_PREFIX}*.json",
            own_cache_path,
            cache_dir
        );

        let state = Self {
            upstream_url,
            markdown_thought,
            store: Arc::new(Mutex::new(CacheStore::default())),
            own_cache_path,
            cache_dir,
        };
        state.sync_all_from_disk();
        state
    }

    /// Scans `cache_dir` for all `.gemini_adapter_cache*.json` files and merges any new or modified files.
    pub fn sync_all_from_disk(&self) {
        let mut s = match self.store.lock() {
            Ok(guard) => guard,
            Err(_) => return,
        };
        Self::sync_store_from_dir(&self.cache_dir, &mut s);
    }

    fn sync_store_from_dir(dir: &Path, store: &mut CacheStore) {
        let entries = match fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        let mut loaded_any = false;
        for entry in entries.flatten() {
            let path = entry.path();
            let file_name = match path.file_name().and_then(|n| n.to_str()) {
                Some(n) => n,
                None => continue,
            };

            // Match `.gemini_adapter_cache.json` and `.gemini_adapter_cache_*.json`, skip `.tmp` files
            if !file_name.starts_with(CACHE_FILE_PREFIX) || !file_name.ends_with(".json") {
                continue;
            }

            let mtime = entry
                .metadata()
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);

            if store.file_mtimes.get(&path) == Some(&mtime) {
                continue;
            }

            if Self::merge_file_into_store(&path, store) {
                store.file_mtimes.insert(path, mtime);
                loaded_any = true;
            }
        }

        if loaded_any {
            log::debug!(
                "Synced cache files: {} tool calls, {} text turns, {} uploaded files",
                store.tool_by_id.len(),
                store.text_turns.len(),
                store.uploaded_files.len()
            );
        }
    }

    fn merge_file_into_store(path: &Path, store: &mut CacheStore) -> bool {
        let raw = match fs::read_to_string(path) {
            Ok(r) => r,
            Err(_) => return false,
        };
        let root: Value = match serde_json::from_str(&raw) {
            Ok(v) => v,
            Err(_) => return false,
        };

        if let Some(tools) = root.get("tools").and_then(Value::as_array) {
            for item in tools {
                let id = item.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                let name = item.get("name").and_then(Value::as_str).unwrap_or("").to_string();
                let args = item.get("args").and_then(Value::as_str).unwrap_or("{}").to_string();
                let sig = item
                    .get("sig")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());
                let thought = item
                    .get("thought")
                    .and_then(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(|s| s.to_string());

                if !id.is_empty() && !name.is_empty() {
                    let incoming = ToolCallMeta {
                        name,
                        canonical_args: args,
                        thought_signature: sig,
                        thought_text: thought,
                    };
                    store
                        .tool_by_id
                        .entry(id)
                        .and_modify(|existing| merge_tool_meta(existing, incoming.clone()))
                        .or_insert(incoming);
                }
            }
        }

        if let Some(texts) = root.get("texts").and_then(Value::as_array) {
            for item in texts {
                if let Some(h) = item.get("hash").and_then(Value::as_u64) {
                    let sig = item
                        .get("sig")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());
                    let thought = item
                        .get("thought")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(|s| s.to_string());

                    let incoming = TextTurnMeta {
                        thought_signature: sig,
                        thought_text: thought,
                    };
                    store
                        .text_turns
                        .entry(h)
                        .and_modify(|existing| merge_text_meta(existing, incoming.clone()))
                        .or_insert(incoming);
                }
            }
        }

        let now = now_secs();
        if let Some(files) = root.get("files").and_then(Value::as_array) {
            for item in files {
                if let (Some(h), Some(mime), Some(uri), Some(ts)) = (
                    item.get("hash").and_then(Value::as_u64),
                    item.get("mime").and_then(Value::as_str),
                    item.get("uri").and_then(Value::as_str),
                    item.get("ts").and_then(Value::as_u64),
                ) {
                    if now.saturating_sub(ts) < FILE_CACHE_TTL_SECS {
                        store.uploaded_files.entry(h).or_insert(UploadedFileMeta {
                            mime_type: mime.to_string(),
                            file_uri: uri.to_string(),
                            uploaded_at: ts,
                        });
                    }
                }
            }
        }

        true
    }

    fn save_to_disk(own_path: &Path, store: &mut CacheStore) {
        // Only persist entries that carry real signatures or non-empty thoughts so fallback
        // placeholders never pollute sibling instances.
        let tools_json: Vec<Value> = store
            .tool_by_id
            .iter()
            .filter(|(_, m)| is_real_signature(&m.thought_signature) || m.thought_text.is_some())
            .map(|(id, m)| {
                json!({
                    "id": id,
                    "name": m.name,
                    "args": m.canonical_args,
                    "sig": m.thought_signature,
                    "thought": m.thought_text
                })
            })
            .collect();

        let text_json: Vec<Value> = store
            .text_turns
            .iter()
            .map(|(k, m)| {
                json!({
                    "hash": k,
                    "sig": m.thought_signature,
                    "thought": m.thought_text
                })
            })
            .collect();

        let files_json: Vec<Value> = store
            .uploaded_files
            .iter()
            .map(|(k, m)| {
                json!({
                    "hash": k,
                    "mime": m.mime_type,
                    "uri": m.file_uri,
                    "ts": m.uploaded_at
                })
            })
            .collect();

        let root = json!({
            "tools": tools_json,
            "texts": text_json,
            "files": files_json
        });

        if let Ok(serialized) = serde_json::to_string(&root) {
            let tmp_path = own_path.with_extension(format!("json.tmp.{}", std::process::id()));
            if fs::write(&tmp_path, serialized).is_ok() {
                if fs::rename(&tmp_path, own_path).is_ok() {
                    if let Ok(mtime) = fs::metadata(own_path).and_then(|m| m.modified()) {
                        store.file_mtimes.insert(own_path.to_path_buf(), mtime);
                    }
                } else {
                    let _ = fs::remove_file(&tmp_path);
                }
            }
        }
    }

    pub fn record_tool_call(
        &self,
        id: String,
        name: String,
        canonical_args: String,
        thought_signature: Option<String>,
        thought_text: Option<String>,
    ) {
        if let Ok(mut s) = self.store.lock() {
            if s.tool_by_id.len() > 8192 {
                s.tool_by_id.clear();
                s.tool_by_pos_key.clear();
            }
            let incoming = ToolCallMeta {
                name,
                canonical_args,
                thought_signature,
                thought_text: thought_text.filter(|t| !t.is_empty()),
            };
            s.tool_by_id
                .entry(id)
                .and_modify(|existing| merge_tool_meta(existing, incoming.clone()))
                .or_insert(incoming);
            Self::save_to_disk(&self.own_cache_path, &mut s);
        }
    }

    pub fn update_tool_call_meta(
        &self,
        id: &str,
        thought_signature: Option<String>,
        thought_text: Option<String>,
    ) {
        if let Ok(mut s) = self.store.lock() {
            if let Some(meta) = s.tool_by_id.get_mut(id) {
                if is_real_signature(&thought_signature)
                    && !is_real_signature(&meta.thought_signature)
                {
                    meta.thought_signature = thought_signature;
                }
                if let Some(tt) = thought_text.filter(|t| !t.is_empty()) {
                    if meta.thought_text.is_none() {
                        meta.thought_text = Some(tt);
                    }
                }
                Self::save_to_disk(&self.own_cache_path, &mut s);
            }
        }
    }

    pub fn get_or_pin_tool_call(
        &self,
        id: &str,
        msg_idx: usize,
        tc_idx: usize,
        name: &str,
        canonical_args: &str,
        fallback_sig: &str,
        fallback_thought: Option<String>,
    ) -> ToolCallMeta {
        let mut s = match self.store.lock() {
            Ok(guard) => guard,
            Err(_) => {
                return ToolCallMeta {
                    name: name.to_string(),
                    canonical_args: canonical_args.to_string(),
                    thought_signature: Some(fallback_sig.to_string()),
                    thought_text: fallback_thought,
                }
            }
        };

        if !id.is_empty() {
            if let Some(meta) = s.tool_by_id.get(id) {
                if is_real_signature(&meta.thought_signature) {
                    return meta.clone();
                }
            }
        }

        // On miss (or if only a fallback signature was present), sync sibling cache files from disk
        Self::sync_store_from_dir(&self.cache_dir, &mut s);

        if !id.is_empty() {
            if let Some(meta) = s.tool_by_id.get(id) {
                return meta.clone();
            }
        }

        let pos_key = format!("{msg_idx}:{tc_idx}:{name}:{canonical_args}");
        if let Some(meta) = s.tool_by_pos_key.get(&pos_key) {
            return meta.clone();
        }

        let pinned = ToolCallMeta {
            name: name.to_string(),
            canonical_args: canonical_args.to_string(),
            thought_signature: Some(fallback_sig.to_string()),
            thought_text: fallback_thought.filter(|t| !t.is_empty()),
        };

        if !id.is_empty() {
            s.tool_by_id.insert(id.to_string(), pinned.clone());
        }
        s.tool_by_pos_key.insert(pos_key, pinned.clone());

        if is_real_signature(&pinned.thought_signature) || pinned.thought_text.is_some() {
            Self::save_to_disk(&self.own_cache_path, &mut s);
        }
        pinned
    }

    pub fn get_tool_name_by_id(&self, id: &str) -> Option<String> {
        let mut s = self.store.lock().ok()?;
        if let Some(meta) = s.tool_by_id.get(id) {
            return Some(meta.name.clone());
        }
        Self::sync_store_from_dir(&self.cache_dir, &mut s);
        s.tool_by_id.get(id).map(|m| m.name.clone())
    }

    pub fn record_text_turn(
        &self,
        answer_text: &str,
        thought_signature: Option<String>,
        thought_text: Option<String>,
    ) {
        if answer_text.is_empty() && thought_signature.is_none() && thought_text.is_none() {
            return;
        }
        if let Ok(mut s) = self.store.lock() {
            if s.text_turns.len() > 4096 {
                s.text_turns.clear();
            }
            let key = hash_str(answer_text);
            let incoming = TextTurnMeta {
                thought_signature,
                thought_text: thought_text.filter(|t| !t.is_empty()),
            };
            s.text_turns
                .entry(key)
                .and_modify(|existing| merge_text_meta(existing, incoming.clone()))
                .or_insert(incoming);
            Self::save_to_disk(&self.own_cache_path, &mut s);
        }
    }

    pub fn get_text_turn(&self, answer_text: &str) -> Option<TextTurnMeta> {
        let mut s = self.store.lock().ok()?;
        let key = hash_str(answer_text);
        if let Some(meta) = s.text_turns.get(&key) {
            return Some(meta.clone());
        }
        Self::sync_store_from_dir(&self.cache_dir, &mut s);
        s.text_turns.get(&key).cloned()
    }

    pub fn get_cached_file(&self, base64_data: &str) -> Option<(String, String)> {
        let mut s = self.store.lock().ok()?;
        let key = hash_str(base64_data);
        let now = now_secs();
        if let Some(meta) = s.uploaded_files.get(&key) {
            if now.saturating_sub(meta.uploaded_at) < FILE_CACHE_TTL_SECS {
                return Some((meta.mime_type.clone(), meta.file_uri.clone()));
            }
        }
        Self::sync_store_from_dir(&self.cache_dir, &mut s);
        if let Some(meta) = s.uploaded_files.get(&key) {
            if now.saturating_sub(meta.uploaded_at) < FILE_CACHE_TTL_SECS {
                return Some((meta.mime_type.clone(), meta.file_uri.clone()));
            }
        }
        None
    }

    pub fn record_cached_file(&self, base64_data: &str, mime_type: String, file_uri: String) {
        if let Ok(mut s) = self.store.lock() {
            if s.uploaded_files.len() > 2048 {
                s.uploaded_files.clear();
            }
            let key = hash_str(base64_data);
            s.uploaded_files.insert(
                key,
                UploadedFileMeta {
                    mime_type,
                    file_uri,
                    uploaded_at: now_secs(),
                },
            );
            Self::save_to_disk(&self.own_cache_path, &mut s);
        }
    }

    pub fn verify_prefix_stability(&self, contents: &[Value]) {
        let current_hashes: Vec<u64> = contents
            .iter()
            .map(|c| hash_str(&c.to_string()))
            .collect();

        if let Ok(mut s) = self.store.lock() {
            if !s.prev_contents_hashes.is_empty() {
                let common_len = s.prev_contents_hashes.len().min(current_hashes.len());
                let mut first_diff = None;
                for i in 0..common_len {
                    if s.prev_contents_hashes[i] != current_hashes[i] {
                        first_diff = Some(i);
                        break;
                    }
                }
                match first_diff {
                    Some(idx) => {
                        let role = contents
                            .get(idx)
                            .and_then(|c| c.get("role"))
                            .and_then(Value::as_str)
                            .unwrap_or("unknown");
                        log::warn!(
                            "Prefix Check: DIVERGENCE at contents[{}] (role={}) out of {} prev blocks (curr={} blocks)",
                            idx,
                            role,
                            s.prev_contents_hashes.len(),
                            current_hashes.len()
                        );
                    }
                    None => {
                        log::info!(
                            "Prefix Check: 100% HIT across all {}/{} historical content blocks (curr={} blocks)",
                            common_len,
                            s.prev_contents_hashes.len(),
                            current_hashes.len()
                        );
                    }
                }
            }
            s.prev_contents_hashes = current_hashes;
        }
    }
}