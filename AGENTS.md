# AGENTS.md - AI Agent Instructions

Instructions for AI coding agents (Claude Code, Cursor, Windsurf, Codex, GitHub Copilot, etc.)

## Project Overview

**claw-hooks** - Hooks CLI for AI coding agents with TOML-based configuration.

- **Language**: Rust (MSRV 1.85)
- **Version**: 26.7.100
- **Purpose**: Block dangerous commands, run formatters/linters only after file save/edit completes, send notifications on agent stop/subagent events
- **Supported Agents**: Claude Code, Cursor, Windsurf, Antigravity CLI, Codex CLI

## Key Features

1. **Command Blocking**: `rm`/`kill`/`dd` → suggest `safe-rm`/`safe-kill`
2. **AST Parsing**: tree-sitter-bash for accurate command detection (optional feature `ast-parser`), with command basename/executable-extension/case normalization (`/bin/rm`, `RM.EXE`), shell quote-removal handling (`r\m`, `r''m`, `$'r\x6d'`), and fallback parsing that also handles subshells/command substitutions/env-prefix commands, single `&` (background), newline separators, `eval`, `find -exec`/`-execdir`, `xargs -I`/`xargs sh -c`, Windows `cmd /c`, and wrapper options (e.g., `sudo -n`, `/usr/bin/sudo -u root rm`, `sudo --user root`, `sudo VAR=value rm`, `timeout --signal TERM 10 rm`, `command rm`, `exec rm`), value-taking flags followed by numeric arguments (`xargs -n 1 rm`, `sudo -u 1000 rm`, `nice -n 10 rm` — the numeric token is preserved so the flag does not swallow the real command), `bash -c --` option terminators, shell `-c` clustered with inline script (`bash -c'rm -rf /'`, `bash -lc'rm -rf /'` — shells join `-c` and the immediately-quoted script into a single argv token `-crm -rf /`; the parser splits the script out of the cluster so the inner command is still detected and cannot bypass the filter), and `env -S` / `--split-string` command strings (env-specific, so `sort -S` etc. are not misparsed), command-position brace expansion (`{rm,-rf,/p}`, `/bin/{rm,ls}`, `{r,}m` → first-choice expansion so the launched command is still detected), brace command groups (`{ rm -rf /; }` — the leading `{` group delimiter is skipped so the inner command is still detected, including in the fallback parser), shell control structures in the fallback parser (`if … then rm …`, `for/select … do rm …`, `while/until … do rm …`, `case W in P) rm …`, `! rm …` — the control keywords, loop variables, and `case` patterns are skipped so the inner command is still detected; loop variables and patterns named like a blocked command, e.g. `for rm in …` or `case x in rm) …`, are NOT misdetected as that command; command substitutions inside loop/case headers such as `for f in $(rm …)` are also extracted, preventing the fallback parser from failing open on control-flow-wrapped dangerous commands), process substitution (`diff <(rm ...) <(ls)`, `tee >(rm ...)` — the inner command actually runs, so it is extracted; bare redirections `< file` / `> file` are not misdetected), and additional execution-delegation wrappers (`setsid`, `stdbuf`, `taskset` mask, `nsenter`, `unshare`, `setpriv`, `chroot`/`flock` with their leading directory/lockfile positional, `busybox`/`toybox` applets, `su`/`runuser`/`flock -c` shell-string re-evaluation, `pkexec` (Polkit), `gosu` (container-friendly setuid wrapper that takes a leading `<user[:group]>` positional and runs the rest as that user), and Busybox/Alpine-style `su <user> cmd` (`su` takes a leading positional username followed by the command, distinct from `su -c "cmd"` which is handled via the shell-`-c` path; the parser also recognises GNU/util-linux `su` value-taking flags such as `-c`/`-g`/`-G`/`-s` and `--shell`/`--command`/`--group`/`--supp-group`/`--session-command` so `su -s /bin/bash root rm` does not consume `/bin/bash` as the user positional and misdetect the real command) — all expanded so `pkexec rm -rf /` / `gosu root rm -rf /` / `su root rm -rf /` cannot bypass the filter under elevated privileges). For wrappers that take a leading positional (`su`/`gosu`/`chroot`/`flock`), the `--` option terminator still leaves that positional in place, so the parser consumes the remaining leading positionals after `--` before locating the executed command (`su -- root rm -rf /` / `gosu -- root rm -rf /` resolve to `rm`, not the username `root`); a bare `sudo -- rm` keeps returning the token after `--` since `sudo` has no leading positional. Wrapper chains (`sudo sudo … rm`) are expanded iteratively rather than recursively so even a very long chain (within the size limit) cannot overflow the stack and fail open. When a wrapper's resolved command is itself a re-evaluating command (`eval`, `xargs`, `find -exec`/`-execdir`, `env -S`, or a shell `-c`), the inner command string is extracted and re-parsed through a shared helper used by both the top-level and wrapper-expansion paths, so `sudo eval 'rm -rf /'` / `sudo xargs rm` / `sudo find . -exec rm {} \;` / `timeout 10 env -S 'rm -rf /'` are detected rather than failing open (previously the wrapper-expansion path re-evaluated only the shell `-c` form and let the others through); the extraction is applied only to genuine re-evaluating commands via their argument helpers (not by blindly re-joining the tail), so a quoted argument to a non-re-evaluating command — e.g. `sudo echo '; rm -rf /'` or `sudo find . -name rm` — stays an argument and is not over-blocked. The custom-filter command-string extractor uses the same top-level terminator splitter (`;`, newline, single `&`) as the AST/fallback dangerous-command path, so anchored regex filters (`^rm `) cannot be evaded by chaining the target command after a newline or `&` (e.g., `echo ok\nrm -rf /tmp`, `echo ok & rm -rf /tmp`).
3. **Custom Filters**: Regex-based and argument-based command filtering
4. **Extension Hooks**: Auto-format/lint on AfterFileEdit events only (not on BeforeCommand) with timeout support (`{file}` must appear exactly once, parent-directory traversal, shell redirection paths, tabs/newlines/NUL are blocked; on Windows, `cmd /c` shell metacharacters such as `%`, `!`, `^`, `"` are also rejected from file paths to prevent variable-expansion injection). Successful no-op formatter/linter notices are suppressed, while rewritten-file/failure output is preserved; agent-facing command labels contain only the configured program name.
5. **Stop Hooks**: Run commands on agent stop (lint/typecheck, notifications, git commit, cleanup). `report=true` hooks are waited on and failures are returned to the agent; `report=false` hooks are started detached with stdin/stdout/stderr discarded, and a background reaper thread `wait`s on each detached `Child` so the kernel does not accumulate zombie entries when Stop fires repeatedly. Loop prevention is three-layered: a cross-process env flag, the agent's `stop_hook_active` flag (Claude/Codex), and Cursor's `loop_count` (>= 1 means a follow-up was already fired, so all stop hooks are skipped — symmetric with `stop_hook_active`). Session-kind filtering: Claude Code team features spawn delegated agents (teammates) as separate `claude` processes whose own `Stop` events carry both non-empty `agent_id` and `agent_type` fields. A main session launched with `--agent` can carry `agent_type` alone, so `agent_type` by itself must remain primary. Each `[[stop_hooks]]` entry has `session_scope = "primary" (default) | "delegated" | "all"`, and the default `primary` runs hooks (and the NanoBuddy stop notification) only for main-session Stops, so teammate fleets do not cause notification spam, redundant lints, or racing parallel git auto-commits. Missing/empty/blank/non-string discriminator fields are treated as primary (misclassifying a delegated session as primary only runs extra hooks, while the reverse would silently drop the final lint/commit, so the parse errs toward primary); in-process subagents (Task tool) fire only `SubagentStop`, not `Stop`, and are unaffected.
6. **Subagent Events**: NanoBuddy notifications on subagent start/stop
7. **Output Normalization**: ANSI stripping (CSI/OSC/SS2/SS3), carriage-return progress collapsing (a logical line rewritten in place via a lone `\r` — common in Godot headless import and download/migration progress bars — is reduced to the final post-`\r` segment that a terminal would actually display; Rust's `str::lines()` only splits on `\n`/`\r\n`, so without this the line would otherwise retain every intermediate progress state plus the literal `\r` control bytes, and `\r\n` Windows line endings are unaffected because they are already split into separate lines), path prefix removal including absolute paths under directories with spaces (the prefix is only stripped when both the preceding character and the following character match the path-boundary conditions used by `extract_absolute_paths` — line start / whitespace / `-` / `>` / `=` before, and a filename character after — so that a common prefix appearing incidentally inside non-path text such as `foo/proj/src/...` is not silently mangled), repeated decorative character compression (`.`, `=`, `-`, `─`, `━`, `^`, `·`, `→`, `_`) — the `_` rule collapses rustc/ruff/biome multiline span underlines (`| |_______^` → `| |_^`) while snake_case separators (1–2 underscores) are preserved — plus trailing progress ellipsis compression and rustc-style location marker compression (`-->` / `---->` → `->`, single-hyphen `->` preserved so function return types stay intact), space-separated decorative run compression for biome diff visualization (`→ → → → → → → Google` → `→ Google`, applies to `·` and `→` with single-space delimiter), biome duplicate diff context line-number compression (`129 129 │ text` → `129 │ text`, empty context lines `129 129 │` → `129 │` when both line numbers are identical integers; differing pairs like `10 9 │` are preserved as informative), diagnostic frame-line removal (lines composed solely of `|` / `^` / `_` / Box Drawing characters (U+2500–U+257F: `│`, `─`, `╭─╮` / `╰─╯` banner frames from pnpm-style update notices, biome `─────` separators) and whitespace — ruff/biome/rustc caret and separator scaffolding — are dropped entirely, since the pointed-at column already appears numerically in the preceding `file:line:col` header; lines with any alphanumeric content such as headings, source lines, labelled carets `| ^ expected u32`, banner body lines `│ Update available! │`, or snake_case-containing lines like `__init__` are preserved — `_` is in the dropped set so rustc/clippy multiline span underlines collapsed to `| |_^` are removed, but any letter/digit keeps the line), progress-prefix line collapsing limited to a whitelist of progress words (`Compiling`, `Checking`, `Building`, `Blocking` — cargo's repeated `Blocking waiting for file lock on ...` lines — etc.) where a run of consecutive lines each starting with any whitelisted progress word is collapsed as one run even when the words differ (cargo's concurrent build interleaves `Compiling`/`Checking`, so a same-word-only rule would leave most progress noise uncollapsed) — single-word runs summarize as `... (and N more lines starting with "<word>")`, mixed-word runs as `... (and N more progress lines)`, while diagnostic lines (`error:` / `warning:`) break the run and are never collapsed so their per-line information is preserved, lint ruleset-incompatibility configuration warnings (ruff's `warning: … are incompatible. Ignoring …` lines emitted on every run — about the D-rule set rather than the edited code) are dropped as pure per-edit noise while genuine code diagnostics such as `warning: unused variable` are preserved, successful no-op formatter/linter notices (`All checks passed!`, `N file(s) already formatted/left unchanged`, Biome `No fixes applied`) are removed only when the command succeeds, and character-count-based output length truncation for token efficiency
8. **Fail-Closed Security**: Block on parse errors, empty input, or missing/blank/incorrectly typed required agent fields. Non-empty unknown/unsupported event names are still passed through as allow to keep claw-hooks within its intentional scope. Pathologically deep command nesting or oversized command input is also treated as a block candidate, avoiding recursion-based stack overflow that would otherwise crash and fail open. The pathological-input guard counts brace nesting (`{`/`}`) in addition to parentheses, and the AST walk enforces an explicit traversal-depth limit, so deeply nested command groups (`{ ...;}`) cannot overflow the stack and fail open even under small thread stacks (Windows/containers). Wrapper-chain expansion (`sudo sudo … rm`) is iterative with an explicit recursion bound rather than self-recursive, so a long wrapper chain that slips under the size limit cannot overflow the stack; chains exceeding the bound fail closed. The stdin reader is bounded (4 MiB) so an oversized or runaway hook payload is treated as fail-closed rather than allocating until the process is OOM-killed (which would otherwise drop the safeguard entirely). Stdin is read as raw bytes and decoded with lossy UTF-8 conversion (not `read_to_string`), so a stray invalid-UTF-8 byte cannot abort the process with `exit 1` and an empty stdout — a state Codex/Antigravity interpret as a failed hook (decision ignored = fail-open). Invalid bytes become U+FFFD and the input still flows through the normal parse/detect path: a now-unparseable payload fails closed (block), while a still-parseable command keeps its dangerous-command detection. Hook subprocess stdout/stderr pipes are drained to EOF but retain at most 4 MiB per stream, preventing a noisy formatter/linter from OOM-killing the hook before output normalization and agent-facing truncation.
9. **Process Group Isolation (Unix)**: Hook subprocesses are placed in their own process group via `setpgid`, so timeouts kill the whole tree (`killpg`) — preventing grandchild leaks like `sh -c 'sleep ...'` after the shell is signaled. If the direct child exits but a background grandchild still keeps stdout/stderr pipes open (for example `sh -c 'sleep 60 &'`), the pipe wait is also treated as a timeout and the process group is killed. After a normal child exit, the reader-thread join uses a short post-exit drain grace rather than reusing the original timeout deadline, so a command that completes right at the deadline still has its buffered output collected instead of being mis-reported as a timeout (which would otherwise return a spurious block to the agent for `report=true` stop hooks); a grandchild that keeps the pipe open past the grace still triggers the process-group kill.
10. **Debug Log Safety**: Debug logs store hook event summaries (event/tool/session/size) instead of raw hook input or raw parsed `ToolInput`, so tool command text, file contents, unknown event payloads, and agent messages are not persisted to disk. Per-agent parsed-input debug lines log byte counts and booleans (`command_bytes`, `file_path_bytes`, `has_cwd`) rather than raw command/file-path/cwd values, and the formatted hook output is logged as a decision + size summary; the full output body (which may include lint diagnostics quoting source lines) is available only via `--trace`, which writes to stderr and is not persisted. Extension-hook logs record the program name with arg-count / inline-template booleans and the resolved file path byte length instead of the expanded command line — the actual file path and rendered command (which can contain user-directory hierarchies or source-quoting lint snippets) are kept off disk. Extension-hook failure logs and timeout messages also use the same sanitized command label, so failed formatter/linter output sizes can be audited without persisting the edited file path. Stop-hook output logging records only the byte size of stdout/stderr per hook; the full diagnostic body is still relayed to the agent via the block `reason`/`followup_message`, but it is never persisted to disk.

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
│   ├── adapter.rs       # Agent format conversion (Claude/Cursor/Windsurf/Antigravity/Codex)
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

