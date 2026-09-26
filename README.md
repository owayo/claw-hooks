<p align="center">
  <img src="docs/images/app.png" width="128" alt="claw-hooks">
</p>

<h1 align="center">claw-hooks</h1>

<p align="center">
  Simple TOML hooks for Claude Code, Cursor, Windsurf, Antigravity CLI, Codex CLI, Grok CLI - Command blocking, auto-formatting, stop-time automation
</p>

<!-- standard:badges:start -->
<h3 align="center">Supported Platforms</h3>

<p align="center">
  <img src="https://img.shields.io/badge/Linux-FCC624?logo=linux&amp;logoColor=black" alt="Linux">
  <img src="https://img.shields.io/badge/macOS-000000?logo=apple&amp;logoColor=white" alt="macOS">
  <img src="https://img.shields.io/badge/Windows-0078D6" alt="Windows">
</p>

<p align="center">
  <a href="https://github.com/owayo/claw-hooks/actions/workflows/ci.yml"><img src="https://github.com/owayo/claw-hooks/actions/workflows/ci.yml/badge.svg?branch=main" alt="CI"></a>
  <a href="https://github.com/owayo/claw-hooks/releases/latest"><img src="https://img.shields.io/github/v/release/owayo/claw-hooks" alt="Release"></a>
  <a href="LICENSE"><img src="https://img.shields.io/github/license/owayo/claw-hooks" alt="License"></a>
</p>

<p align="center">
  <a href="README.md">English</a> |
  <a href="README.ja.md">日本語</a>
</p>
<!-- standard:badges:end -->

---

claw-hooks is a single binary that plugs into the hook systems of Claude Code, Cursor, Windsurf, Antigravity CLI, Codex CLI, and Grok CLI. One TOML file decides which shell commands the agent may not run, which formatters and linters run after a file edit, and what runs when the agent stops.

## Features

