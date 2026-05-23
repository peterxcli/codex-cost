use super::*;

#[test]
fn parses_headless_exec_usage_records() {
    let dir = temp_dir("exec");
    let path = dir.join("run.jsonl");
    fs::write(
        &path,
        [
            json!({
                "type": "turn.completed",
                "timestamp": "2026-01-02T03:04:05.000Z",
                "model": "gpt-5.2-codex",
                "usage": {
                    "input_tokens": 120,
                    "cached_input_tokens": 20,
                    "output_tokens": 30,
                    "total_tokens": 150
                }
            })
            .to_string(),
            json!({
                "type": "result",
                "data": {
                    "timestamp": "2026-01-02T03:05:05.000Z",
                    "model_name": "gpt-5.2-codex",
                    "usage": {
                        "prompt_tokens": 50,
                        "cached_tokens": 5,
                        "completion_tokens": 12
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .unwrap();

    let session = SessionParser::parse(&path).unwrap();
    let final_usage = session.final_usage().unwrap();

    assert_eq!(session.token_events.len(), 2);
    assert_eq!(session.model.as_deref(), Some("gpt-5.2-codex"));
    assert_eq!(session.token_events[0].last.input_tokens, 120);
    assert_eq!(session.token_events[1].last.input_tokens, 50);
    assert_eq!(final_usage.input_tokens, 170);
    assert_eq!(final_usage.cached_input_tokens, 25);
    assert_eq!(final_usage.output_tokens, 42);
    assert_eq!(final_usage.total_tokens, 212);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn derives_last_usage_from_cumulative_token_count() {
    let dir = temp_dir("cumulative");
    let path = dir.join("rollout-test.jsonl");
    fs::write(
        &path,
        [
            json!({
                "timestamp": "2026-01-02T00:00:00.000Z",
                "type": "turn_context",
                "payload": {"model": "gpt-5.5"}
            })
            .to_string(),
            json!({
                "timestamp": "2026-01-02T00:00:01.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 100,
                            "cached_input_tokens": 10,
                            "output_tokens": 20,
                            "reasoning_output_tokens": 5,
                            "total_tokens": 120
                        }
                    }
                }
            })
            .to_string(),
            json!({
                "timestamp": "2026-01-02T00:00:02.000Z",
                "type": "event_msg",
                "payload": {
                    "type": "token_count",
                    "info": {
                        "total_token_usage": {
                            "input_tokens": 180,
                            "cached_input_tokens": 60,
                            "output_tokens": 30,
                            "reasoning_output_tokens": 8,
                            "total_tokens": 210
                        }
                    }
                }
            })
            .to_string(),
        ]
        .join("\n"),
    )
    .unwrap();

    let session = SessionParser::parse(&path).unwrap();

    assert_eq!(session.token_events.len(), 2);
    assert_eq!(session.token_events[1].last.input_tokens, 80);
    assert_eq!(session.token_events[1].last.cached_input_tokens, 50);
    assert_eq!(session.token_events[1].last.output_tokens, 10);
    assert_eq!(session.token_events[1].last.reasoning_output_tokens, 3);
    assert_eq!(session.final_usage().unwrap().total_tokens, 210);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parses_first_human_prompt_after_environment_context() {
    let dir = temp_dir("human-prompt");
    let path = dir.join("rollout-human.jsonl");
    fs::write(
            &path,
            [
                json!({
                    "timestamp": "2026-01-02T00:00:00.000Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{
                            "type": "input_text",
                            "text": "<environment_context>\n  <cwd>/tmp/project</cwd>\n</environment_context>"
                        }]
                    }
                })
                .to_string(),
                json!({
                    "timestamp": "2026-01-02T00:00:01.000Z",
                    "type": "response_item",
                    "payload": {
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": "hello"}]
                    }
                })
                .to_string(),
            ]
            .join("\n"),
        )
        .unwrap();

    let session = SessionParser::parse(&path).unwrap();
    let index = SearchIndex::build(std::slice::from_ref(&session), |_current, _total| {});

    assert_eq!(session.first_user_message.as_deref(), Some("hello"));
    assert_eq!(index.search("hello"), vec![0]);
    assert!(index.search("environment_context").is_empty());

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn indexes_event_user_message_text() {
    let dir = temp_dir("event-user-message");
    let path = dir.join("rollout-event-user.jsonl");
    fs::write(
        &path,
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "event_msg",
            "payload": {
                "type": "user_message",
                "message": "hello from event stream",
                "images": [],
                "local_images": []
            }
        })
        .to_string(),
    )
    .unwrap();

    let session = SessionParser::parse(&path).unwrap();
    let index = SearchIndex::build(std::slice::from_ref(&session), |_current, _total| {});

    assert_eq!(
        session.first_user_message.as_deref(),
        Some("hello from event stream")
    );
    assert_eq!(index.search("hello event"), vec![0]);

    fs::remove_dir_all(dir).unwrap();
}

#[test]
fn parse_session_with_fingerprint_matches_file_content_hash() {
    let dir = temp_dir("parse-fingerprint");
    let path = dir.join("session.jsonl");
    let content = [
        json!({
            "timestamp": "2026-01-02T00:00:00.000Z",
            "type": "response_item",
            "payload": {
                "type": "message",
                "role": "user",
                "content": [{"text": "single pass marker"}]
            }
        })
        .to_string(),
        json!({
            "timestamp": "2026-01-02T00:00:01.000Z",
            "type": "event_msg",
            "payload": {
                "type": "token_count",
                "info": {
                    "total_token_usage": {
                        "input_tokens": 3,
                        "output_tokens": 4,
                        "total_tokens": 7
                    }
                }
            }
        })
        .to_string(),
    ]
    .join("\n");
    fs::write(&path, &content).unwrap();

    let parsed = SessionParser::parse_with_fingerprint(
        &path,
        "session.jsonl",
        file_metadata_parts(&path).unwrap(),
    )
    .unwrap();

    assert_eq!(
        parsed.session.first_user_message.as_deref(),
        Some("single pass marker")
    );
    assert_eq!(
        parsed.fingerprint.content_hash,
        hash_hex(content.as_bytes())
    );
    assert_eq!(parsed.fingerprint.size, content.len() as u64);

    fs::remove_dir_all(dir).unwrap();
}
