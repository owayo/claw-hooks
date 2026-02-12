# AGENTS.md - AI Agent Instructions

Instructions for AI coding agents (Claude Code, Cursor, Windsurf, Codex, GitHub Copilot, etc.)

## Project Overview

**claw-hooks** - Hooks CLI for AI coding agents with TOML-based configuration.

- **Language**: Rust (MSRV 1.75)
- **Version**: 26.2.104
- **Purpose**: Block dangerous commands, run formatters/linters on file save, send notifications on agent stop/subagent events
- **Supported Agents**: Claude Code, Cursor, Windsurf, Gemini CLI

## Key Features

1. **Command Blocking**: `rm`/`kill`/`dd` → suggest `safe-rm`/`safe-kill`
2. **AST Parsing**: tree-sitter-bash for accurate command detection (optional feature `ast-parser`)
3. **Custom Filters**: Regex-based and argument-based command filtering
4. **Extension Hooks**: Auto-format/lint on file save with timeout support
5. **Stop Hooks**: Run commands on agent stop (lint/typecheck, notifications, git commit, cleanup)
6. **Subagent Events**: NanoBuddy notifications on subagent start
7. **Output Normalization**: ANSI stripping and path prefix removal for token efficiency
8. **Fail-Closed Security**: Block on parse errors or empty input

## Project Structure

```
src/
├── main.rs              # Entry point
├── cli.rs               # CLI (clap) - Format/Commands enums
├── config/              # Configuration
│   ├── types.rs         # Config types (TOML deserialization)
│   ├── service.rs       # Config loader & generator
│   └── validation.rs    # Validation (regex, extensions, stop hooks)
├── service/             # Service layer
│   ├── adapter.rs       # Agent format conversion (Claude/Cursor/Windsurf/Gemini)
│   └── hook_service.rs  # Hook processing orchestration
├── domain/              # Domain layer
│   ├── types.rs         # Domain types (HookEvent, ToolInput, Decision)
│   ├── error.rs         # Error types (thiserror)
│   ├── parser.rs        # Shell command parser (tree-sitter / fallback)
│   ├── logger.rs        # Daily rotation logging (logroller)
│   ├── normalize.rs     # ANSI/whitespace/path normalization
│   ├── command.rs       # Timeout-aware command execution
│   └── filters/         # Filter implementations
│       ├── filter_trait.rs    # Filter trait definition
│       ├── chain.rs           # FilterChain (priority-based)
│       ├── rm_filter.rs       # rm/rmdir/del/erase blocking
│       ├── kill_filter.rs     # kill/pkill/killall blocking
│       ├── dd_filter.rs       # dd blocking
│       ├── custom_filter.rs   # Regex & Args mode filtering
│       ├── extension_filter.rs # File extension hooks
│       ├── stop_filter.rs     # Stop hooks (conditional/unconditional)
│       └── subagent_filter.rs # Subagent event handling
└── notify/              # Notification system
    └── nano_buddy.rs    # NanoBuddy via Darwin Notification API (macOS)
```

## Development Commands

```bash
# Build
cargo build              # Debug
cargo build --release    # Release

# Test
cargo test
cargo test -- --nocapture

# Lint
cargo clippy --all-targets --all-features -- -D warnings
cargo fmt --check

# Run
cargo run -- hook        # Process hook from stdin
cargo run -- init        # Generate default config
cargo run -- check       # Validate config
cargo run -- version     # Show version
```

## Code Style

- Follow Rust idioms and conventions
- Use `thiserror` for error types, `anyhow` for application errors
- Keep functions small and focused
- Prefer iterators over loops where appropriate
- Document public APIs with rustdoc

## Architecture Decisions

1. **Layered Architecture**: config → service → domain separation
2. **Filter Chain Pattern**: Priority-based extensible filter pipeline
3. **Adapter Pattern**: Convert agent-specific JSON to internal types
4. **tree-sitter for AST**: Accurate shell command parsing with fallback string parser
5. **Fail-Closed Security**: Block commands when input parsing fails

## Testing Guidelines

- Unit tests in same file with `#[cfg(test)]` module
- Integration tests in `tests/` directory
- Test both success and error cases
- Use descriptive test names

## Agent-Specific Notes

### Claude Code
- Primary development agent
- Uses CLAUDE.md (symlink to AGENTS.md) for instructions
- Format: `--format claude` (default)

### Cursor / Windsurf
- Refer to README.md for integration examples
- Use `--format cursor` or `--format windsurf` when testing

### Gemini CLI
- Supports BeforePrompt event
- Use `--format gemini` when testing

## README Update Rules

- `SubagentStart` and `SubagentStop` events are internal features and MUST NOT be documented in README.md or README.ja.md
- The `init` command's default config should not reference subagent events
- When updating READMEs, keep the supported hook events list as: `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`

## Configuration

Default: `~/.config/claw-hooks/config.toml`

See README.md for full configuration reference.
