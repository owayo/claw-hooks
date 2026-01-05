//! Hook processing service.

use std::io::{self, BufRead, Write};
use std::process;

use anyhow::Result;
use tracing::{debug, error, info};

use crate::cli::Format;
use crate::config::Config;
use crate::domain::{Decision, FilterChain, HookInput};
use crate::service::adapter::FormatAdapter;

/// Service for processing hook events.
pub struct HookService {
    config: Config,
    filter_chain: FilterChain,
    adapter: FormatAdapter,
}

impl HookService {
    /// Create a new HookService with the specified format.
    pub fn new(config: Config, format: Format) -> Self {
        let filter_chain = FilterChain::new(&config);
        let adapter = FormatAdapter::new(format);
        Self {
            config,
            filter_chain,
            adapter,
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

        if input.is_empty() {
            error!("No input received from stdin");
            // SECURITY: Use fail-closed - block when no input received
            let output_json = self.adapter.format_error("No input received from stdin");
            writeln!(stdout, "{}", output_json)?;
            process::exit(self.adapter.error_exit_code());
        }

        debug!("Received input: {}", input);

        // Parse input using format adapter
        let hook_input: HookInput = match self.adapter.parse_input(&input) {
            Ok(input) => input,
            Err(e) => {
                let error_msg = format!("Failed to parse input: {}", e);
                error!("{}", error_msg);
                // Output error in the appropriate format with message
                // SECURITY: Use fail-closed exit code (2 = block)
                let output_json = self.adapter.format_error(&error_msg);
                writeln!(stdout, "{}", output_json)?;
                process::exit(self.adapter.error_exit_code());
            }
        };

        // Process the hook
        let decision = self.process(&hook_input);
        let exit_code = self.adapter.exit_code(&decision);

        // Write output using format adapter
        let output_json = self.adapter.format_output(&decision)?;
        info!("Output: {}", output_json);
        writeln!(stdout, "{}", output_json)?;

        process::exit(exit_code);
    }

    /// Process hook input and return decision.
    pub fn process(&self, input: &HookInput) -> Decision {
        debug!(
            "Processing hook: event={}, tool_name={}",
            input.event, input.tool_name
        );

        match input.event.as_str() {
            "PreToolUse" => self.handle_pre_tool_use(input),
            "PostToolUse" => self.handle_post_tool_use(input),
            "Stop" => self.handle_stop(input),
            _ => {
                debug!("Unknown event type: {}", input.event);
                Decision::Allow
            }
        }
    }

    /// Handle PreToolUse event.
    fn handle_pre_tool_use(&self, input: &HookInput) -> Decision {
        debug!("Handling PreToolUse for tool: {}", input.tool_name);

        // Run through filter chain
        self.filter_chain.execute(input)
    }

    /// Handle PostToolUse event.
    fn handle_post_tool_use(&self, input: &HookInput) -> Decision {
        if self.config.debug {
            debug!(
                "PostToolUse: tool_name={}, tool_input={:?}",
                input.tool_name, input.tool_input
            );
        }

        // For Write/Edit/MultiEdit, run through filter chain for extension hooks
        // This enables:
        // - Claude Code: PostToolUse with Write
        // - Cursor: afterFileEdit (mapped to PostToolUse + Write)
        // - Windsurf: post_write_code (mapped to PostToolUse + Write)
        if matches!(input.tool_name.as_str(), "Write" | "Edit" | "MultiEdit") {
            return self.filter_chain.execute(input);
        }

        // Other PostToolUse events always allow
        Decision::Allow
    }

    /// Handle Stop event.
    fn handle_stop(&self, input: &HookInput) -> Decision {
        info!("Stop event received: session_id={:?}", input.session_id);

        // Execute stop hooks through the filter chain
        self.filter_chain.execute(input)
    }
}
