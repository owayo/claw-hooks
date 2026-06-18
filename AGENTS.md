# AGENTS.md - AI Agent Instructions

Instructions for AI coding agents (Claude Code, Cursor, Windsurf, Codex, GitHub Copilot, etc.)

## Project Overview

**claw-hooks** - Hooks CLI for AI coding agents with TOML-based configuration.

- **Language**: Rust (MSRV 1.85)
- **Version**: 26.6.100
- **Purpose**: Block dangerous commands, run formatters/linters only after file save/edit completes, send notifications on agent stop/subagent events
- **Supported Agents**: Claude Code, Cursor, Windsurf, Gemini CLI, Codex CLI

## Key Features

1. **Command Blocking**: `rm`/`kill`/`dd` → suggest `safe-rm`/`safe-kill`
2. **AST Parsing**: tree-sitter-bash for accurate command detection (optional feature `ast-parser`), with command basename/executable-extension/case normalization (`/bin/rm`, `RM.EXE`), shell quote-removal handling (`r\m`, `r''m`, `$'r\x6d'`), and fallback parsing that also handles subshells/command substitutions/env-prefix commands, single `&` (background), newline separators, `eval`, `find -exec`/`-execdir`, `xargs -I`/`xargs sh -c`, Windows `cmd /c`, and wrapper options (e.g., `sudo -n`, `/usr/bin/sudo -u root rm`, `sudo --user root`, `sudo VAR=value rm`, `timeout --signal TERM 10 rm`, `command rm`, `exec rm`), value-taking flags followed by numeric arguments (`xargs -n 1 rm`, `sudo -u 1000 rm`, `nice -n 10 rm` — the numeric token is preserved so the flag does not swallow the real command), `bash -c --` option terminators, shell `-c` clustered with inline script (`bash -c'rm -rf /'`, `bash -lc'rm -rf /'` — shells join `-c` and the immediately-quoted script into a single argv token `-crm -rf /`; the parser splits the script out of the cluster so the inner command is still detected and cannot bypass the filter), and `env -S` / `--split-string` command strings (env-specific, so `sort -S` etc. are not misparsed), command-position brace expansion (`{rm,-rf,/p}`, `/bin/{rm,ls}`, `{r,}m` → first-choice expansion so the launched command is still detected), brace command groups (`{ rm -rf /; }` — the leading `{` group delimiter is skipped so the inner command is still detected, including in the fallback parser), shell control structures in the fallback parser (`if … then rm …`, `for/select … do rm …`, `while/until … do rm …`, `case W in P) rm …`, `! rm …` — the control keywords, loop variables, and `case` patterns are skipped so the inner command is still detected; loop variables and patterns named like a blocked command, e.g. `for rm in …` or `case x in rm) …`, are NOT misdetected as that command; command substitutions inside loop/case headers such as `for f in $(rm …)` are also extracted, preventing the fallback parser from failing open on control-flow-wrapped dangerous commands), process substitution (`diff <(rm ...) <(ls)`, `tee >(rm ...)` — the inner command actually runs, so it is extracted; bare redirections `< file` / `> file` are not misdetected), and additional execution-delegation wrappers (`setsid`, `stdbuf`, `taskset` mask, `nsenter`, `unshare`, `setpriv`, `chroot`/`flock` with their leading directory/lockfile positional, `busybox`/`toybox` applets, `su`/`runuser`/`flock -c` shell-string re-evaluation, `pkexec` (Polkit) and `gosu` (container-friendly setuid wrapper that takes a leading `<user[:group]>` positional and runs the rest as that user) — both expanded so `pkexec rm -rf /` / `gosu root rm -rf /` cannot bypass the filter under elevated privileges). Wrapper chains (`sudo sudo … rm`) are expanded iteratively rather than recursively so even a very long chain (within the size limit) cannot overflow the stack and fail open. The custom-filter command-string extractor uses the same top-level terminator splitter (`;`, newline, single `&`) as the AST/fallback dangerous-command path, so anchored regex filters (`^rm `) cannot be evaded by chaining the target command after a newline or `&` (e.g., `echo ok\nrm -rf /tmp`, `echo ok & rm -rf /tmp`).
3. **Custom Filters**: Regex-based and argument-based command filtering
4. **Extension Hooks**: Auto-format/lint on AfterFileEdit events only (not on BeforeCommand) with timeout support (`{file}` must appear exactly once, parent-directory traversal, shell redirection paths, tabs/newlines/NUL are blocked; on Windows, `cmd /c` shell metacharacters such as `%`, `!`, `^`, `"` are also rejected from file paths to prevent variable-expansion injection)
5. **Stop Hooks**: Run commands on agent stop (lint/typecheck, notifications, git commit, cleanup). `report=true` hooks are waited on and failures are returned to the agent; `report=false` hooks are started detached with stdin/stdout/stderr discarded. Loop prevention is three-layered: a cross-process env flag, the agent's `stop_hook_active` flag (Claude/Codex), and Cursor's `loop_count` (>= 1 means a follow-up was already fired, so all stop hooks are skipped — symmetric with `stop_hook_active`).
6. **Subagent Events**: NanoBuddy notifications on subagent start
7. **Output Normalization**: ANSI stripping (CSI/OSC/SS2/SS3), path prefix removal including absolute paths under directories with spaces (the prefix is only stripped when both the preceding character and the following character match the path-boundary conditions used by `extract_absolute_paths` — line start / whitespace / `-` / `>` / `=` before, and a filename character after — so that a common prefix appearing incidentally inside non-path text such as `foo/proj/src/...` is not silently mangled), repeated decorative character compression (`.`, `=`, `-`, `─`, `━`, `^`, `·`, `→`, `_`) — the `_` rule collapses rustc/ruff/biome multiline span underlines (`| |_______^` → `| |_^`) while snake_case separators (1–2 underscores) are preserved — plus trailing progress ellipsis compression and rustc-style location marker compression (`-->` / `---->` → `->`, single-hyphen `->` preserved so function return types stay intact), space-separated decorative run compression for biome diff visualization (`→ → → → → → → Google` → `→ Google`, applies to `·` and `→` with single-space delimiter), biome duplicate diff context line-number compression (`129 129 │ text` → `129 │ text`, empty context lines `129 129 │` → `129 │` when both line numbers are identical integers; differing pairs like `10 9 │` are preserved as informative), diagnostic frame-line removal (lines composed solely of `|` / `^` / `_` / Box Drawing characters (U+2500–U+257F: `│`, `─`, `╭─╮` / `╰─╯` banner frames from pnpm-style update notices, biome `─────` separators) and whitespace — ruff/biome/rustc caret and separator scaffolding — are dropped entirely, since the pointed-at column already appears numerically in the preceding `file:line:col` header; lines with any alphanumeric content such as headings, source lines, labelled carets `| ^ expected u32`, banner body lines `│ Update available! │`, or snake_case-containing lines like `__init__` are preserved — `_` is in the dropped set so rustc/clippy multiline span underlines collapsed to `| |_^` are removed, but any letter/digit keeps the line), progress-prefix line collapsing limited to a whitelist of progress words (`Compiling`, `Checking`, `Building`, `Blocking` — cargo's repeated `Blocking waiting for file lock on ...` lines — etc.) where a run of consecutive lines each starting with any whitelisted progress word is collapsed as one run even when the words differ (cargo's concurrent build interleaves `Compiling`/`Checking`, so a same-word-only rule would leave most progress noise uncollapsed) — single-word runs summarize as `... (and N more lines starting with "<word>")`, mixed-word runs as `... (and N more progress lines)`, while diagnostic lines (`error:` / `warning:`) break the run and are never collapsed so their per-line information is preserved, and character-count-based output length truncation for token efficiency
8. **Fail-Closed Security**: Block on parse errors, empty input, or missing required agent fields. Pathologically deep command nesting or oversized command input is also treated as a block candidate, avoiding recursion-based stack overflow that would otherwise crash and fail open. The pathological-input guard counts brace nesting (`{`/`}`) in addition to parentheses, and the AST walk enforces an explicit traversal-depth limit, so deeply nested command groups (`{ ...;}`) cannot overflow the stack and fail open even under small thread stacks (Windows/containers). Wrapper-chain expansion (`sudo sudo … rm`) is iterative with an explicit recursion bound rather than self-recursive, so a long wrapper chain that slips under the size limit cannot overflow the stack; chains exceeding the bound fail closed. The stdin reader is bounded (4 MiB) so an oversized or runaway hook payload is treated as fail-closed rather than allocating until the process is OOM-killed (which would otherwise drop the safeguard entirely). Unknown/unsupported event names are passed through as allow (fail-open) to keep claw-hooks within its intentional scope.
9. **Process Group Isolation (Unix)**: Hook subprocesses are placed in their own process group via `setpgid`, so timeouts kill the whole tree (`killpg`) — preventing grandchild leaks like `sh -c 'sleep ...'` after the shell is signaled.
10. **Debug Log Safety**: Debug logs store hook event summaries (event/tool/session/size) instead of raw hook input or raw parsed `ToolInput`, so tool command text, file contents, unknown event payloads, and agent messages are not persisted to disk. Per-agent parsed-input debug lines log byte counts and booleans (`command_bytes`, `file_path_bytes`, `has_cwd`) rather than raw command/file-path/cwd values, and the formatted hook output is logged as a decision + size summary; the full output body (which may include lint diagnostics quoting source lines) is available only via `--trace`, which writes to stderr and is not persisted. Extension-hook logs record the program name with arg-count / inline-template booleans and the resolved file path byte length instead of the expanded command line — the actual file path and rendered command (which can contain user-directory hierarchies or source-quoting lint snippets) are kept off disk. Stop-hook output logging records only the byte size of stdout/stderr per hook; the full diagnostic body is still relayed to the agent via the block `reason`/`followup_message`, but it is never persisted to disk.

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
- Fail-closed errors: exit code 2 + stderr にメッセージ本文をプレーンテキストで出力（Claude は exit 2 のとき stdout/JSON を解析せず stderr 本文をエラーメッセージとして扱うため、`{"decision":...}` のような JSON ではなく本文のみを出す。exit 2 自体がブロックを意味するためフェイルクローズドは維持される）
- 未対応イベント（`StopFailure`, `PermissionRequest`, `PreCompact` 等）は allow でパススルー（Cursor / Codex / Gemini と同じ挙動）