- **Built with Rust**: Low overhead, lightweight single binary, blazing fast (<10ms startup)
- **Kill Command Blocking**: Blocks `kill`, `pkill`, `killall`, `taskkill`, PowerShell's `Stop-Process` and its `spps` alias, and suggests [safe-kill](https://github.com/owayo/safe-kill)
- **RM Command Blocking**: Blocks `rm`, `rmdir`, `del`, `erase`, `rd`, PowerShell's `Remove-Item` and suggests [safe-rm](https://github.com/owayo/safe-rm)
- **PowerShell Tool Coverage**: The same filters apply to Claude Code's `PowerShell` tool, which is the only shell tool on Windows without Git Bash. Configure the matcher as `Bash|PowerShell`
- **DD Command Blocking**: Optionally blocks `dd` to prevent disk overwrite accidents
- **AST-based Parsing**: [tree-sitter-bash](https://github.com/tree-sitter/tree-sitter-bash) handles wrappers (`sudo`, `timeout`, `command`, `exec`, `pkexec`, `gosu`, `su`, `arch`, `systemd-run`, `script`), subshells, pipes, `eval`, `find -exec`, `bash -c`/`-lc`, command substitution, brace groups, control flow (`if`/`for`/`while`/`case`), basename/extension/case normalization, and shell quote-removal forms. A string fallback parser keeps the same coverage for non-`ast-parser` builds
- **Custom Command Filters**: Define custom filters with regex support
- **Command Hooks**: Pass each call of a chosen program in a shell command (every `gws` call, say) to an external checker before the command runs. The parser that detects dangerous commands finds the calls, so `sudo gws …` and `bash -c 'gws …'` count too, and the checker receives that call's arguments as JSON after quote removal, never the whole command. The checker can block the command, add context for the agent (Claude Code and Codex CLI), or stay silent
- **Extension Hooks**: Execute external tools (formatters, linters) only after file save/edit completes for `Write` / `Edit` / `MultiEdit` / `NotebookEdit`. Commands are keyed by extension (`".rs"`), and the `"*"` key runs its commands on every edited file, including files without an extension (`Makefile`) and dotfiles (`.gitignore`), after the commands for the file's extension. Lint output flows back to Claude Code / Codex CLI via `additionalContext`, and to Windsurf as exit 2 + stderr. Antigravity CLI needs `--event PostToolUse` on its `PostToolUse` entry; the tools then run against `toolCall.args.TargetFile`, but its output is fixed at `{}` so only the formatter's own rewrite reaches the agent. Grok CLI does deliver the edited file path, so the tools run normally, but its post-hook stdout is ignored, so the formatter's own rewrite is the only feedback the agent sees
- **Stop Hooks**: Run commands when agent loop ends (notifications, git commit with [git-sc](https://github.com/owayo/git-smart-commit), cleanup)
- **Project-wide Lint on Stop**: Auto-detect project type (`Cargo.toml`, `tsconfig.json`, etc.) and run lint/typecheck; failures are surfaced back to the agent (Windsurf and Grok CLI are best-effort)
- **Hook Timeout**: Configurable per-hook timeout (default 60s). On Unix the whole process group is SIGKILL'd, so grandchildren of `sh -c '...'` cannot leak past the deadline
- **Output Truncation**: Multi-byte-safe truncation of hook output (default 1000 chars) to protect the agent's context window
- **Output Compression**: Collapses decorative runs (`.`, `=`, `-`, `─`, `━`, `^`, `·`, `→`, `_`), `\r`-overwriting progress bars, repeated cargo `Compiling`/`Blocking` lines, common absolute-path prefixes, rustc/ruff/biome span underlines and frame characters, and Biome's whitespace markers / duplicate diff line-number pairs. Successful no-op formatter/linter notices such as `All checks passed!` and `1 file already formatted` are omitted, while changed-file and failure output is preserved. The no-op test runs on the *normalized* text, so a tool that pairs a success line with per-run config warnings (e.g. `ruff check --select D…`, which writes ruleset-incompatibility warnings to stderr on every run) is still recognised as a no-op instead of returning a bare `All checks passed!` after every edit. Biome's `Checked N file(s) in <duration>. No fixes applied.` counter and its closing `check ━` / `× Some errors were emitted while running checks.` block are dropped when diagnostics accompany them, and kept when they are the whole output. ANSI stripping also covers the general `ESC` + intermediate-byte escape form (terminfo's `sgr0`, e.g. `\E(B\E[m`) and bare `SO`/`SI`, which otherwise leak a stray character onto every colored `cargo fmt --check` diff line and defeat all the rules above
- **Repeated Source Excerpt Removal**: Within a single diagnostic, source-excerpt lines (`3 │ code`, `> 3 │ code`, `12 | code`) that repeat verbatim are dropped after the first occurrence: biome re-prints the same excerpt once per sub-block (the `!` message, the `i` note, the `i Safe fix:` block) and ruff re-prints context inside its fix diff, and those repeats carry no information. Diff lines (`- old` / `+ new`) survive because they *are* the fix. Measured on real output: ruff −6%, biome −14%
- **Cross-Diagnostic Excerpt Removal**: Consecutive diagnostics that point at the same place re-print the *whole* excerpt each time — one function definition draws `ANN201` / `D103` / `ANN001` / `ANN001`, one `let` draws `useConst` / `noUnusedVariables`. When a diagnostic's excerpt is byte-identical to the previous diagnostic's, the whole block is dropped; each diagnostic keeps its own header, so the file, line, and column are never lost, and a diagnostic separated by a different excerpt keeps its own copy. Measured on real output: a further ruff −15%, biome −8%. This directly buys information rather than just tokens, since the default 1000-character cap was otherwise spent re-printing the same code instead of showing the diagnostics that followed
- **Debug Log Safety**: Logs persist only event/tool/session metadata, executable basenames, argument counts, and byte-size summaries. Stop/extension hook arguments and executable directories are stripped, so raw commands, file contents, agent messages, and rendered formatter/linter output never reach disk — full output bodies are available only via `--trace` (stderr, non-persistent)
- **Bounded I/O**: stdin is capped at 4 MiB and oversized or invalid-UTF-8 payloads fail closed instead of OOM-killing the process. Hook subprocess stdout/stderr is also drained without deadlock while retaining at most 4 MiB per stream, so a noisy formatter/linter cannot exhaust memory before agent-facing truncation
- **Fail-Closed Gates**: Command blocking denies on parse errors, unreadable input, or a broken config. A typo in `config.toml` does not switch protection off: a config error returns the agent's own deny response (diagnostic on stderr, plus a `claw-hooks check` hint) rather than exiting `1` with empty stdout, which several agents read as "hook failed, ignore its decision". Only the pre-execution gates fail closed, though: on a stop event a "block" means "keep going", and on the events claw-hooks never inspects a deny would erase a user prompt or replace real tool output while buying no safety, so all of those allow instead. A payload too damaged to identify still blocks
- **Project Config Merge**: Place `.claw-hooks.toml` in your project root to extend global settings per project. Project configs are treated as untrusted input (a repository your agent cloned can contain one), so they may only *strengthen* protection: enabling a guard and adding filters are honored, while disabling a guard, replacing global filters, and declaring stop/extension/command hooks are ignored with a warning
- **Multi-Agent Support**: Works with Claude Code, Cursor, Windsurf, Antigravity CLI, Codex CLI, and Grok CLI

## Why claw-hooks?

Native agent hooks make you ship a Python/Bash script for every dangerous-command check and every formatter. claw-hooks collapses that to TOML.

```toml
# Block dangerous commands
rm_block = true
rm_block_message = "🚫 Use safe-rm instead"

# Auto-format on save
[extension_hooks]
".rs"  = ["rustfmt {file}"]
".py"  = ["ruff format --check {file}", "ruff check --preview --select=I,F,DOC {file}"]
".ts"  = ["biome check {file}"]
".tsx" = ["biome check {file}"]
"*"    = ["noslop hook file {file}"]   # every edited file, after its extension's commands
```

…wired in once via the agent's standard hooks config:

```json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Bash|PowerShell",
      "hooks": [{"type": "command", "command": "claw-hooks hook"}]
    }]
  }
}
```

A naive `grep -E '^rm '` filter misses `sudo rm`, `cd /tmp && rm`, `bash -lc 'rm …'`, pipes, `xargs`, brace groups, process substitution, privilege wrappers (`pkexec` / `gosu` / `su <user> cmd`), and shell quote-removal forms (`r\m`, `$'r\x6d'`). claw-hooks resolves every one of those through tree-sitter-bash (with a string fallback parser of the same coverage) — one binary, no Python/jq dependency, identical behavior across Claude Code / Cursor / Windsurf / Antigravity / Codex / Grok.

<details>
<summary>What the equivalent native Python hook looks like</summary>

```python
#!/usr/bin/env python3
import json, sys

data = json.loads(sys.stdin.read())
if data.get("tool_name") == "Bash":
    cmd = data.get("tool_input", {}).get("command", "")
    if any(s in cmd for s in ("rm ", "rm -", "rmdir")):
        print(json.dumps({
            "hookSpecificOutput": {
                "hookEventName": "PreToolUse",
                "permissionDecision": "deny",
                "permissionDecisionReason": "🚫 Dangerous command blocked",
            }
        }))
        sys.exit(2)
sys.exit(0)
```

Then duplicate it per agent, per dangerous command, per formatter — and re-implement quote/wrapper handling for every one.
</details>

### Comparison

| Feature | Native Hooks | claw-hooks |
|---------|--------------|------------|
| Block dangerous commands | 25+ lines Python per command | 1 line TOML |
| Custom filters | New script per filter | Add to `[[custom_filters]]` |
| Extension hooks (formatters) | Complex file detection script | `[extension_hooks]` map |
| Lint output to agent | Manual JSON construction | Automatic (Claude Code, Codex CLI); Windsurf via exit 2 + stderr*; Antigravity CLI via Stop hooks*; not available on Cursor (`afterFileEdit` has no output schema) or Grok CLI (post-hook stdout is ignored) |
| Multi-agent support | Different scripts per agent | Single binary with `--format` |
| Stop hooks (lint, notifications, etc.) | Custom scripts per use case | `[[stop_hooks]]` config |

\* Lint/formatter output is automatically passed via `additionalContext` where the agent hook runtime supports it, enabling the agent to fix warnings. Windsurf has no equivalent JSON field, so post-edit diagnostics are delivered as exit code 2 with the body on stderr — per the official spec only `pre_*` hooks can block, so this surfaces the diagnostics to the agent without reverting the edit (and to the user as well when `show_output` is `true`).

## Installation

<!-- standard:install:start -->
### Homebrew (macOS/Linux)

```bash
brew install owayo/claw-hooks/claw-hooks
```

### Cargo

Requires Rust 1.98.1 or later.

```bash
cargo install --git https://github.com/owayo/claw-hooks --locked
```

### From GitHub Releases

Download the archive for your platform from [Releases](https://github.com/owayo/claw-hooks/releases/latest), extract it, and put `claw-hooks` on your `PATH`. Each release also includes `SHA256SUMS` for checking the downloads.

| Platform | Archive |
|---|---|
| Linux (x86_64) | `claw-hooks-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (x86_64, musl) | `claw-hooks-x86_64-unknown-linux-musl.tar.gz` |
| Linux (ARM64) | `claw-hooks-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `claw-hooks-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `claw-hooks-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `claw-hooks-x86_64-pc-windows-msvc.zip` |

On macOS, if you downloaded the archive with a browser, remove the quarantine attribute before running it: `xattr -d com.apple.quarantine claw-hooks`.

### From Source

Requires [mise](https://mise.jdx.dev/) (the Rust toolchain is pinned in `mise.toml`).

```bash
git clone https://github.com/owayo/claw-hooks.git
cd claw-hooks
make install
```

`make install` installs to `/usr/local/bin`. Set `INSTALL_PATH` to change it (for example `make install INSTALL_PATH="$HOME/.local/bin"`).
<!-- standard:install:end -->

## Quickstart

```bash
# Generate default configuration (never overwrites an existing config;
# pass --path/--config to write somewhere else)
claw-hooks init

# Test with a safe command (allowed)
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"}}' | claw-hooks hook
# Output: {}

# Test with a dangerous command (blocked)
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}' | claw-hooks hook
# Output: {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"🚫 Use safe-rm instead..."}}
```

> **claw-hooks is deny-only.** An allowed command returns an empty object (`{}`) with exit `0`, which means "no objection" — not "approved". claw-hooks never emits `permissionDecision: "allow"`, because per the official spec that *skips the permission prompt* and would silently auto-approve everything claw-hooks did not block. Your existing permission prompts and rules stay in effect for everything else.

## Usage

`claw-hooks hook` reads one hook event from stdin and writes the agent's native response. `init` writes the default config, `check` validates it, and `version` prints the version.

```bash
# Process Claude Code hooks (default)
claw-hooks hook

# Other agents: pass --format (cursor, windsurf, agy, codex, grok)
claw-hooks hook --format cursor

# Antigravity CLI payloads carry no event name, so pass --event as well
claw-hooks hook --format agy --event PostToolUse

# Use custom config
claw-hooks hook --config /path/to/config.toml
```

Every subcommand and option, how each `--format` reads its agent's payload, the output and exit code for every event, the fail-closed rules, and the command hook protocol: [docs/cli-reference.md](docs/cli-reference.md)

## Agent Integration

Register `claw-hooks hook` in each agent's hooks file. Agents other than Claude Code need `--format`.

| Agent | Hooks file (user / project) | Command |
|---|---|---|
| Claude Code | `~/.claude/settings.json` / `.claude/settings.json` | `claw-hooks hook` |
| Cursor | `~/.cursor/hooks.json` / `<project>/.cursor/hooks.json` | `claw-hooks hook --format cursor` |
| Windsurf (Cascade) | `~/.codeium/windsurf/hooks.json` / `.windsurf/hooks.json` | `claw-hooks hook --format windsurf` |
| Antigravity CLI | `~/.gemini/config/hooks.json` / `<project>/.agents/hooks.json` | `claw-hooks hook --format agy --event <event>` |
| Codex CLI | `~/.codex/hooks.json` | `claw-hooks hook --format codex` |
| Grok CLI | `~/.grok/hooks/` / `<project>/.grok/hooks/` | `claw-hooks hook --format grok` |

The hooks JSON for each agent, the events and matchers to register, and what each agent can receive back (for example, whether lint output reaches the agent): [docs/integrations.md](docs/integrations.md)

## Configuration

claw-hooks reads `~/.config/claw-hooks/config.toml` on every platform. `claw-hooks init` writes a default config there (it never overwrites an existing one), and `claw-hooks check` validates it.

```toml
# Block dangerous commands and point the agent at safer tools
rm_block = true
kill_block = true
rm_block_message = "🚫 Use safe-rm instead: safe-rm <file>"

# Block a command only with specific arguments (command is a regex)
[[custom_filters]]
command = "npm"
args = ["install", "i", "add"]
message = "Use `pnpm` instead of `npm`"

# Pass every gws call to an external checker before the command runs
[[command_hooks]]
command = "gws"
run = "noslop hook command"

# Run formatters and linters after a file is written or edited
# ("*" covers every edited file and runs after the commands for the file's extension)
[extension_hooks]
".rs" = ["rustfmt {file}"]
".ts" = ["biome check {file}"]
"*" = ["noslop hook file {file}"]

# Lint the whole project when the agent stops (only where Cargo.toml exists)
[[stop_hooks]]
commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
condition = { file_exists = "Cargo.toml" }
```

A `.claw-hooks.toml` in the working directory is merged into the global config, but only toward more protection: enabling a guard and adding filters take effect, while disabling a guard and declaring stop, extension, or command hooks are ignored with a warning. `--config <path>` uses that file instead of the global config.

Every setting and its default, the project merge rules, staged and session-scoped stop hooks, the environment passed to stop hooks, the custom filter modes, and command hooks: [docs/configuration.md](docs/configuration.md)

## Performance

| Metric | Value |
|--------|-------|
| Startup time | <10ms |

## Development

<!-- standard:dev:start -->
Requires [mise](https://mise.jdx.dev/). Tool versions are pinned in `mise.toml`.

```bash
make setup   # Install the toolchain (mise) and dependencies
make ci      # Run the same checks as CI (no changes)
```

| Command | Description |
|---|---|
| `make setup` | Install the toolchain (mise) and dependencies |
| `make build` | Build a debug binary |
| `make release` | Build a release binary |
| `make run` | Run the debug binary (arguments via ARGS="...") |
| `make test` | Run the tests |
| `make lint` | Run clippy with warnings as errors |
| `make fmt` | Format the code (rewrites files) |
| `make fmt-check` | Check the formatting (no changes) |
| `make check` | Run fmt-check and lint (no changes) |
| `make ci` | Run the same checks as CI (no changes) |
| `make install` | Install the release binary to INSTALL_PATH (default /usr/local/bin) |
| `make uninstall` | Remove the binary from INSTALL_PATH |
| `make clean` | Remove build artifacts |

Run `make` to list every target. Releases are published from GitHub Actions (**Actions → Release → Run workflow**).
<!-- standard:dev:end -->

`make test` and `make lint` run in two configurations, with all features (the tree-sitter AST parser) and with `--no-default-features` (the string fallback parser), because the fallback build has a parser of its own.

## License

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
