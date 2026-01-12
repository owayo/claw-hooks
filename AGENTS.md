# AGENTS.md - AI Agent Instructions

Instructions for AI coding agents (Claude Code, Cursor, Windsurf, Codex, GitHub Copilot, etc.)

## Project Overview

**claw-hooks** - Hooks CLI for AI coding agents with TOML-based configuration.

- **Language**: Rust (MSRV 1.75)
- **Purpose**: Block dangerous commands, run formatters/linters on file save, send notifications on agent stop
- **Supported Agents**: Claude Code, Cursor, Windsurf

## Key Features

1. **Command Blocking**: `rm`/`kill`/`dd` → suggest `safe-rm`/`safe-kill`
2. **AST Parsing**: tree-sitter-bash for accurate command detection
3. **Custom Filters**: Regex-based command filtering
4. **Extension Hooks**: Auto-format/lint on file save
5. **Stop Hooks**: Notifications when agent loop ends

## Project Structure

```
src/
├── main.rs              # Entry point
├── cli.rs               # CLI (clap)
├── config/              # Configuration
│   ├── types.rs         # Config types
│   ├── service.rs       # Config loader
│   └── validation.rs    # Validation
├── service/             # Service layer
│   ├── adapter.rs       # Agent format conversion
│   └── hook_service.rs  # Hook processing
└── domain/              # Domain layer
    ├── types.rs         # Domain types
    ├── error.rs         # Error types
    ├── parser.rs        # Shell command parser
    ├── logger.rs        # Logging
    └── filters/         # Filter implementations
        ├── filter_trait.rs
        ├── chain.rs
        ├── rm_filter.rs
        ├── kill_filter.rs
        ├── dd_filter.rs
        ├── custom_filter.rs
        ├── extension_filter.rs
        └── stop_filter.rs
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
cargo clippy
cargo fmt --check

# Run
cargo run -- hook        # Process hook from stdin
cargo run -- init        # Generate default config
cargo run -- check       # Validate config
```

## Code Style

- Follow Rust idioms and conventions
- Use `thiserror` for error handling
- Keep functions small and focused
- Prefer iterators over loops where appropriate
- Document public APIs with rustdoc

## Architecture Decisions

1. **Layered Architecture**: config → service → domain separation
2. **Filter Chain Pattern**: Extensible filter pipeline
3. **Adapter Pattern**: Convert agent-specific JSON to internal types
4. **tree-sitter for AST**: Accurate shell command parsing

## Testing Guidelines

- Unit tests in same file with `#[cfg(test)]` module
- Integration tests in `tests/` directory
- Test both success and error cases
- Use descriptive test names

## Spec-Driven Development

This project uses Kiro-style Spec-Driven Development.

- **Steering**: `.kiro/steering/` - Project-wide rules
- **Specs**: `.kiro/specs/` - Feature specifications

Current spec status: All phases complete (v26.1.102)

## Agent-Specific Notes

### Claude Code
- Primary development agent
- Uses CLAUDE.md for additional instructions

### Cursor / Windsurf
- Refer to README.md for integration examples
- Use `--format cursor` or `--format windsurf` when testing

## Configuration

Default: `~/.config/claw-hooks/config.toml`

See README.md for full configuration reference.
