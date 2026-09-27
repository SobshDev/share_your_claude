use crate::{
    AppState,
    error::{AppError, Result},
    oauth,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// Catalog group of every Fable 5.1 variant. Also used by the seed migration.
pub(crate) const FABLE_GROUP: &str = "fable-5.1";
/// Token sequence that identifies Fable 5.1 in any spelling of an id, alias, group, or name.
const FABLE_TOKENS: [&str; 3] = ["fable", "5", "1"];
/// Denial returned whenever a request or admin action would expose Fable 5.1.
pub(crate) const FABLE_RESERVED: &str = "Fable 5.1 is reserved for the owner";

/// Whether a model id, alias, group, or display name refers to Fable 5.1.
///
/// Names are lowercased and split into runs of ASCII letters or digits, so separators such
/// as `.`, `_`, `@`, `/`, and whitespace are equivalent, and so are letter-digit boundaries
/// (`fable5.1`). Only a whole `fable 5 1` sequence matches: `claude-fable-5-10` does not.
pub(crate) fn names_fable(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let mut tokens = Vec::new();
    for part in lower.split(|c: char| !c.is_ascii_alphanumeric()) {
        // `part` is ASCII, so byte offsets are character boundaries.
        let mut start = 0;
        for (i, pair) in part.as_bytes().windows(2).enumerate() {
            if pair[0].is_ascii_digit() != pair[1].is_ascii_digit() {
                tokens.push(&part[start..=i]);
                start = i + 1;
            }
        }
        if start < part.len() {
            tokens.push(&part[start..]);
        }
    }
    tokens
        .windows(FABLE_TOKENS.len())
        .any(|w| w == FABLE_TOKENS)
}

/// The single rule that keeps Fable 5.1 away from friend keys.
pub(crate) fn blocked(id: &str, group: &str) -> bool {
    names_fable(id) || names_fable(group)
}

/// Catalog group for a model discovered upstream.
pub(crate) fn catalog_group(id: &str, display_name: &str) -> &'static str {
    if blocked(id, "") || names_fable(display_name) {
        FABLE_GROUP
    } else {
        "claude"
    }
}

pub(crate) async fn resolve(state: &AppState, key_id: &str, requested: &str) -> Result<String> {
    use sqlx::Row;
    let row = sqlx::query("SELECT m.id,m.model_group FROM model m JOIN key_model_grant g ON g.model_id=m.id JOIN api_key k ON k.id=g.key_id WHERE g.key_id=? AND k.revoked_at IS NULL AND m.enabled=1 AND m.reviewed_at IS NOT NULL AND m.id=COALESCE((SELECT id FROM model WHERE id=?),(SELECT model_id FROM model_alias WHERE alias=?))")
        .bind(key_id).bind(requested).bind(requested).fetch_optional(&state.db).await?
        .ok_or_else(|| AppError::forbidden("This key does not have access to that model"))?;
    let id: String = row.get("id");
    let group: String = row.get("model_group");
    if blocked(&id, &group) {
        return Err(AppError::forbidden(FABLE_RESERVED));
    }
    Ok(id)
}

pub(crate) fn validate(body: &Value, counting: bool) -> Result<()> {
    let object = body
        .as_object()
        .ok_or_else(|| AppError::bad("Expected a JSON object"))?;
    let allowed = [
        "model",
        "messages",
        "system",
        "tools",
        "tool_choice",
        "max_tokens",
        "stream",
        "temperature",
        "top_p",
        "top_k",
        "stop_sequences",
        "metadata",
        "thinking",
        "output_config",
        "cache_control",
        "service_tier",
    ];
    if object.keys().any(|k| !allowed.contains(&k.as_str())) {
        return Err(AppError::bad(
            "Unsupported request field; fallback and alternate model routing are unavailable",
        ));
    }
    let model = object
        .get("model")
        .and_then(Value::as_str)
        .ok_or_else(|| AppError::bad("model is required"))?;
    if model.is_empty() || model.len() > crate::proxy::MAX_MODEL_ID_BYTES {
        return Err(AppError::bad("Invalid model identifier"));
    }
    if !object.get("messages").is_some_and(Value::is_array) {
        return Err(AppError::bad("messages must be an array"));
    }
    if !counting
        && !object
            .get("max_tokens")
            .and_then(Value::as_u64)
            .is_some_and(|n| n > 0 && n <= i64::MAX as u64)
    {
        return Err(AppError::bad("max_tokens must be a positive integer"));
    }
    if object.get("stream").is_some_and(|v| !v.is_boolean()) {
        return Err(AppError::bad("stream must be a boolean"));
    }
    if counting && object.get("stream") == Some(&Value::Bool(true)) {
        return Err(AppError::bad("Token counting does not stream"));
    }
    if let Some(tools) = object.get("tools") {
        for tool in tools
            .as_array()
            .ok_or_else(|| AppError::bad("tools must be an array"))?
        {
            let fields = tool
                .as_object()
                .ok_or_else(|| AppError::bad("Invalid tool"))?;
            let allowed_tool_fields = [
                "name",
                "type",
                "description",
                "input_schema",
                "cache_control",
                "strict",
                "defer_loading",
                "allowed_callers",
                "max_uses",
                "allowed_domains",
                "blocked_domains",
                "user_location",
                "citations",
                "max_content_tokens",
                "display_width_px",
                "display_height_px",
                "display_number",
            ];
            if fields
                .keys()
                .any(|k| !allowed_tool_fields.contains(&k.as_str()))
            {
                return Err(AppError::bad(
                    "Unsupported tool field; advisor and model-routing tools are unavailable",
                ));
            }
            if let Some(kind) = fields.get("type") {
                let permitted = [
                    "custom",
                    "web_search_20250305",
                    "web_fetch_20250910",
                    "code_execution_20250522",
                    "code_execution_20250825",
                    "text_editor_20250124",
                    "text_editor_20250429",
                    "text_editor_20250728",
                    "computer_20250124",
                    "bash_20250124",
                ];
                if !kind.as_str().is_some_and(|k| permitted.contains(&k)) {
                    return Err(AppError::bad("This server tool type has not been reviewed"));
                }
            }
            if !fields
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|v| !v.is_empty() && v.len() <= 128)
            {
                return Err(AppError::bad("Invalid tool name"));
            }
        }
    }
    // Only reviewed control fields can reach Anthropic. JSON inside message/tool schemas is data.
    for (field, allowed) in [
        ("output_config", &["effort", "format"][..]),
        ("thinking", &["type", "budget_tokens", "display"][..]),
        (
            "tool_choice",
            &["type", "name", "disable_parallel_tool_use"][..],
        ),
        ("metadata", &["user_id"][..]),
        ("cache_control", &["type", "ttl"][..]),
    ] {
        if let Some(value) = object.get(field) {
            let map = value
                .as_object()
                .ok_or_else(|| AppError::bad("Invalid request control object"))?;
            if map.keys().any(|k| !allowed.contains(&k.as_str())) {
                return Err(AppError::bad("Unsupported request control field"));
            }
        }
    }
    Ok(())
}

