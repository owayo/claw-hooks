//! Hook processing service.

use std::io::{self, BufRead, Write};
use std::process;

use anyhow::Result;
use tracing::{debug, error, info};

use crate::cli::Format;
use crate::config::Config;
use crate::domain::{Decision, FilterChain, HookEvent, HookInput};
use crate::service::adapter::FormatAdapter;

/// Service for processing hook events.
pub struct HookService {
    config: Config,
    filter_chain: FilterChain,
    adapter: FormatAdapter,
    /// Trace mode: output raw input to stderr for debugging
    trace: bool,
}

impl HookService {
    /// Create a new HookService with the specified format.
    pub fn new(config: Config, format: Format, trace: bool) -> Self {
        let filter_chain = FilterChain::new(&config);
        let adapter = FormatAdapter::new(format);
        Self {
            config,
            filter_chain,
            adapter,
            trace,
        }
    }

    /// Run the hook processing loop.
    ///
    /// Reads JSON input from stdin, processes it, and writes JSON output to stdout.
    /// The input/output format depends on the configured agent format.
    pub fn run(&self) -> Result<()> {
        let stdin = io::stdin();
        let stdout = io::stdout();
        let mut stdout = stdout.lock();

        // Read all input from stdin
        let mut input = String::new();
        for line in stdin.lock().lines() {
            input.push_str(&line?);
        }

        // Trace mode: output raw input to stderr immediately
        if self.trace {
            eprintln!("🔍 [TRACE] Raw input received:");
            eprintln!("{}", input);
            eprintln!("🔍 [TRACE] End of input");
        }

        if input.is_empty() {
            if self.trace {
                eprintln!("🔍 [TRACE] ERROR: No input received from stdin");
            }
            error!("No input received from stdin");
            // SECURITY: Use fail-closed - block when no input received
            let output_json = self.adapter.format_error("No input received from stdin");
            writeln!(stdout, "{}", output_json)?;
            stdout.flush()?; // Ensure output is flushed before exit (important for pipes)
            process::exit(self.adapter.error_exit_code());
        }

        debug!("Received input: {}", input);

        // Parse input using format adapter
        let hook_input: HookInput = match self.adapter.parse_input(&input) {
            Ok(parsed) => {
                if self.trace {
                    eprintln!("🔍 [TRACE] Parsed input:");
                    eprintln!("  event: {:?}", parsed.event);
                    eprintln!("  tool_name: {}", parsed.tool_name);
                    eprintln!("  tool_input: {:?}", parsed.tool_input);
                    eprintln!("  session_id: {:?}", parsed.session_id);
                }
                parsed
            }
            Err(e) => {
                let error_msg = format!("Failed to parse input: {}", e);
                if self.trace {
                    eprintln!("🔍 [TRACE] Parse error: {}", error_msg);
                }
                error!("{}", error_msg);
                // Output error in the appropriate format with message
                // SECURITY: Use fail-closed exit code (2 = block)
                let output_json = self.adapter.format_error(&error_msg);
                writeln!(stdout, "{}", output_json)?;
                stdout.flush()?; // Ensure output is flushed before exit (important for pipes)
                process::exit(self.adapter.error_exit_code());
            }
        };

        // Process the hook
        let decision = self.process(&hook_input);
        let exit_code = self.adapter.exit_code(&decision, hook_input.event);

        if self.trace {
            eprintln!("🔍 [TRACE] Decision: {:?}", decision);
            eprintln!("🔍 [TRACE] Exit code: {}", exit_code);
        }

        // Write output using format adapter
        let output = self.adapter.format_output(&decision, hook_input.event)?;

        if self.trace {
            eprintln!("🔍 [TRACE] Output:");
            eprintln!("{}", output);
        }

        let emoji = if matches!(decision, crate::domain::Decision::Block { .. }) {
            "🚫"
        } else {
            "✅"
        };
        info!("Output {}: {}", emoji, output);

        // Windsurf Stop Block outputs to stderr (agent reads stderr on exit 2)
        if self.adapter.use_stderr(&decision, hook_input.event) {
            let stderr = io::stderr();
            let mut stderr = stderr.lock();
            writeln!(stderr, "{}", output)?;
            stderr.flush()?;
        } else {
            writeln!(stdout, "{}", output)?;
            stdout.flush()?; // Ensure output is flushed before exit (important for pipes)
        }

        process::exit(exit_code);
    }

    /// Process hook input and return decision.
    pub fn process(&self, input: &HookInput) -> Decision {
        debug!(
            "Processing hook: event={:?}, tool_name={}",
            input.event, input.tool_name
        );

        match input.event {
            HookEvent::BeforeCommand => self.handle_before_command(input),
            HookEvent::AfterFileEdit => self.handle_after_file_edit(input),
            HookEvent::Stop => self.handle_stop(input),
            HookEvent::BeforePrompt => self.handle_before_prompt(input),
            HookEvent::SubagentStart | HookEvent::SubagentStop => self.handle_subagent(input),
        }
    }

    /// Handle BeforeCommand event (pre-tool-use).
    fn handle_before_command(&self, input: &HookInput) -> Decision {
        debug!("Handling BeforeCommand for tool: {}", input.tool_name);

        // Run through filter chain
        self.filter_chain.execute(input)
    }