# MSRV
make msrv

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
6. **Intentional Scope Limits**: This tool is for command blocking, post-edit hooks, stop hooks, and subagent notifications. It must not expand into general lifecycle/prompt orchestration. `SessionStart`, `UserPromptSubmit`, and other unsupported lifecycle events are intentionally out of scope; adapters map them to the internal `HookEvent::Passthrough` marker, which must remain allow-by-design unless the project direction explicitly changes.

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
- Stop 入力の空白でない `agent_id` と `agent_type` が両方ある場合だけ teammate（別プロセスの委譲エージェントセッション）と判定する。`--agent` で起動したメインセッションにも `agent_type` は入るが、サブエージェント固有の `agent_id` は入らない。claw-hooks はこの組で `StopSessionKind::Primary / Delegated` を判別し、`session_scope` によるフック選別と NanoBuddy 通知の抑止を行う。どちらかの欠落・空白・非文字列は Primary 扱い（フェイルセーフ方向）。インプロセスのサブエージェント（Task tool）は Stop を発火せず `SubagentStop` のみ
- 通常の許可/ブロック判定は stdout JSON + exit code 0 で返す（Claude は exit 0 のときだけ stdout JSON を解析する）
- Fail-closed errors: exit code 2 + stderr にメッセージ本文をプレーンテキストで出力（Claude は exit 2 のとき stdout/JSON を解析せず stderr 本文をエラーメッセージとして扱うため、`{"decision":...}` のような JSON ではなく本文のみを出す。exit 2 自体がブロックを意味するためフェイルクローズドは維持される）
- 未対応イベント（`StopFailure`, `PermissionRequest`, `PreCompact` 等）は allow でパススルー（Cursor / Codex と同じ挙動）

