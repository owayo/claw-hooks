# Agent Integration

Register `claw-hooks hook` in each agent's hooks file. This page lists, per agent, where the file lives, which events to register, and what the agent can and cannot receive back. How claw-hooks reads each agent's payload and what it answers is in the [CLI reference](cli-reference.md).

## Claude Code

Add to `~/.claude/settings.json` (user) or `.claude/settings.json` (project):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash|PowerShell",
        "hooks": [{ "type": "command", "command": "claw-hooks hook" }]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Write|Edit|MultiEdit|NotebookEdit",
        "hooks": [{ "type": "command", "command": "claw-hooks hook" }]
      }
    ],
    "Stop": [
      {
        "matcher": "",
        "hooks": [{ "type": "command", "command": "claw-hooks hook" }]
      }
    ]
  }
}
```

## Cursor

Add to `<project>/.cursor/hooks.json` (project) or `~/.cursor/hooks.json` (user):

```json
{
  "version": 1,
  "hooks": {
    "preToolUse": [
      {
        "command": "claw-hooks hook --format cursor",
        "matcher": "Shell|Bash|Terminal|Exec|Run|Command",
        "failClosed": true
      }
    ],
    "beforeShellExecution": [
      { "command": "claw-hooks hook --format cursor", "failClosed": true }
    ],
    "afterFileEdit": [
      { "command": "claw-hooks hook --format cursor" }
    ],
    "stop": [
      { "command": "claw-hooks hook --format cursor" }
    ]
  }
}
```

> **`failClosed: true` on the command-blocking hooks is recommended.** Cursor is fail-open by default: a clean block (exit `0` plus `{"permission":"deny", …}` on stdout) works without it, but if claw-hooks itself crashes or times out, Cursor lets the command through unless `failClosed: true` is set. Leave it off for `afterFileEdit`/`stop` (a formatter/lint crash should not block the agent).

> **Keep the matcher on `preToolUse`.** That hook fires for *every* tool, so without a matcher a large `Write` also reaches claw-hooks; once the payload exceeds the 4 MiB stdin limit it can no longer be parsed, and `preToolUse` is a pre-execution gate, so the fail-closed path denies a file write that claw-hooks has no opinion about. Cursor matchers are regular expressions, so the pattern above stays deliberately broad — a false positive is harmless (claw-hooks still passes through anything without `tool_input.command`), and a false negative is covered by `beforeShellExecution`, which is shell-specific by event rather than by tool name.

> **Prefer project hooks when you use stop-time lint.** Cursor runs project hooks (`<project>/.cursor/hooks.json`) from the project root, but user hooks (`~/.cursor/hooks.json`) from `~/.cursor/`. claw-hooks resolves `condition = { file_exists = "Cargo.toml" }`, the `.claw-hooks.toml` lookup, and each hook's own working directory from that directory, so a user-level registration makes every project-type condition fail silently — and a hook without a condition (a `git-sc` auto-commit, say) runs in your Cursor config directory instead of the repository.

> **Post-edit diagnostics can't be returned to Cursor.** `afterFileEdit` has no documented output schema, so formatters still rewrite files but linter text has nowhere to go. Run project-wide lint as a `stop` hook when you need the diagnostics — those come back through `followup_message`.

## Windsurf (Cascade)

Add to `~/.codeium/windsurf/hooks.json` (user) or `.windsurf/hooks.json` (project):

```json
{
  "hooks": {
    "pre_run_command": [
      { "command": "claw-hooks hook --format windsurf", "show_output": true }
    ],
    "post_write_code": [
      { "command": "claw-hooks hook --format windsurf", "show_output": true }
    ],
    "post_cascade_response": [
      { "command": "claw-hooks hook --format windsurf", "show_output": true }
    ]
  }
}
```

## Antigravity CLI

Add to `~/.gemini/config/hooks.json` (user) or `<project>/.agents/hooks.json` (project workspace):

```json
{
  "claw-hooks": {
    "PreToolUse": [
      {
        "matcher": "run_command|manage_task",
        "hooks": [{ "type": "command", "command": "claw-hooks hook --format agy --event PreToolUse" }]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "write_to_file|replace_file_content|multi_replace_file_content",
        "hooks": [{ "type": "command", "command": "claw-hooks hook --format agy --event PostToolUse" }]
      }
    ],
    "Stop": [
      { "type": "command", "command": "claw-hooks hook --format agy --event Stop" }
    ]
  }
}
```

Notes:
- **The matcher covers `manage_task` as well as `run_command`.** With `Action: "send_input"`, `manage_task` writes its `Input` to a running process's stdin. Start a persistent shell with `run_command` + `RunPersistent: true` and every later command arrives through `send_input` without ever passing `CommandLine`, so leaving `manage_task` unmatched lets the rm/kill/dd filters be bypassed entirely. The other actions (`list` / `status` / `kill`) manage the agent's own background tasks — unrelated to the shell `kill` command — and pass through.
- **Pass `--event` for Antigravity.** Antigravity payloads carry no event-name field, and `PreToolUse` and `PostToolUse` are indistinguishable by shape — both send `toolCall` plus `stepIdx`, differing only in an optional `error`. Since `hooks.json` registers each event separately, `--event` tells claw-hooks which one it is. Without it, claw-hooks infers the event and resolves the ambiguous case to `PreToolUse`, which keeps command blocking intact but leaves post-edit hooks inactive.
- Extension hooks work on Antigravity when `--event PostToolUse` is set: the edited path is read from `toolCall.args.TargetFile`. The official `PostToolUse` output is fixed at `{}`, so formatters and linters **run** but their diagnostics cannot be returned to the agent. To surface diagnostics, run project-wide lint/typecheck as Stop hooks — those failures are injected back via `{"decision":"continue","reason":"..."}`.
- Antigravity has no `stop_hook_active` / `loop_count` equivalent (`executionNum` is just an attempt counter and is `1` on a normal first stop), so claw-hooks cannot break a loop caused by a stop hook that fails forever. Give reported stop hooks a self-limiting exit condition.
- `PreInvocation` / `PostInvocation` are out of claw-hooks' scope and pass through automatically; no hook entry is needed for those events.
- Official Antigravity hooks docs: <https://antigravity.google/docs/customizations/hooks>

## Codex CLI

Add to `~/.codex/hooks.json` (user):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "claw-hooks hook --format codex"
          }
        ]
      }
    ],
    "PermissionRequest": [
      {
        "matcher": "Bash",
        "hooks": [
          {
            "type": "command",
            "command": "claw-hooks hook --format codex"
          }
        ]
      }
    ],
    "PostToolUse": [
      {
        "matcher": "Bash|apply_patch|Edit|Write",
        "hooks": [
          {
            "type": "command",
            "command": "claw-hooks hook --format codex"
          }
        ]
      }
    ],
    "Stop": [
      {
        "hooks": [
          {
            "type": "command",
            "command": "claw-hooks hook --format codex"
          }
        ]
      }
    ]
  }
}
```