    /// Handle AfterFileEdit event (post-tool-use for file operations).
    fn handle_after_file_edit(&self, input: &HookInput) -> Decision {
        if self.config.debug {
            debug!(
                "AfterFileEdit: tool_name={}, tool_input={:?}",
                input.tool_name, input.tool_input
            );
        }

        // For Write/Edit/MultiEdit, run through filter chain for extension hooks
        // This enables:
        // - Claude Code: PostToolUse with Write
        // - Cursor: afterFileEdit (mapped to AfterFileEdit + Write)
        // - Windsurf: post_write_code (mapped to AfterFileEdit + Write)
        if matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit") {
            return self.filter_chain.execute(input);
        }

        // Other AfterFileEdit events always allow
        Decision::allow()
    }

    /// Handle Stop event.
    fn handle_stop(&self, input: &HookInput) -> Decision {
        info!("Stop event received: session_id={:?}", input.session_id);

        // Execute stop hooks through the filter chain
        self.filter_chain.execute(input)
    }

    /// Handle BeforePrompt event (Gemini CLI only).
    fn handle_before_prompt(&self, _input: &HookInput) -> Decision {
        debug!("Handling BeforePrompt event");

        // BeforePrompt is currently a pass-through event
        Decision::allow()
    }

    /// Handle SubagentStart/SubagentStop events.
    fn handle_subagent(&self, input: &HookInput) -> Decision {
        info!(
            "Subagent event received: {:?}, session_id={:?}",
            input.event, input.session_id
        );

        // Execute through filter chain (SubagentFilter handles NanoBuddy notifications)
        self.filter_chain.execute(input)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::{BashInput, FileOperationInput, StopInput, SubagentInput, ToolInput};

    fn make_service() -> HookService {
        let config = Config::default();
        HookService::new(config, Format::Claude, false)
    }

    fn make_bash_input(command: &str) -> HookInput {
        HookInput {
            event: HookEvent::BeforeCommand,
            tool_name: "Bash".to_string(),
            tool_input: ToolInput::Bash(BashInput {
                command: command.to_string(),
                timeout: None,
            }),
            session_id: None,
        }
    }

    #[test]
    fn test_process_allows_safe_command() {
        let service = make_service();
        let input = make_bash_input("ls -la");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_blocks_rm() {
        let service = make_service();
        let input = make_bash_input("rm -rf /tmp/foo");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_kill() {
        let service = make_service();
        let input = make_bash_input("kill -9 1234");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_dd() {
        let service = make_service();
        let input = make_bash_input("dd if=/dev/zero of=/dev/sda");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_after_file_edit_write_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Write".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: Some("content".to_string()),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_read_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Read".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_stop_event_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::Stop,
            tool_name: "Stop".to_string(),
            tool_input: ToolInput::Stop(StopInput::default()),
            session_id: Some("session-123".to_string()),
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_before_prompt_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::BeforePrompt,
            tool_name: "BeforePrompt".to_string(),
            tool_input: ToolInput::Other(serde_json::json!({})),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_subagent_start_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::SubagentStart,
            tool_name: "SubagentStart".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some("explore".to_string()),
                prompt: Some("Search the codebase".to_string()),
                status: None,
                duration: None,
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_subagent_stop_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::SubagentStop,
            tool_name: "SubagentStop".to_string(),
            tool_input: ToolInput::Subagent(SubagentInput {
                subagent_type: Some("explore".to_string()),
                prompt: None,
                status: Some("completed".to_string()),
                duration: Some(5000),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_blocks_sudo_rm() {
        let service = make_service();
        let input = make_bash_input("sudo rm -rf /");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_blocks_piped_kill() {
        let service = make_service();
        let input = make_bash_input("ps aux | grep node | xargs kill");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_with_custom_filter() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "yarn".to_string(),
            args: vec![],
            message: "Use pnpm instead".to_string(),
        });
        let service = HookService::new(config, Format::Claude, false);

        let input = make_bash_input("yarn install");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Block { .. }));
    }

    #[test]
    fn test_process_custom_filter_allows_non_matching() {
        let mut config = Config::default();
        config.custom_filters.push(crate::config::CustomFilter {
            command: "yarn".to_string(),
            args: vec![],
            message: "Use pnpm instead".to_string(),
        });
        let service = HookService::new(config, Format::Claude, false);

        let input = make_bash_input("pnpm install");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_with_disabled_rm_block() {
        let config = Config {
            rm_block: false,
            ..Config::default()
        };
        let service = HookService::new(config, Format::Claude, false);
        let input = make_bash_input("rm file.txt");
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_non_write_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Grep".to_string(),
            tool_input: ToolInput::Other(serde_json::json!({"pattern": "test"})),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_edit_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "Edit".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: Some("new content".to_string()),
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }

    #[test]
    fn test_process_after_file_edit_multi_edit_tool_allows() {
        let service = make_service();
        let input = HookInput {
            event: HookEvent::AfterFileEdit,
            tool_name: "MultiEdit".to_string(),
            tool_input: ToolInput::File(FileOperationInput {
                file_path: "/tmp/test.txt".to_string(),
                content: None,
            }),
            session_id: None,
        };
        let decision = service.process(&input);
        assert!(matches!(decision, Decision::Allow { .. }));
    }
}