#[derive(Default, Debug)]
pub(crate) struct ToolMap {
    originals: HashMap<String, String>,
    builtins: HashSet<String>,
}

impl ToolMap {
    fn wire(&mut self, name: &str) -> Result<String> {
        // Stable bijection within a request, including names already starting with custom_.
        // 63 characters fits Anthropic's 64-character tool name limit.
        let digest = Sha256::digest(name.as_bytes());
        let wire = format!(
            "custom_{}",
            digest[..28]
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        );
        if self
            .originals
            .get(&wire)
            .is_some_and(|previous| previous != name)
        {
            return Err(AppError::bad("Tool name collision"));
        }
        self.originals.insert(wire.clone(), name.to_owned());
        Ok(wire)
    }
    fn rewrite_block(&mut self, block: &mut Value) -> Result<()> {
        if block.get("type").and_then(Value::as_str) == Some("tool_use")
            && let Some(name) = block.get("name").and_then(Value::as_str).map(str::to_owned)
            && !self.builtins.contains(&name)
        {
            block["name"] = self.wire(&name)?.into();
        }
        Ok(())
    }
    pub(crate) fn prepare(body: &mut Value) -> Result<Self> {
        let mut mapping = Self::default();
        let mut seen = std::collections::HashSet::new();
        if let Some(tools) = body.get_mut("tools").and_then(Value::as_array_mut) {
            for tool in tools {
                let name = tool["name"]
                    .as_str()
                    .ok_or_else(|| AppError::bad("Tool name missing"))?
                    .to_owned();
                if !seen.insert(name.clone()) {
                    return Err(AppError::bad("Duplicate tool name"));
                }
                if tool.get("type").is_none() || tool["type"] == "custom" {
                    tool["name"] = mapping.wire(&name)?.into();
                } else {
                    mapping.builtins.insert(name);
                }
            }
        }
        if let Some(messages) = body.get_mut("messages").and_then(Value::as_array_mut) {
            for message in messages {
                if let Some(blocks) = message.get_mut("content").and_then(Value::as_array_mut) {
                    for block in blocks {
                        mapping.rewrite_block(block)?;
                    }
                }
            }
        }
        if let Some(choice) = body.get_mut("tool_choice")
            && let Some(name) = choice
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned)
        {
            let candidate = mapping
                .originals
                .iter()
                .find(|(_, v)| **v == name)
                .map(|(k, _)| k.clone());
            if let Some(wire) = candidate {
                choice["name"] = wire.into();
            }
        }
        let system = body
            .as_object_mut()
            .expect("validated body")
            .remove("system");
        let mut blocks = vec![json!({"type":"text","text":oauth::SYSTEM})];
        match system {
            Some(Value::String(text)) => blocks.push(json!({"type":"text","text":text})),
            Some(Value::Array(items)) => blocks.extend(items),
            None => (),
            _ => return Err(AppError::bad("system must be text or content blocks")),
        }
        body["system"] = blocks.into();
        Ok(mapping)
    }
    pub(crate) fn restore(&self, value: &mut Value) {
        // Only protocol tool-use blocks are rewritten, never arbitrary tool arguments or text.
        if value.get("type").and_then(Value::as_str) == Some("tool_use")
            && let Some(original) = value
                .get("name")
                .and_then(Value::as_str)
                .and_then(|n| self.originals.get(n))
        {
            value["name"] = original.clone().into();
        }
        if let Some(block) = value.get_mut("content_block") {
            self.restore(block);
        }
        if let Some(blocks) = value.get_mut("content").and_then(Value::as_array_mut) {
            for block in blocks {
                self.restore(block);
            }
        }
    }
}
