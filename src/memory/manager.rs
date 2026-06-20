use std::path::{Path, PathBuf};
use std::sync::Arc;

use rusqlite::Connection;
use zerocopy::{FromBytes, IntoBytes};

use crate::message::{Conversation, Message, Role, ToolCall};
use crate::provider::{ChatRequest, Provider, ProviderError, ToolChoice};

use super::embedders::{EmbedError, Embedder};
use super::schema;

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub project_root: String,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct CodeChunk {
    pub file_path: String,
    pub chunk_text: String,
    pub start_line: i64,
    pub end_line: i64,
    pub distance: f64,
}

#[derive(Debug, thiserror::Error)]
pub enum MemoryError {
    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),
    #[error("embedding error: {0}")]
    Embed(#[from] EmbedError),
    #[error("provider error during compression: {0}")]
    Provider(#[from] ProviderError),
    #[error("task join error: {0}")]
    Join(#[from] tokio::task::JoinError),
    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}

/// Tokens are stored compactly in SQLite as a single TEXT blob of the
/// joined tool_calls JSON, since SQLite has no native JSON column type.
fn serialize_tool_calls(calls: &[ToolCall]) -> Option<String> {
    if calls.is_empty() {
        None
    } else {
        serde_json::to_string(calls).ok()
    }
}

fn role_to_str(role: &Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn role_from_str(s: &str) -> Role {
    match s {
        "system" => Role::System,
        "assistant" => Role::Assistant,
        "tool" => Role::Tool,
        _ => Role::User,
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let norm_a = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

pub struct MemoryManager {
    conn: Arc<std::sync::Mutex<Connection>>,
    embedder: Arc<dyn Embedder>,
}

impl MemoryManager {
    pub fn open(db_path: &Path, embedder: Arc<dyn Embedder>) -> Result<Self, MemoryError> {
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = schema::open(db_path)?;
        Ok(MemoryManager { conn: Arc::new(std::sync::Mutex::new(conn)), embedder })
    }

    pub fn default_path() -> Result<PathBuf, MemoryError> {
        let dir = dirs::data_dir().ok_or_else(|| std::io::Error::other("could not determine platform data directory"))?;
        Ok(dir.join("cog").join("memory.db"))
    }

    pub fn count_tokens(&self, messages: &[Message]) -> usize {
        messages.iter().map(Message::approx_tokens).sum()
    }

    /// Triggers at 80% of `conversation.token_budget`: summarizes the
    /// middle of the conversation (everything except a leading system
    /// message and the last 5 messages) into one ~200-word note via `llm`,
    /// replacing that slice in place. Returns `Ok(false)` with no DB/LLM
    /// call if under budget.
    pub async fn compress_if_needed(&self, conversation: &mut Conversation, llm: &dyn Provider, model: &str) -> Result<bool, MemoryError> {
        let total = self.count_tokens(&conversation.messages);
        if (total as f64) <= (conversation.token_budget as f64) * 0.8 {
            return Ok(false);
        }

        let len = conversation.messages.len();
        let keep_tail = 5.min(len);
        let skip_head = if conversation.messages.first().map(|m| m.role == Role::System).unwrap_or(false) { 1 } else { 0 };
        if skip_head + keep_tail >= len {
            // Nothing meaningful in the "middle" to summarize.
            return Ok(false);
        }

        let middle = &conversation.messages[skip_head..len - keep_tail];
        let excerpt: String = middle
            .iter()
            .map(|m| format!("{}: {}", role_to_str(&m.role), m.content.as_deref().unwrap_or("")))
            .collect::<Vec<_>>()
            .join("\n");

        let req = ChatRequest {
            model: model.to_string(),
            messages: vec![
                Message::system("Summarize the following conversation excerpt in about 200 words, preserving decisions made and facts established."),
                Message::user(excerpt),
            ],
            tools: None,
            tool_choice: None::<ToolChoice>,
            stream: false,
            temperature: None,
            max_tokens: None,
        };
        let resp = llm.chat(&req).await?;
        let summary = resp.message.content.unwrap_or_default();

        let mut new_messages = Vec::with_capacity(skip_head + 1 + keep_tail);
        new_messages.extend_from_slice(&conversation.messages[..skip_head]);
        new_messages.push(Message::system(format!("[Project Status summary of earlier conversation]\n{summary}")));
        new_messages.extend_from_slice(&conversation.messages[len - keep_tail..]);
        conversation.messages = new_messages;
        conversation.recompute_estimate();

        Ok(true)
    }

    pub async fn save_message(&self, session_id: &str, msg: &Message) -> Result<(), MemoryError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();
        let role = role_to_str(&msg.role).to_string();
        let content = msg.content.clone();
        let tool_calls = serialize_tool_calls(&msg.tool_calls);
        let token_count = msg.approx_tokens() as i64;
        let timestamp = now_unix();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO messages(session_id, role, content, tool_calls, token_count, timestamp) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                rusqlite::params![session_id, role, content, tool_calls, token_count, timestamp],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn load_recent(&self, session_id: &str, limit: usize) -> Result<Vec<Message>, MemoryError> {
        let conn = self.conn.clone();
        let session_id = session_id.to_string();

        let rows = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT role, content, tool_calls FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let mut rows = stmt.query(rusqlite::params![session_id, limit as i64])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                let role: String = row.get(0)?;
                let content: Option<String> = row.get(1)?;
                let tool_calls_json: Option<String> = row.get(2)?;
                let tool_calls: Vec<ToolCall> = tool_calls_json.and_then(|s| serde_json::from_str(&s).ok()).unwrap_or_default();
                out.push(Message { role: role_from_str(&role), content, tool_calls, tool_call_id: None, name: None });
            }
            out.reverse();
            Ok::<Vec<Message>, rusqlite::Error>(out)
        })
        .await??;
        Ok(rows)
    }

    pub async fn list_sessions(&self) -> Result<Vec<Session>, MemoryError> {
        let conn = self.conn.clone();
        let sessions = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT id, project_root, created_at FROM sessions ORDER BY created_at DESC")?;
            let mut rows = stmt.query([])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(Session { id: row.get(0)?, project_root: row.get(1)?, created_at: row.get(2)? });
            }
            Ok::<Vec<Session>, rusqlite::Error>(out)
        })
        .await??;
        Ok(sessions)
    }

    pub async fn create_session(&self, id: &str, project_root: &str) -> Result<(), MemoryError> {
        let conn = self.conn.clone();
        let id = id.to_string();
        let project_root = project_root.to_string();
        let created_at = now_unix();
        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT OR IGNORE INTO sessions(id, project_root, created_at) VALUES (?1, ?2, ?3)",
                rusqlite::params![id, project_root, created_at],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn remember(&self, key: &str, value: &str) -> Result<(), MemoryError> {
        let embedding = self.embedder.embed(&[value.to_string()]).await?.into_iter().next().unwrap_or_default();
        let bytes = embedding.as_bytes().to_vec();

        let conn = self.conn.clone();
        let key = key.to_string();
        let value = value.to_string();
        let timestamp = now_unix();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.execute(
                "INSERT INTO facts(key, value, embedding, last_updated) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value, embedding = excluded.embedding, last_updated = excluded.last_updated",
                rusqlite::params![key, value, bytes, timestamp],
            )?;
            Ok::<(), rusqlite::Error>(())
        })
        .await??;
        Ok(())
    }

    pub async fn delete_fact(&self, key: &str) -> Result<bool, MemoryError> {
        let conn = self.conn.clone();
        let key = key.to_string();
        let deleted = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let n = conn.execute("DELETE FROM facts WHERE key = ?1", [key])?;
            Ok::<usize, rusqlite::Error>(n)
        })
        .await??;
        Ok(deleted > 0)
    }

    pub async fn list_facts(&self, limit: usize) -> Result<Vec<(String, String)>, MemoryError> {
        let conn = self.conn.clone();
        let facts = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare("SELECT key, value FROM facts ORDER BY last_updated DESC LIMIT ?1")?;
            let mut rows = stmt.query([limit as i64])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
            }
            Ok::<Vec<(String, String)>, rusqlite::Error>(out)
        })
        .await??;
        Ok(facts)
    }

    pub async fn count_facts(&self) -> Result<usize, MemoryError> {
        let conn = self.conn.clone();
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM facts", [], |row| row.get::<_, i64>(0))
        })
        .await??;
        Ok(count as usize)
    }

    pub async fn count_code_chunks(&self) -> Result<usize, MemoryError> {
        let conn = self.conn.clone();
        let count = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            conn.query_row("SELECT COUNT(*) FROM code_chunks_meta", [], |row| row.get::<_, i64>(0))
        })
        .await??;
        Ok(count as usize)
    }

    /// Substring matches over `value` are ranked first (a cheap, precise
    /// signal beats embedding fuzziness for a small fact store), merged
    /// with an in-memory cosine-similarity pass — sqlite-vec KNN only
    /// works against a `vec0` virtual table, and a second one isn't worth
    /// it for a table expected to hold dozens-to-hundreds of rows.
    pub async fn recall(&self, query: &str, limit: usize) -> Result<Vec<(String, String)>, MemoryError> {
        let query_embedding = self.embedder.embed(&[query.to_string()]).await?.into_iter().next().unwrap_or_default();

        let conn = self.conn.clone();
        let like_pattern = format!("%{query}%");

        let (substring_matches, all_facts): (Vec<(String, String)>, Vec<(String, String, Vec<f32>)>) = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();

            let mut stmt = conn.prepare("SELECT key, value FROM facts WHERE value LIKE ?1")?;
            let mut rows = stmt.query([&like_pattern])?;
            let mut substring_matches = Vec::new();
            while let Some(row) = rows.next()? {
                substring_matches.push((row.get::<_, String>(0)?, row.get::<_, String>(1)?));
            }

            let mut stmt = conn.prepare("SELECT key, value, embedding FROM facts")?;
            let mut rows = stmt.query([])?;
            let mut all_facts = Vec::new();
            while let Some(row) = rows.next()? {
                let key: String = row.get(0)?;
                let value: String = row.get(1)?;
                let blob: Option<Vec<u8>> = row.get(2)?;
                let embedding = blob.and_then(|b| <[f32]>::ref_from_bytes(&b).ok().map(|s| s.to_vec())).unwrap_or_default();
                all_facts.push((key, value, embedding));
            }

            Ok::<_, rusqlite::Error>((substring_matches, all_facts))
        })
        .await??;

        let mut seen: std::collections::HashSet<String> = substring_matches.iter().map(|(k, _)| k.clone()).collect();
        let mut results = substring_matches;

        let mut ranked: Vec<(f32, String, String)> = all_facts
            .into_iter()
            .filter(|(k, _, _)| !seen.contains(k))
            .filter(|(_, _, emb)| !emb.is_empty())
            .map(|(k, v, emb)| (cosine_similarity(&query_embedding, &emb), k, v))
            .collect();
        ranked.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));

        for (_, key, value) in ranked {
            if results.len() >= limit {
                break;
            }
            if seen.insert(key.clone()) {
                results.push((key, value));
            }
        }
        results.truncate(limit);
        Ok(results)
    }

    pub async fn index_file(&self, path: &Path) -> Result<usize, MemoryError> {
        let source = std::fs::read_to_string(path)?;
        let Some(lang) = crate::tools::lang_spec_for(path) else { return Ok(0) };

        let mut parser = tree_sitter::Parser::new();
        if parser.set_language(&lang.language).is_err() {
            return Ok(0);
        }
        let Some(tree) = parser.parse(&source, None) else { return Ok(0) };

        let mut chunks: Vec<(String, i64, i64)> = Vec::new();
        collect_chunks(tree.root_node(), &source, lang.symbol_kinds, &mut chunks);
        if chunks.is_empty() {
            return Ok(0);
        }

        let texts: Vec<String> = chunks.iter().map(|(text, _, _)| text.clone()).collect();
        let embeddings = self.embedder.embed(&texts).await?;

        let file_path = path.to_string_lossy().to_string();
        let conn = self.conn.clone();
        let count = chunks.len();

        tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let tx = conn.unchecked_transaction()?;
            for ((chunk_text, start_line, end_line), embedding) in chunks.into_iter().zip(embeddings) {
                tx.execute(
                    "INSERT INTO code_chunks_meta(file_path, chunk_text, start_line, end_line) VALUES (?1, ?2, ?3, ?4)",
                    rusqlite::params![file_path, chunk_text, start_line, end_line],
                )?;
                let rowid = tx.last_insert_rowid();
                tx.execute("INSERT INTO code_chunks(rowid, embedding) VALUES (?1, ?2)", rusqlite::params![rowid, embedding.as_bytes()])?;
            }
            tx.commit()?;
            Ok::<(), rusqlite::Error>(())
        })
        .await??;

        Ok(count)
    }

    pub async fn semantic_search(&self, query: &str, top_k: usize) -> Result<Vec<CodeChunk>, MemoryError> {
        let query_embedding = self.embedder.embed(&[query.to_string()]).await?.into_iter().next().unwrap_or_default();
        let bytes = query_embedding.as_bytes().to_vec();

        let conn = self.conn.clone();
        let results = tokio::task::spawn_blocking(move || {
            let conn = conn.lock().unwrap();
            let mut stmt = conn.prepare(
                "SELECT code_chunks_meta.file_path, code_chunks_meta.chunk_text, code_chunks_meta.start_line, code_chunks_meta.end_line, code_chunks.distance
                 FROM code_chunks JOIN code_chunks_meta ON code_chunks.rowid = code_chunks_meta.id
                 WHERE code_chunks.embedding MATCH ?1 AND k = ?2
                 ORDER BY code_chunks.distance",
            )?;
            let mut rows = stmt.query(rusqlite::params![bytes, top_k as i64])?;
            let mut out = Vec::new();
            while let Some(row) = rows.next()? {
                out.push(CodeChunk {
                    file_path: row.get(0)?,
                    chunk_text: row.get(1)?,
                    start_line: row.get(2)?,
                    end_line: row.get(3)?,
                    distance: row.get(4)?,
                });
            }
            Ok::<Vec<CodeChunk>, rusqlite::Error>(out)
        })
        .await??;
        Ok(results)
    }
}

fn collect_chunks(node: tree_sitter::Node, source: &str, kinds: &[&str], out: &mut Vec<(String, i64, i64)>) {
    if kinds.contains(&node.kind()) {
        let text = source[node.byte_range()].to_string();
        let start = node.start_position().row as i64 + 1;
        let end = node.end_position().row as i64 + 1;
        out.push((text, start, end));
        return; // don't descend into an already-chunked node's children
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_chunks(child, source, kinds, out);
    }
}

fn now_unix() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}
