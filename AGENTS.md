# AGENTS.md - AI Agent Instructions

Instructions for AI coding agents (Claude Code, Cursor, Windsurf, Codex, GitHub Copilot, etc.)

## Project Overview

**claw-hooks** - Hooks CLI for AI coding agents with TOML-based configuration.

- **Language**: Rust (MSRV 1.85)
- **Version**: 26.5.104
- **Purpose**: Block dangerous commands, run formatters/linters only after file save/edit completes, send notifications on agent stop/subagent events
- **Supported Agents**: Claude Code, Cursor, Windsurf, Gemini CLI, Codex CLI

## Key Features

1. **Command Blocking**: `rm`/`kill`/`dd` → suggest `safe-rm`/`safe-kill`
2. **AST Parsing**: tree-sitter-bash for accurate command detection (optional feature `ast-parser`), with command basename/executable-extension/case normalization (`/bin/rm`, `RM.EXE`), shell quote-removal handling (`r\m`, `r''m`, `$'r\x6d'`), and fallback parsing that also handles subshells/command substitutions/env-prefix commands, single `&` (background), newline separators, `eval`, `find -exec`/`-execdir`, `xargs -I`/`xargs sh -c`, Windows `cmd /c`, and wrapper options (e.g., `sudo -n`, `/usr/bin/sudo -u root rm`, `sudo --user root`, `sudo VAR=value rm`, `timeout --signal TERM 10 rm`, `command rm`, `exec rm`)
3. **Custom Filters**: Regex-based and argument-based command filtering
4. **Extension Hooks**: Auto-format/lint on AfterFileEdit events only (not on BeforeCommand) with timeout support (`{file}` must appear exactly once, parent-directory traversal, shell redirection paths, tabs/newlines/NUL are blocked; on Windows, `cmd /c` shell metacharacters such as `%`, `!`, `^`, `"` are also rejected from file paths to prevent variable-expansion injection)
5. **Stop Hooks**: Run commands on agent stop (lint/typecheck, notifications, git commit, cleanup). `report=true` hooks are waited on and failures are returned to the agent; `report=false` hooks are started detached with stdin/stdout/stderr discarded.
6. **Subagent Events**: NanoBuddy notifications on subagent start
7. **Output Normalization**: ANSI stripping (CSI/OSC/SS2/SS3), path prefix removal including absolute paths under directories with spaces, repeated decorative character compression (`.`, `=`, `-`, `─`, `━`, `^`, `·`, `→`) plus trailing progress ellipsis compression and rustc-style location marker compression (`-->` / `---->` → `->`, single-hyphen `->` preserved so function return types stay intact), space-separated decorative run compression for biome diff visualization (`→ → → → → → → Google` → `→ Google`, applies to `·` and `→` with single-space delimiter), biome duplicate diff context line-number compression (`129 129 │ text` → `129 │ text` when both line numbers are identical integers; differing pairs like `10 9 │` are preserved as informative), repeated-prefix line collapsing for noisy progress logs (e.g., cargo `Compiling foo v1.0`), and character-count-based output length truncation for token efficiency
8. **Fail-Closed Security**: Block on parse errors, empty input, or missing required agent fields. Unknown/unsupported event names are passed through as allow (fail-open) to keep claw-hooks within its intentional scope.
9. **Process Group Isolation (Unix)**: Hook subprocesses are placed in their own process group via `setpgid`, so timeouts kill the whole tree (`killpg`) — preventing grandchild leaks like `sh -c 'sleep ...'` after the shell is signaled.
10. **Debug Log Safety**: Debug logs store hook event summaries (event/tool/session/size) instead of raw hook input, so tool command text and file contents are not persisted to disk.

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
│   ├── adapter.rs       # Agent format conversion (Claude/Cursor/Windsurf/Gemini/Codex)
│   └── hook_service.rs  # Hook processing orchestration
├── domain/              # Domain layer
│   ├── types.rs         # Domain types (HookEvent, ToolInput, Decision)
│   ├── parser.rs        # Shell command parser (tree-sitter / fallback)
│   ├── logger.rs        # Daily rotation logging (logroller)
│   ├── normalize.rs     # ANSI/whitespace/path normalization
│   ├── command.rs       # Timeout-aware command execution
│   └── filters/         # Filter implementations
│       ├── filter_trait.rs    # Filter trait definition
│       ├── builtin_filter.rs  # Built-in command filter (shared implementation)
│       ├── chain.rs           # FilterChain (priority-based)
│       ├── rm_filter.rs       # rm/rmdir/del/erase blocking
│       ├── kill_filter.rs     # kill/pkill/killall blocking
│       ├── dd_filter.rs       # dd blocking
│       ├── custom_filter.rs   # Regex & Args mode filtering
│       ├── extension_filter.rs # File extension hooks
│       ├── stop_filter.rs     # Stop hooks (conditional/reported/detached)
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
4. **tree-sitter for AST**: Accurate shell command parsing with robust fallback string parser for non-AST builds
5. **Fail-Closed Security**: Block commands when input parsing fails
6. **Intentional Scope Limits**: This tool is for command blocking, post-edit hooks, stop hooks, and subagent notifications. It must not expand into general lifecycle/prompt orchestration. `SessionStart`, `UserPromptSubmit`, and internal `BeforePrompt`-style events are intentionally out of scope and should remain pass-through/allow-by-design unless the project direction explicitly changes.

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
- PreToolUse Allow: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}`
- PreToolUse Block: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"..."}}`（トップレベル `decision`/`reason` は deprecated のため使用しない）
- PostToolUse Allow: `{}` (追加コンテキストがある場合は `hookSpecificOutput.additionalContext` を含む)
- PostToolUse Block: `{"decision":"block","reason":"..."}`
- Stop output: Allow = `{}`, Block = `{"decision":"block","reason":"..."}`
- 通常の許可/ブロック判定は stdout JSON + exit code 0 で返す（Claude は exit 0 のときだけ stdout JSON を解析する）
- Fail-closed errors: exit code 2 + stderr に `{"decision":"block","reason":"..."}` を出す
- 未対応イベント（`StopFailure`, `PermissionRequest`, `PreCompact` 等）は allow でパススルー（Cursor / Codex / Gemini と同じ挙動）