### Cursor
- Refer to README.md for integration examples
- Use `--format cursor` when testing
- Input parsing uses `hook_event_name` field for event identification (not field-structure matching)
- Supported events: `preToolUse` for `Shell`/`Bash`, `beforeShellExecution`, `afterFileEdit`, `afterTabFileEdit`, `stop`, `subagentStart`, `subagentStop`
- Unsupported events (e.g., `afterShellExecution`, non-shell `preToolUse`, `postToolUse`) are passed through as allow
- `stop` / `subagentStop` の Output スキーマは公式に `{ followup_message?: string }` のみ（`permission` フィールドは存在しない）。Allow は `{}`、Block は `followup_message` に修正指示を入れて返す
- `stop` / `subagentStop` のパースエラーも `followup_message` でフェイルクローズドにする（汎用の `permission: deny` はこのスキーマでは無効）
- `stop` の `loop_count`（stop hook 由来の自動フォローアップ発火回数、0 始まり）が 1 以上の場合は全 stop hook をスキップする。Cursor は `stop_hook_active` を持たないため、`loop_count` が無限フォローアップループ防止の役割を担う（Cursor 側の `loop_limit`(デフォルト5) に頼らず 1 回で止める）

### Windsurf
- Refer to README.md for integration examples
- Use `--format windsurf` when testing
- BeforeCommand (pre_run_command) Block: exit code 2 + stderr にメッセージ本文をプレーンテキストで出力（Windsurf は stdout/stderr を JSON 解析せず stderr を表示用テキストとして扱うため、`{"decision":...}` のような JSON ではなく本文のみを出す）。本文は他エージェントと同様に `normalize_lint_output` で ANSI/空白を正規化してから返す（ANSI エスケープが残ると Windsurf UI の表示が壊れるため）
- Stop は `post_cascade_response` に対応する事後フックのためベストエフォート。stop hook が失敗しても `{}` を返し、エージェントにはブロックを返さない
- 未対応の `agent_action_name` は allow でパススルーする（全イベント対応が目的ではないため）
- フェイルクローズ（パースエラー/空入力）: exit code 2 + stderr にメッセージ本文をプレーンテキストで出力

