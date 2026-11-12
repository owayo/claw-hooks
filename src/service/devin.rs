//! Devin CLI / Devin Local のフック入出力。
//!
//! 従来の Windsurf Cascade とはイベント名・ツール名・出力契約が異なるため分ける。

use super::{FormatAdapter, reported_cwd, reported_session_id};
use crate::domain::{
    BashInput, Decision, FileOperationInput, HookEvent, HookInput, StopInput, StopSessionKind,
    ToolInput,
};
use anyhow::{Result, anyhow};
use serde_json::{Value, json};

impl FormatAdapter {
    pub(super) fn parse_devin_input(&self, input: &str) -> Result<HookInput> {
        let raw: Value =
            serde_json::from_str(input).map_err(|e| anyhow!("Failed to parse Devin input: {e}"))?;
        let event_name = raw["hook_event_name"]
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .ok_or_else(|| anyhow!("Missing hook_event_name field"))?;
        let event = match event_name {
            "PreToolUse" => HookEvent::BeforeCommand,
            "PermissionRequest" => HookEvent::PermissionRequest,
            "PostToolUse" => HookEvent::AfterFileEdit,
            "Stop" => HookEvent::Stop,
            _ => HookEvent::Passthrough,
        };
        let session_id = reported_session_id(raw.get("session_id"));
        if event == HookEvent::Stop {
            let stop_hook_active = raw["stop_hook_active"]
                .as_bool()
                .ok_or_else(|| anyhow!("Missing or invalid stop_hook_active field"))?;
            return Ok(HookInput {
                event,
                tool_name: "Stop".into(),
                tool_input: ToolInput::Stop(StopInput {
                    status: None,
                    loop_count: None,
                    response: None,
                    agent_message: None,
                    stop_hook_active,
                    // セッションの委譲を判別するフィールドは公式仕様に無い。
                    session_kind: StopSessionKind::Primary,
                }),
                session_id,
            });
        }
        let raw_tool_name = raw["tool_name"].as_str().filter(|s| !s.trim().is_empty());
        if matches!(
            event,
            HookEvent::BeforeCommand | HookEvent::PermissionRequest
        ) && raw_tool_name.is_none()
        {
            return Err(anyhow!("Missing tool_name field"));
        }
        let mut tool_name = raw_tool_name.unwrap_or(event_name).to_string();
        let args = &raw["tool_input"];
        let tool_input = match (event, tool_name.as_str()) {
            (HookEvent::BeforeCommand | HookEvent::PermissionRequest, "exec") => {
                let command = args["command"]
                    .as_str()
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing tool_input.command field"))?;
                tool_name = "Bash".into();
                ToolInput::Bash(BashInput {
                    command: command.into(),
                    timeout: None,
                    cwd: reported_cwd(raw.get("cwd")),
                })
            }
            (HookEvent::AfterFileEdit, "write" | "edit" | "notebook_edit") => {
                let file_path = ["file_path", "path", "notebook_path"]
                    .iter()
                    .filter_map(|key| args[*key].as_str())
                    .find(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow!("Missing edited file path in tool_input"))?;
                tool_name = "Write".into();
                ToolInput::File(FileOperationInput {
                    file_path: file_path.into(),
                    content: None,
                })
            }
            (HookEvent::AfterFileEdit, "apply_patch") => {
                let patch = args["patch"]
                    .as_str()
                    .or_else(|| args["command"].as_str())
                    .ok_or_else(|| anyhow!("Missing patch text in tool_input"))?;
                tool_name = "MultiEdit".into();
                ToolInput::Files(Self::parse_apply_patch_file_inputs(patch))
            }
            // 編集前・シェル実行後・管轄外イベントは入力を検証しない。
            _ => ToolInput::Other(Value::Null),
        };
        Ok(HookInput {
            event,
            tool_name,
            tool_input,
            session_id,
        })
    }

