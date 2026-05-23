use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::models::{
    json_u64, FileFingerprint, FileMetadataParts, GoalUsage, ParsedSessionFile, Session,
    TokenEvent, TokenUsage,
};
use crate::util::{fingerprint_from_hash, hash_file_fingerprint, hex_bytes};

pub(crate) struct SessionParser;

impl SessionParser {
    pub(crate) fn parse_or_error(path: &Path) -> Session {
        match Self::parse(path) {
            Ok(session) => session,
            Err(err) => Self::parse_error_session(path, err),
        }
    }

    fn parse_error_session(path: &Path, err: anyhow::Error) -> Session {
        Session {
            id: path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("unknown")
                .to_string(),
            timestamp: String::new(),
            path: path.to_path_buf(),
            cwd: None,
            model: None,
            model_provider: None,
            first_user_message: None,
            final_assistant_message: None,
            token_events: Vec::new(),
            goal: GoalUsage::default(),
            web_search_calls: 0,
            line_count: 0,
            parse_errors: vec![format!("{err:#}")],
            search_messages: Vec::new(),
            cached_final_usage: None,
            max_request_input_tokens: 0,
            token_event_count: 0,
        }
    }
    pub(crate) fn parse_with_fingerprint_or_error(
        _root: &Path,
        path: &Path,
        relative_path: &str,
        metadata: FileMetadataParts,
    ) -> Result<ParsedSessionFile> {
        match Self::parse_with_fingerprint(path, relative_path, metadata) {
            Ok(parsed) => Ok(parsed),
            Err(err) => Ok(ParsedSessionFile {
                session: Self::parse_error_session(path, err),
                fingerprint: hash_file_fingerprint(path, relative_path, metadata)?,
            }),
        }
    }

    pub(crate) fn parse_with_fingerprint(
        path: &Path,
        relative_path: &str,
        metadata: FileMetadataParts,
    ) -> Result<ParsedSessionFile> {
        let (session, fingerprint) = Self::parse_inner(path, Some((relative_path, metadata)))?;
        Ok(ParsedSessionFile {
            session,
            fingerprint: fingerprint.expect("fingerprint requested"),
        })
    }

    pub(crate) fn parse(path: &Path) -> Result<Session> {
        let (session, _fingerprint) = Self::parse_inner(path, None)?;
        Ok(session)
    }