Codex hooks are enabled by default. If you explicitly configure feature flags, use the current `[features] hooks` key; the older `codex_hooks` alias is deprecated.

## Grok CLI

Add a JSON file under `~/.grok/hooks/` (personal) or `<project>/.grok/hooks/` (project):

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "hooks": [{ "type": "command", "command": "claw-hooks hook --format grok", "timeout": 10 }]
      }
    ],
    "PostToolUse": [
      {
        "hooks": [{ "type": "command", "command": "claw-hooks hook --format grok", "timeout": 10 }]
      }
    ],
    "Stop": [
      {
        "hooks": [{ "type": "command", "command": "claw-hooks hook --format grok", "timeout": 10 }]
      }
    ]
  }
}
```

Notes:
- `matcher` is a regular expression tested against the tool name; omit it to match every tool. Grok maps Claude-style names such as `Bash` and `Edit` onto its own tool names, but the mapped names are not published, so omitting `matcher` is the safer choice — claw-hooks decides what to do from the payload itself and passes everything irrelevant through (see [Format Detection Logic](cli-reference.md#format-detection-logic)).
- `timeout` is in **seconds** and defaults to `5`, which is short for formatters and project-wide lint. Raise it as shown above.
- Project hooks only run after the repository is trusted: run `/hooks-trust` once, or start Grok with `--trust`.
- Grok also loads Claude Code (`.claude/settings.json`) and Cursor (`.cursor/hooks.json`) hook files. If claw-hooks is already registered in one of those, keep a single registration so it does not run twice per event.
- claw-hooks dispatches on the shape of `toolInput`, never on `toolName`: a `command` field means a shell command, a `file_path` / `filePath` / `notebook_path` / `notebookPath` field means a file edit, and anything else passes through. `toolName` and `toolInput` are both optional for the same reason — they are not what the decision is made from, and requiring them would deny unrelated tool calls (tools without arguments omit `toolInput` entirely).
- `PreToolUse` is Grok's only blocking event. Every other event is a post-hook whose stdout is ignored, so extension hooks still reformat files and Stop hooks still run lint, but their output cannot be reported back to the agent — the same limitation as Windsurf's `post_cascade_response`.
- Grok is fail-open for anything that is not an explicit deny: a timeout, a crash, or malformed output is recorded as a hook failure and the tool call proceeds. claw-hooks therefore emits the deny JSON **and** exit code `2` when it blocks, and uses exit `2` (never `1`) on its fail-closed paths, so the block holds under either reading of the contract.