### Antigravity CLI
- 新規セットアップでは `--format agy` を使用する。設定は `hooks.json` (`.agents/hooks.json` または `~/.gemini/config/hooks.json` — Antigravity 公式が再利用する設定パス)
- Supports these hook events: `PreToolUse`, `PostToolUse`, `PreInvocation`, `PostInvocation`, `Stop`
- 公式入力スキーマは camelCase でイベント名フィールドを持たない。`toolCall`、Stop 固有フィールド、`stepIdx` + `error`、invocation 固有フィールドからイベントを判別する。後方互換の `hook_event_name` / `event` は空白でない文字列だけ受理する。PreToolUse は `stepIdx`、Stop は `executionNum` / `terminationReason` / `fullyIdle` が必須で、`conversationId`, `workspacePaths`, `transcriptPath`, `artifactDirectoryPath` 等も使用
- Use `--format agy` when testing
- PreToolUse Allow: `{"decision":"allow"}`
- PreToolUse Block: `{"decision":"deny","reason":"..."}`（仕様上は `allow` / `deny` / `ask` / `force_ask` を取れるが、claw-hooks は二択で運用）
- PostToolUse output: `{}` 固定（公式仕様。事後フックでありブロック・追加コンテキスト伝達不可）
- PreInvocation / PostInvocation: claw-hooks のスコープ外（モデル呼び出し前後のオーケストレーション）なので allow でパススルー（`{}`）
- Stop output: Allow = `{}`, Block = `{"decision":"continue","reason":"..."}`（`continue` はエージェントを再投入し reason を system message として注入する）
- 判定はすべて stdout JSON + exit code 0 で返す。フェイルクローズドのパースエラーは PreToolUse では `{"decision":"deny","reason":"..."}`、Stop では `{"decision":"continue","reason":"..."}` を返す
- PostToolUse には `toolCall` が含まれないため、拡張子フック（保存後の auto-format / lint）は Antigravity では成立しない。代替として Stop hooks で lint/typecheck を回し、Block を `"decision":"continue"` で再投入する運用に倒している
- run_command（args.CommandLine）以外のツール（`write_to_file` / `replace_file_content` / `multi_replace_file_content` / `view_file` / `list_dir` / `find_by_name` / `grep_search` / `invoke_subagent` / ...）は claw-hooks のコマンドブロックの対象外として PreToolUse でパススルー（`{"decision":"allow"}`）
- 未対応イベント（`SomeNewEvent` 等）は allow でパススルー（他エージェントと同じ）
- Official docs: https://antigravity.google/docs/customizations/hooks

