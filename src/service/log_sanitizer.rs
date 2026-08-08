//! フック入力をログ用に安全な概要へ変換する補助処理。

use serde_json::Value;

use crate::domain::{HookInput, ToolInput};

/// 生のフック入力 JSON から、機密になり得る本文を含まない概要を作る。
///
/// `tool_input.command` や `tool_input.content`、編集前後の全文はログへ残さない。
/// デバッグログは原因調査に必要なイベント種別と入力サイズだけを記録する。
pub(crate) fn summarize_hook_input(input: &str) -> String {
    let bytes = input.len();
    let Ok(raw) = serde_json::from_str::<Value>(input) else {
        return format!("invalid_json bytes={bytes}");
    };

    let event = raw
        .get("hook_event_name")
        .or_else(|| raw.get("event"))
        .or_else(|| raw.get("agent_action_name"))
        .and_then(Value::as_str)
        .unwrap_or("<unknown>");

    let tool = raw
        .get("tool_name")
        .and_then(Value::as_str)
        .or_else(|| {
            raw.get("tool_info")
                .and_then(|tool_info| tool_info.get("tool_name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("-");

    let session_id = raw.get("session_id").and_then(Value::as_str).unwrap_or("-");

    format!("event={event} tool={tool} session_id={session_id} bytes={bytes}")
}

/// パース済みフック入力から、本文を含まないログ用概要を作る。
///
/// `Debug` 表示で `ToolInput` をそのまま出すと、コマンド文字列やファイル本文、
/// エージェント応答がログに残る。ここでは調査に必要な種別とサイズだけを残す。
pub(crate) fn summarize_parsed_hook_input(input: &HookInput) -> String {
    let session_id = input.session_id.as_deref().unwrap_or("-");
    let detail = match &input.tool_input {
        ToolInput::Bash(bash) => format!("input=Bash command_bytes={}", bash.command.len()),
        ToolInput::File(file) => format!(
            "input=File file_path_bytes={} content_bytes={}",
            file.file_path.len(),
            file.content.as_ref().map_or(0, String::len)
        ),
        ToolInput::Files(files) => {
            let file_path_bytes: usize = files.iter().map(|file| file.file_path.len()).sum();
            let content_bytes: usize = files
                .iter()
                .map(|file| file.content.as_ref().map_or(0, String::len))
                .sum();
            format!(
                "input=Files count={} file_path_bytes={} content_bytes={}",
                files.len(),
                file_path_bytes,
                content_bytes
            )
        }
        ToolInput::Stop(stop) => format!(
            "input=Stop status={} response_bytes={} agent_message_bytes={} active={}",
            stop.status.as_deref().unwrap_or("-"),
            stop.response.as_ref().map_or(0, String::len),
            stop.agent_message.as_ref().map_or(0, String::len),
            stop.stop_hook_active
        ),
        ToolInput::Subagent(subagent) => format!(
            "input=Subagent type_bytes={} prompt_bytes={} status={}",
            subagent.subagent_type.as_ref().map_or(0, String::len),
            subagent.prompt.as_ref().map_or(0, String::len),
            subagent.status.as_deref().unwrap_or("-")
        ),
        ToolInput::Other(value) => format!("input=Other json_bytes={}", value.to_string().len()),
    };

    format!(
        "event={:?} tool={} session_id={} {}",
        input.event, input.tool_name, session_id, detail
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{
        BashInput, FileOperationInput, HookEvent, StopInput, SubagentInput, ToolInput,
    };

    #[test]
    fn summarize_hook_input_does_not_include_sensitive_tool_input() {
        let input = r#"{
            "session_id": "session-1",
            "hook_event_name": "PostToolUse",
            "tool_name": "Write",
            "tool_input": {
                "command": "echo super-secret-token",
                "content": "API_KEY=super-secret-token",
                "old_string": "old secret",
                "new_string": "new secret"
            }
        }"#;

        let summary = summarize_hook_input(input);

        assert!(summary.contains("event=PostToolUse"));
        assert!(summary.contains("tool=Write"));
        assert!(summary.contains("session_id=session-1"));
        assert!(summary.contains("bytes="));
        assert!(!summary.contains("super-secret-token"));
        assert!(!summary.contains("API_KEY"));
        assert!(!summary.contains("old secret"));
        assert!(!summary.contains("new secret"));
    }

    #[test]
    fn summarize_hook_input_reads_nested_windsurf_tool_name_without_sensitive_data() {
        let input = r#"{
            "agent_action_name": "pre_run_command",
            "tool_info": {
                "tool_name": "Shell",
                "command_line": "echo super-secret-token"
            }
        }"#;

        let summary = summarize_hook_input(input);

        assert!(summary.contains("event=pre_run_command"));
        assert!(summary.contains("tool=Shell"));
        assert!(!summary.contains("super-secret-token"));
        assert!(!summary.contains("command_line"));
    }

    #[test]
    fn summarize_hook_input_handles_invalid_json_without_echoing_input() {
        let summary = summarize_hook_input("not json with secret");

        assert_eq!(summary, "invalid_json bytes=20");
        assert!(!summary.contains("secret"));
    }

    #[test]
    fn summarize_parsed_hook_input_does_not_include_sensitive_fields() {
        let inputs = [
            HookInput {
                event: HookEvent::BeforeCommand,
                tool_name: "Bash".to_string(),
                tool_input: ToolInput::Bash(BashInput {
                    command: "echo super-secret-token".to_string(),
                    timeout: None,
                }),
                session_id: Some("session-1".to_string()),
            },
            HookInput {
                event: HookEvent::AfterFileEdit,
                tool_name: "Write".to_string(),
                tool_input: ToolInput::File(FileOperationInput {
                    file_path: "/tmp/secret-path.rs".to_string(),
                    content: Some("API_KEY=super-secret-token".to_string()),
                }),
                session_id: None,
            },
            HookInput {
                event: HookEvent::Stop,
                tool_name: "Stop".to_string(),
                tool_input: ToolInput::Stop(StopInput {
                    agent_message: Some("assistant secret".to_string()),
                    response: Some("response secret".to_string()),
                    ..StopInput::default()
                }),
                session_id: None,
            },
            HookInput {
                event: HookEvent::AfterFileEdit,
                tool_name: "MultiEdit".to_string(),
                tool_input: ToolInput::Files(vec![FileOperationInput {
                    file_path: "/tmp/multi-secret.rs".to_string(),
                    content: Some("multi file secret".to_string()),
                }]),
                session_id: None,
            },
            HookInput {
                event: HookEvent::Passthrough,
                tool_name: "UnknownEvent".to_string(),
                tool_input: ToolInput::Other(serde_json::json!({
                    "prompt": "other secret",
                    "tool_input": { "command": "echo other secret" }
                })),
                session_id: None,
            },
            HookInput {
                event: HookEvent::SubagentStart,
                tool_name: "SubagentStart".to_string(),
                tool_input: ToolInput::Subagent(SubagentInput {
                    subagent_type: Some("private-agent".to_string()),
                    prompt: Some("prompt secret".to_string()),
                    ..SubagentInput::default()
                }),
                session_id: None,
            },
        ];

        for input in inputs {
            let summary = summarize_parsed_hook_input(&input);

            assert!(summary.contains("event="));
            assert!(summary.contains("tool="));
            assert!(!summary.contains("super-secret-token"));
            assert!(!summary.contains("API_KEY"));
            assert!(!summary.contains("secret-path"));
            assert!(!summary.contains("assistant secret"));
            assert!(!summary.contains("response secret"));
            assert!(!summary.contains("multi-secret"));
            assert!(!summary.contains("multi file secret"));
            assert!(!summary.contains("other secret"));
            assert!(!summary.contains("private-agent"));
            assert!(!summary.contains("prompt secret"));
        }
    }
}