    pub(super) fn format_devin_output(
        &self,
        decision: &Decision,
        event: HookEvent,
    ) -> Result<String> {
        if event == HookEvent::Passthrough {
            return Ok("{}".into());
        }
        let output = match decision {
            Decision::Block { message } => json!({
                "decision": "block", "reason": self.normalize_and_truncate(message)
            }),
            Decision::Allow {
                additional_context: Some(context),
                ..
            } if event == HookEvent::AfterFileEdit && !context.trim().is_empty() => {
                json!({"hookSpecificOutput": {
                    "hookEventName": "PostToolUse",
                    "additionalContext": self.normalize_and_truncate(context)
                }})
            }
            // approve は権限確認を飛ばすため返さない。Stop の通知専用フィールドも未確認。
            Decision::Allow { .. } => json!({}),
        };
        Ok(output.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Format;

    #[test]
    fn devin_exec_is_checked_before_execution_only() {
        let adapter = FormatAdapter::new(Format::Devin, 0);
        for event in ["PreToolUse", "PermissionRequest"] {
            let input = json!({"hook_event_name":event, "tool_name":"exec",
                "tool_input":{"command":"rm -rf /tmp/demo"}, "session_id":"session",
                "cwd":"/tmp/project"});
            let parsed = adapter.parse_input(&input.to_string()).unwrap();
            assert!(parsed.is_shell_tool());
            let ToolInput::Bash(command) = parsed.tool_input else {
                panic!()
            };
            assert_eq!(command.cwd.as_deref(), Some("/tmp/project"));
            assert_eq!(parsed.session_id.as_deref(), Some("session"));
        }
        let parsed = adapter
            .parse_input(r#"{"hook_event_name":"PostToolUse","tool_name":"exec"}"#)
            .unwrap();
        assert!(matches!(parsed.tool_input, ToolInput::Other(_)));
    }

    #[test]
    fn devin_editor_paths_and_patch_moves() {
        let adapter = FormatAdapter::new(Format::Devin, 0);
        for (tool, key) in [
            ("write", "file_path"),
            ("edit", "path"),
            ("notebook_edit", "notebook_path"),
        ] {
            let input = json!({"hook_event_name":"PostToolUse", "tool_name":tool,
                "tool_input":{key:"demo.rs"}});
            let parsed = adapter.parse_input(&input.to_string()).unwrap();
            assert!(
                matches!(parsed.tool_input, ToolInput::File(file) if file.file_path == "demo.rs")
            );
        }
        let input = json!({"hook_event_name":"PostToolUse", "tool_name":"apply_patch",
            "tool_input":{"patch":"*** Begin Patch\n*** Update File: old.rs\n*** Move to: new.rs\n*** End Patch"}});
        let parsed = adapter.parse_input(&input.to_string()).unwrap();
        assert!(
            matches!(parsed.tool_input, ToolInput::Files(files) if files.len() == 1 && files[0].file_path == "new.rs")
        );
    }

    #[test]
    fn devin_scoped_validation_and_stop_loop_prevention() {
        let adapter = FormatAdapter::new(Format::Devin, 0);
        assert!(
            adapter
                .parse_input(r#"{"hook_event_name":"PreToolUse","tool_name":"exec"}"#)
                .is_err()
        );
        assert!(
            adapter
                .parse_input(
                    r#"{"hook_event_name":"PreToolUse","tool_input":{"command":"rm -rf /"}}"#
                )
                .is_err()
        );
        assert!(
            adapter
                .parse_input(r#"{"hook_event_name":"PreToolUse","tool_name":"edit"}"#)
                .is_ok()
        );
        assert!(
            adapter
                .parse_input(r#"{"hook_event_name":"SessionStart"}"#)
                .is_ok()
        );
        let input = r#"{"hook_event_name":"Stop","stop_hook_active":true}"#;
        let parsed = adapter.parse_input(input).unwrap();
        assert!(matches!(parsed.tool_input, ToolInput::Stop(stop) if stop.stop_hook_active));
        for input in [
            r#"{"hook_event_name":"Stop"}"#,
            "{\"hook_event_name\":\"Stop\",\"cut",
        ] {
            assert_eq!(adapter.format_error_for_input("broken", input), "{}");
            assert_eq!(adapter.error_exit_code(Some(input)), 0);
        }
        let input = r#"{"hook_event_name":"PermissionRequest","tool_name":"exec"}"#;
        assert_eq!(adapter.error_exit_code(Some(input)), 2);
        assert!(
            adapter
                .format_error_for_input("broken", input)
                .contains("\"decision\":\"block\"")
        );
    }
}