### Cursor
- Refer to README.md for integration examples
- Use `--format cursor` when testing
- Input parsing uses `hook_event_name` field for event identification (not field-structure matching)
- Supported events: `preToolUse` for `Shell`/`Bash`, `beforeShellExecution`, `afterFileEdit`, `afterTabFileEdit`, `stop`, `subagentStart`, `subagentStop`
- Unsupported events (e.g., `afterShellExecution`, non-shell `preToolUse`, `postToolUse`) are passed through as allow
- `stop` / `subagentStop` の Output スキーマは公式に `{ followup_message?: string }` のみ（`permission` フィールドは存在しない）。Allow は `{}`、Block は `followup_message` に修正指示を入れて返す
- `stop` の `loop_count`（stop hook 由来の自動フォローアップ発火回数、0 始まり）が 1 以上の場合は全 stop hook をスキップする。Cursor は `stop_hook_active` を持たないため、`loop_count` が無限フォローアップループ防止の役割を担う（Cursor 側の `loop_limit`(デフォルト5) に頼らず 1 回で止める）

### Windsurf
- Refer to README.md for integration examples
- Use `--format windsurf` when testing
- BeforeCommand (pre_run_command) Block: exit code 2 + stderr にメッセージ本文をプレーンテキストで出力（Windsurf は stdout/stderr を JSON 解析せず stderr を表示用テキストとして扱うため、`{"decision":...}` のような JSON ではなく本文のみを出す）
- Stop は `post_cascade_response` に対応する事後フックのためベストエフォート。stop hook が失敗しても `{}` を返し、エージェントにはブロックを返さない
- 未対応の `agent_action_name` は allow でパススルーする（全イベント対応が目的ではないため）
- フェイルクローズ（パースエラー/空入力）: exit code 2 + stderr にメッセージ本文をプレーンテキストで出力

### Gemini CLI
- Supports BeforeTool, AfterTool, BeforeAgent, AfterAgent events
- AfterTool の追加コンテキストは `hookSpecificOutput.additionalContext` で返す
- 未対応イベントは allow でパススルーする（全イベント対応が目的ではないため）
- Use `--format gemini` when testing

### Codex CLI
- Supports these hook events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `Stop`
- Use `--format codex` when testing
- Allow output: `{}` (empty JSON, exit 0)
- PreToolUse Block output: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"..."}}`（公式ドキュメントの主形式。legacy の `{"decision":"block"}` も受理されるが使用しない）
- PostToolUse / Stop Block output: `{"decision":"block","reason":"..."}`（これらのイベントではこれが正式形式。Stop では reason が継続プロンプトになる）
- PermissionRequest Block output: `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"..."}}}`
- Missing required Codex fields must be treated as fail-closed parse errors
- PermissionRequest の parse error も PermissionRequest 専用 deny schema で返す
- PreToolUse の parse error も PreToolUse 推奨形式（`hookSpecificOutput.permissionDecision="deny"`）で返す。イベント名が判別できない入力は legacy block 形式（全イベント共通で受理される）にフォールバック
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