### Codex CLI
- Supports these hook events: `SessionStart`, `UserPromptSubmit`, `PreToolUse`, `PermissionRequest`, `PostToolUse`, `PreCompact`, `PostCompact`, `SubagentStart`, `SubagentStop`, `Stop`
- Use `--format codex` when testing
- Allow output: `{}` (empty JSON, exit 0)
- PreToolUse Block output: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"..."}}`（公式ドキュメントの主形式。legacy の `{"decision":"block"}` も受理されるが使用しない）
- PostToolUse / Stop Block output: `{"decision":"block","reason":"..."}`（これらのイベントではこれが正式形式。Stop では reason が継続プロンプトになる）
- PermissionRequest Block output: `{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"deny","message":"..."}}}`
- Codex の全イベントで共通必須フィールド（非空文字列の `session_id` / `cwd` / `model`、文字列または `null` の `transcript_path`）を検証し、イベント固有の `turn_id` / `permission_mode` / ツール情報 / Stop 状態等も型まで検証する。欠落・型不正はフェイルクローズドのパースエラーとして扱う
- PermissionRequest の parse error も PermissionRequest 専用 deny schema で返す
- PreToolUse の parse error も PreToolUse 推奨形式（`hookSpecificOutput.permissionDecision="deny"`）で返す。イベント名が判別できない入力は legacy block 形式（全イベント共通で受理される）にフォールバック
- PostToolUse の追加コンテキストは `hookSpecificOutput.additionalContext` で返す
- Codex `SessionStart` / `UserPromptSubmit` / `PreCompact` / `PostCompact` は claw-hooks のスコープ外として allow パススルー
- Codex `PreToolUse` / `PermissionRequest` / `PostToolUse` は `Bash` と `apply_patch` を受け取る。`apply_patch` は `MultiEdit` にマップし、patch コマンドから変更ファイルパスを抽出する
- Codex `SubagentStart` / `SubagentStop` は内部通知イベントとして扱う。NanoBuddy 用に `ToolInput::Subagent` へマップするが、README.md / README.ja.md には記載しない
- Official docs: https://developers.openai.com/codex/hooks

## README Update Rules

- `SubagentStart` and `SubagentStop` events are internal features and MUST NOT be documented in README.md or README.ja.md
- The `init` command's default config should not reference subagent events
- When updating READMEs, keep the supported hook events list as: `PreToolUse`, `PostToolUse`, `Stop`, `Notification`, `UserPromptSubmit`, `SessionStart`, `SessionEnd`

## Configuration

Default: `~/.config/claw-hooks/config.toml`

See README.md for full configuration reference.