    fn parse_inner(
        path: &Path,
        fingerprint_input: Option<(&str, FileMetadataParts)>,
    ) -> Result<(Session, Option<FileFingerprint>)> {
        let file = File::open(path)?;
        let mut reader = BufReader::new(file);
        let mut hasher = fingerprint_input.as_ref().map(|_| Sha256::new());

        let mut id = String::new();
        let mut timestamp = String::new();
        let mut cwd = None;
        let mut model = None;
        let mut model_provider = None;
        let mut first_user_message = None;
        let mut final_assistant_message = None;
        let mut search_messages = Vec::new();
        let mut token_events = Vec::new();
        let mut goal = GoalUsage::default();
        let mut web_search_calls = 0;
        let mut parse_errors = Vec::new();
        let mut line_count = 0;
        let mut previous_total_usage: Option<TokenUsage> = None;
        let mut current_model: Option<String> = None;

        let mut line_bytes = Vec::new();
        loop {
            line_bytes.clear();
            let next_line = line_count + 1;
            let bytes_read = reader
                .read_until(b'\n', &mut line_bytes)
                .with_context(|| format!("failed to read line {next_line}"))?;
            if bytes_read == 0 {
                break;
            }
            if let Some(hasher) = hasher.as_mut() {
                hasher.update(&line_bytes);
            }
            line_count += 1;
            let line_idx = line_count;
            let line = std::str::from_utf8(&line_bytes)
                .with_context(|| format!("line {line_idx} is not valid UTF-8"))?;
            if line.trim().is_empty() {
                continue;
            }
            let value: Value = match serde_json::from_str(line) {
                Ok(value) => value,
                Err(err) => {
                    parse_errors.push(format!("line {}: {}", line_idx, err));
                    continue;
                }
            };

            let top_timestamp = value
                .get("timestamp")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            if timestamp.is_empty() && !top_timestamp.is_empty() {
                timestamp = top_timestamp.clone();
            }

            match value
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or_default()
            {
                "session_meta" => {
                    let payload = value.get("payload").unwrap_or(&Value::Null);
                    if let Some(meta_id) = payload.get("id").and_then(Value::as_str) {
                        id = meta_id.to_string();
                    }
                    if let Some(meta_ts) = payload.get("timestamp").and_then(Value::as_str) {
                        timestamp = meta_ts.to_string();
                    }
                    if let Some(meta_cwd) = payload.get("cwd").and_then(Value::as_str) {
                        cwd = Some(meta_cwd.to_string());
                    }
                    if let Some(provider) = payload.get("model_provider").and_then(Value::as_str) {
                        model_provider = Some(provider.to_string());
                    }
                }
                "turn_context" => {
                    let payload = value.get("payload").unwrap_or(&Value::Null);
                    if let Some(turn_cwd) = payload.get("cwd").and_then(Value::as_str) {
                        cwd = Some(turn_cwd.to_string());
                    }
                    if let Some(turn_model) = Self::model_from_payload(Some(payload)) {
                        current_model = Some(turn_model.clone());
                        model = Some(turn_model);
                    }
                }
                "response_item" => {
                    let payload = value.get("payload").unwrap_or(&Value::Null);
                    match payload
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "message" => {
                            let role = payload
                                .get("role")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            let text = Self::extract_message_text(payload);
                            if role == "user" && !text.is_empty() {
                                Self::record_user_message(
                                    &mut first_user_message,
                                    &mut search_messages,
                                    &text,
                                );
                            } else if role == "assistant" && !text.is_empty() {
                                final_assistant_message = Some(text);
                            }
                        }
                        "web_search_call" => {
                            web_search_calls += 1;
                        }
                        _ => {}
                    }
                    if let Some(raw_usage) = Self::usage_from_exec_result(&value) {
                        if raw_usage.is_zero() {
                            continue;
                        }
                        if let Some(parsed_model) = Self::model_from_result(&value) {
                            current_model = Some(parsed_model.clone());
                            model = Some(parsed_model);
                        } else if model.is_none() {
                            model = current_model.clone().or_else(|| Some("gpt-5".to_string()));
                        }
                        let total = previous_total_usage
                            .clone()
                            .unwrap_or_default()
                            .saturating_add(&raw_usage);
                        previous_total_usage = Some(total.clone());
                        token_events.push(TokenEvent {
                            timestamp: Self::timestamp_from_result(&value)
                                .unwrap_or_else(|| top_timestamp.clone()),
                            total,
                            last: raw_usage.normalize_total(),
                            context_window: None,
                        });
                    }
                }
                "event_msg" => {
                    let payload = value.get("payload").unwrap_or(&Value::Null);
                    match payload
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                    {
                        "token_count" => {
                            let info = payload.get("info").unwrap_or(&Value::Null);
                            if let Some(parsed_model) = Self::model_from_payload(Some(payload))
                                .or_else(|| Self::model_from_payload(Some(info)))
                            {
                                current_model = Some(parsed_model.clone());
                                model = Some(parsed_model);
                            } else if model.is_none() {
                                model = current_model.clone();
                            }

                            let total_usage = Self::usage_from_token_count(info);
                            let last_usage = info
                                .get("last_token_usage")
                                .and_then(Self::usage_from_value)
                                .or_else(|| {
                                    total_usage.as_ref().map(|usage| {
                                        usage.saturating_sub(previous_total_usage.as_ref())
                                    })
                                });
                            let Some(last_usage) = last_usage else {
                                continue;
                            };
                            if last_usage.is_zero() {
                                continue;
                            }
                            let total = total_usage
                                .unwrap_or_else(|| {
                                    previous_total_usage
                                        .clone()
                                        .unwrap_or_default()
                                        .saturating_add(&last_usage)
                                })
                                .normalize_total();
                            previous_total_usage = Some(total.clone());
                            token_events.push(TokenEvent {
                                timestamp: top_timestamp,
                                total,
                                last: last_usage.normalize_total(),
                                context_window: info
                                    .get("model_context_window")
                                    .and_then(Value::as_u64),
                            });
                        }
                        "thread_goal_updated" => {
                            if let Some(goal_value) = payload.get("goal") {
                                if let Some(objective) =
                                    goal_value.get("objective").and_then(Value::as_str)
                                {
                                    goal.objective = Some(objective.to_string());
                                }
                                if let Some(status) =
                                    goal_value.get("status").and_then(Value::as_str)
                                {
                                    goal.status = Some(status.to_string());
                                }
                                goal.tokens_used =
                                    goal_value.get("tokensUsed").and_then(Value::as_u64);
                                goal.time_used_seconds =
                                    goal_value.get("timeUsedSeconds").and_then(Value::as_u64);
                            }
                        }
                        "user_message" => {
                            if let Some(message) = payload.get("message").and_then(Value::as_str) {
                                Self::record_user_message(
                                    &mut first_user_message,
                                    &mut search_messages,
                                    message,
                                );
                            }
                        }
                        "agent_message" => {
                            if let Some(message) = payload.get("message").and_then(Value::as_str) {
                                final_assistant_message = Some(message.to_string());
                            }
                        }
                        _ => {}
                    }
                }
                _ => {
                    if let Some(raw_usage) = Self::usage_from_exec_result(&value) {
                        if raw_usage.is_zero() {
                            continue;
                        }
                        if let Some(parsed_model) = Self::model_from_result(&value) {
                            current_model = Some(parsed_model.clone());
                            model = Some(parsed_model);
                        } else if model.is_none() {
                            model = current_model.clone().or_else(|| Some("gpt-5".to_string()));
                        }
                        let total = previous_total_usage
                            .clone()
                            .unwrap_or_default()
                            .saturating_add(&raw_usage);
                        previous_total_usage = Some(total.clone());
                        token_events.push(TokenEvent {
                            timestamp: Self::timestamp_from_result(&value)
                                .unwrap_or_else(|| top_timestamp.clone()),
                            total,
                            last: raw_usage.normalize_total(),
                            context_window: None,
                        });
                    }
                }
            }
        }