### Cursor
- Refer to README.md for integration examples
- Use `--format cursor` when testing
- Input parsing uses `hook_event_name` field for event identification (not field-structure matching)
- Supported events: `preToolUse` for `Shell`/`Bash`, `beforeShellExecution`, `afterFileEdit`, `afterTabFileEdit`, `stop`, `subagentStart`, `subagentStop`
- Unsupported events (e.g., `afterShellExecution`, non-shell `preToolUse`, `postToolUse`) are passed through as allow

### Windsurf
- Refer to README.md for integration examples
- Use `--format windsurf` when testing
- BeforeCommand (pre_run_command) Block: exit code 2 + stderr にメッセージ出力
- Stop は `post_cascade_response` に対応する事後フックのためベストエフォート。stop hook が失敗しても `{}` を返し、エージェントにはブロックを返さない
- 未対応の `agent_action_name` は allow でパススルーする（全イベント対応が目的ではないため）
- フェイルクローズ（パースエラー/空入力）: exit code 2 + stderr にメッセージ出力

### Gemini CLI
- Supports BeforeTool, AfterTool, BeforeAgent, AfterAgent events
- AfterTool の追加コンテキストは `hookSpecificOutput.additionalContext` で返す
- 未対応イベントは allow でパススルーする（全イベント対応が目的ではないため）
- Use `--format gemini` when testing

### Codex CLI
- Supports these hook events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`
- Use `--format codex` when testing
- Allow output: `{}` (empty JSON, exit 0)
- Block output: `{"decision":"block","reason":"..."}` (legacy format, officially accepted)
- PermissionRequest Block output: `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"..."}}}`
- Missing required Codex fields must be treated as fail-closed parse errors
- PermissionRequest の parse error も PermissionRequest 専用 deny schema で返す
- PostToolUse の追加コンテキストは `hookSpecificOutput.additionalContext` で返す
- Codex `PreToolUse` / `PermissionRequest` / `PostToolUse` can receive `Bash` and `apply_patch`; `apply_patch` is mapped to `MultiEdit` and its patch command is parsed for changed file paths
- Official docs: https://developers.openai.com/codex/hooks

## README Update Rules

- `SubagentStart` and `SubagentStop` events are internal features and MUST NOT be documented in README.md or README.ja.md
- The `init` command's default config should not reference subagent events
- When updating READMEs, keep the supported hook events list as: `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`

## Configuration

Default: `~/.config/claw-hooks/config.toml`

See README.md for full configuration reference.