        if id.is_empty() {
            id = Self::infer_id_from_path(path);
        }
        if timestamp.is_empty() {
            timestamp = path
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .map(|_| String::from("unknown"))
                .unwrap_or_default();
        }

        let cached_final_usage = token_events.last().map(|event| event.total.clone());
        let max_request_input_tokens = token_events
            .iter()
            .map(|event| event.last.input_tokens)
            .max()
            .unwrap_or_default();
        let token_event_count = token_events.len();
        let session = Session {
            id,
            timestamp,
            path: path.to_path_buf(),
            cwd,
            model,
            model_provider,
            first_user_message,
            final_assistant_message,
            token_events,
            goal,
            web_search_calls,
            line_count,
            parse_errors,
            search_messages,
            cached_final_usage,
            max_request_input_tokens,
            token_event_count,
        };
        let fingerprint = match (fingerprint_input, hasher) {
            (Some((relative_path, metadata)), Some(hasher)) => Some(fingerprint_from_hash(
                relative_path,
                metadata,
                hex_bytes(&hasher.finalize()),
            )),
            _ => None,
        };

        Ok((session, fingerprint))
    }

    fn extract_message_text(payload: &Value) -> String {
        let mut parts = Vec::new();
        if let Some(content) = payload.get("content").and_then(Value::as_array) {
            for item in content {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else if let Some(text) = item.get("input_text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                } else if let Some(text) = item.get("output_text").and_then(Value::as_str) {
                    parts.push(text.to_string());
                }
            }
        }
        parts.join("\n")
    }

    fn record_user_message(
        first_user_message: &mut Option<String>,
        search_messages: &mut Vec<String>,
        text: &str,
    ) {
        if !Self::is_searchable_user_message(text) {
            return;
        }
        if first_user_message.is_none() {
            *first_user_message = Some(text.to_string());
        }
        search_messages.push(text.to_string());
    }

    fn is_searchable_user_message(text: &str) -> bool {
        let trimmed = text.trim_start();
        !trimmed.starts_with("<environment_context>")
    }

    fn non_empty_json_string(value: Option<&Value>) -> Option<String> {
        let text = value?.as_str()?.trim();
        (!text.is_empty()).then(|| text.to_string())
    }

    fn model_from_payload(value: Option<&Value>) -> Option<String> {
        let value = value?;
        ["model", "model_name"]
            .into_iter()
            .find_map(|key| Self::non_empty_json_string(value.get(key)))
            .or_else(|| {
                value
                    .get("metadata")
                    .and_then(|metadata| Self::non_empty_json_string(metadata.get("model")))
            })
    }

    fn model_from_result(value: &Value) -> Option<String> {
        Self::model_from_payload(Some(value))
            .or_else(|| Self::model_from_payload(value.get("data")))
            .or_else(|| Self::model_from_payload(value.get("result")))
            .or_else(|| Self::model_from_payload(value.get("response")))
            .or_else(|| Self::model_from_payload(value.get("payload")))
    }

    fn usage_from_token_count(info: &Value) -> Option<TokenUsage> {
        info.get("total_token_usage")
            .and_then(Self::usage_from_value)
    }

    fn usage_from_value(value: &Value) -> Option<TokenUsage> {
        value
            .is_object()
            .then(|| TokenUsage::from_value(value).normalize_total())
    }

    fn usage_object_from_result(value: &Value) -> Option<&Value> {
        value
            .get("usage")
            .or_else(|| value.get("data").and_then(|data| data.get("usage")))
            .or_else(|| value.get("result").and_then(|result| result.get("usage")))
            .or_else(|| {
                value
                    .get("response")
                    .and_then(|response| response.get("usage"))
            })
            .or_else(|| {
                value
                    .get("payload")
                    .and_then(|payload| payload.get("usage"))
            })
    }

    fn usage_from_exec_result(value: &Value) -> Option<TokenUsage> {
        let usage = Self::usage_object_from_result(value)?;
        let input = json_u64(usage.get("input_tokens"))
            .or_else(|| json_u64(usage.get("prompt_tokens")))
            .or_else(|| json_u64(usage.get("input")))
            .unwrap_or(0);
        let cached = json_u64(usage.get("cached_input_tokens"))
            .or_else(|| json_u64(usage.get("cache_read_input_tokens")))
            .or_else(|| json_u64(usage.get("cached_tokens")))
            .unwrap_or(0);
        let output = json_u64(usage.get("output_tokens"))
            .or_else(|| json_u64(usage.get("completion_tokens")))
            .or_else(|| json_u64(usage.get("output")))
            .unwrap_or(0);
        let reasoning = json_u64(usage.get("reasoning_output_tokens"))
            .or_else(|| json_u64(usage.get("reasoning_tokens")))
            .unwrap_or(0);
        let total = json_u64(usage.get("total_tokens")).unwrap_or(0);
        let usage = TokenUsage {
            input_tokens: input,
            cached_input_tokens: cached,
            output_tokens: output,
            reasoning_output_tokens: reasoning,
            total_tokens: total,
        }
        .normalize_total();
        (!usage.is_zero()).then_some(usage)
    }
    fn timestamp_from_result(value: &Value) -> Option<String> {
        Self::timestamp_value(value.get("timestamp"))
            .or_else(|| Self::timestamp_value(value.get("created_at")))
            .or_else(|| Self::timestamp_value(value.get("createdAt")))
            .or_else(|| {
                value
                    .get("data")
                    .and_then(|data| Self::timestamp_value(data.get("timestamp")))
            })
            .or_else(|| {
                value
                    .get("result")
                    .and_then(|result| Self::timestamp_value(result.get("timestamp")))
            })
            .or_else(|| {
                value
                    .get("response")
                    .and_then(|response| Self::timestamp_value(response.get("timestamp")))
            })
            .or_else(|| {
                value
                    .get("payload")
                    .and_then(|payload| Self::timestamp_value(payload.get("timestamp")))
            })
    }

    fn timestamp_value(value: Option<&Value>) -> Option<String> {
        match value? {
            Value::String(text) => {
                let text = text.trim();
                (!text.is_empty()).then(|| text.to_string())
            }
            Value::Number(number) => Some(number.to_string()),
            _ => None,
        }
    }

    fn infer_id_from_path(path: &Path) -> String {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("unknown");
        stem.rsplit('-')
            .take(5)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join("-")
    }
}
