# CLI Reference

claw-hooks reads one hook event from stdin and answers in the calling agent's native format. This page covers the subcommands and options, how each `--format` reads its agent's payload, the output for every event, the exit codes, the fail-closed rules, and the protocol between claw-hooks and a command hook's checker. Registering claw-hooks in each agent is covered in [Agent Integration](integrations.md).

## Commands

| Command | Description |
|---------|-------------|
| `hook` (alias: `run`) | Process hook events from stdin |
| `init` | Generate default configuration |
| `check` | Validate configuration |
| `version` | Show version |

## Options

| Option | Short | Description |
|--------|-------|-------------|
| `--format` | `-f` | Input format: `claude` (default), `cursor`, `windsurf`, `agy` (Antigravity CLI), `codex`, `grok` (Grok CLI) |
| `--event` | `-e` | Hook event name (e.g. `PostToolUse`). For Antigravity CLI, whose payloads carry no event-name field and whose `PreToolUse` / `PostToolUse` are shape-identical. Omit for other agents |
| `--config` | `-c` | Path to configuration file |
| `--trace` | `-t` | Trace mode: write the raw input, parsed input, and output to stderr (not persisted to disk) |
| `--path` | `-p` | `init` only: where to write the new config file (default `~/.config/claw-hooks/config.toml`) |
| `--debug` | — | Enable debug logging for this run (same as `debug = true` in the config) |
| `--quiet` | `-q` | Suppress the status messages of `init` and `check` |
| `--help` | `-h` | Show help |
| `--version` | `-V` | Show version |

## Examples

```bash
# Process Claude Code hooks (default)
claw-hooks hook

# Process Cursor hooks
claw-hooks hook --format cursor

# Process Windsurf hooks
claw-hooks hook --format windsurf

# Process Antigravity CLI hooks (pass --event: its payloads have no event-name field)
claw-hooks hook --format agy --event PreToolUse
claw-hooks hook --format agy --event PostToolUse

# Process Codex CLI hooks
claw-hooks hook --format codex

# Process Grok CLI hooks
claw-hooks hook --format grok

# Use custom config
claw-hooks hook --config /path/to/config.toml
```

## Format Detection Logic

Each AI agent sends different JSON structures. claw-hooks uses `--format` to determine parsing.

### Claude Code (`--format claude`)

Uses the official Claude Code hooks specification:

```jsonc
// PreToolUse/PostToolUse events
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "..." },
  "session_id": "...",
  "cwd": "/path/to/project"
}

// Stop event (no tool_name/tool_input)
{
  "hook_event_name": "Stop",
  "stop_hook_active": true,
  "session_id": "..."
}
```

Handled hook events: `PreToolUse`, `PostToolUse`, `Stop`, `SubagentStart`, and `SubagentStop`. Known lifecycle events outside claw-hooks' scope — including `Notification`, `PermissionRequest`, `UserPromptSubmit`, `SessionStart`, and `SessionEnd` — pass through without a decision.

`stop_hook_active` is required on Claude `Stop`. If it is absent or mistyped, claw-hooks treats the payload as malformed, does not run stop hooks, and allows the session to terminate (`{}` + exit `0`). Defaulting an unreadable guard to `false` would run the hooks and could make a failing reported hook re-trigger `Stop` forever.

### Cursor (`--format cursor`)

Uses the `hook_event_name` field for event detection:

| `hook_event_name` | Required Fields | Internal Mapping |
|-------------------|-----------------|------------------|
| `preToolUse` (any tool) | — (`tool_input.command` selects the command path; without it the event passes through) | PreToolUse + Bash |
| `beforeShellExecution` | `command` | PreToolUse + Bash |
| `afterFileEdit` / `afterTabFileEdit` | `file_path` / `filePath` | PostToolUse + Write |
| `stop` | — (`loop_count` is read when present) | Stop |

Unsupported Cursor events, including non-shell `preToolUse` tools, pass through as an empty object (`{}`) rather than `{"permission":"allow"}`. Cursor merges hook responses from several sources and a higher-priority `allow` can override another hook's `deny`, so claw-hooks never votes to approve an event it did not inspect (`beforeReadFile`, `beforeMCPExecution`, `beforeTabFileRead`, `sessionStart`, `postToolUse`, …). Allowed commands return `{}` for the same reason.

Blocks are returned as `{"permission":"deny", …}` on stdout with exit code `0`. Cursor only consumes the stdout JSON when the hook exits `0`, so exiting `2` would discard the `user_message` that carries the "use safe-rm instead" guidance. Claude Code differs here: its current hook contract reads valid stdout JSON on every exit code, while exit `2` remains unconditionally blocking.

For `stop`, Cursor's `loop_count` field (how many automatic follow-ups the stop hook has already triggered, starting at 0) is used for loop prevention: when it is 1 or higher, all stop hooks are skipped — the same role `stop_hook_active` plays for Claude Code, so a failing lint feeds back to the agent once instead of looping up to Cursor's `loop_limit`.

Malformed `stop` payloads let the stop through (`{}` + exit `0`) instead of failing closed: a `followup_message` is auto-submitted as the next user message, so returning one for a payload claw-hooks could not parse would re-trigger the same failure forever. See [Fail-Closed Behavior](#fail-closed-behavior).

### Windsurf (`--format windsurf`)

Uses `agent_action_name` field:

| agent_action_name | Internal Mapping |
|-------------------|------------------|
| `pre_run_command` | PreToolUse + Bash |
| `post_write_code` | PostToolUse + Write |
| `post_cascade_response` | Stop |

Unsupported Windsurf actions are passed through as allow.

### Antigravity CLI (`--format agy`)

camelCase schema. A representative PreToolUse payload:

```jsonc
{
  "toolCall": {
    "name": "run_command",
    "args": { "CommandLine": "rm -rf /tmp/test", "Cwd": "/workspace" }
  },
  "stepIdx": 3,
  "conversationId": "…",
  "workspacePaths": ["/workspace/project"],
  "transcriptPath": "~/.gemini/antigravity-cli/brain/…/transcript.jsonl",
  "artifactDirectoryPath": "~/.gemini/antigravity-cli/brain/…"
}
```

Official Antigravity payloads do not include an event-name field, and `PreToolUse` and `PostToolUse` are **shape-identical** — both carry `toolCall` and `stepIdx`, differing only in an optional `error`. Pass `--event <name>` so claw-hooks knows which one it received; `hooks.json` registers each event separately, so the calling entry always knows. Resolution order is `--event`, then a legacy non-blank `hook_event_name` / `event` field, then shape inference (`toolCall` → PreToolUse, Stop fields → Stop, invocation fields → Pre/PostInvocation). Inference resolves the PreToolUse/PostToolUse ambiguity to **PreToolUse**, keeping command blocking intact — the opposite choice would let a not-yet-executed command through. `error` is deliberately not used as a discriminator: the spec marks it Optional ("Empty if successful"), so keying on it would misclassify every *successful* tool call.

Required-field validation is limited to what claw-hooks actually uses for a decision. The spec marks only Stop's `fullyIdle` (and the output `decision`) as **Required**, so `stepIdx`, `executionNum` and `terminationReason` are all optional here. Requiring them would answer every `run_command` with a deny — which Antigravity documents as an immediate hard block — and, on Stop, would silently skip every stop hook. A Stop parse error therefore resolves to `{"decision":"stop"}` + exit 0: this satisfies the required output schema without returning `continue`, which would create a re-entry loop. `toolCall.args` is required only for `run_command`, because the spec documents zero-argument tools and allows `matcher: ""` / `"*"`. Missing, blank, or incorrectly typed required fields fail closed using the inferred event's native response.

| Inferred event shape | toolCall.name | Internal Mapping |
|---|---|---|
| `toolCall` + `stepIdx` (PreToolUse) | `run_command` | BeforeCommand (`toolCall.args.CommandLine` → Bash) |
| `toolCall` + `stepIdx` (PreToolUse) | other (`write_to_file`, `replace_file_content`, …) | pass-through allow |
| `stepIdx` without `toolCall`, or invocation fields | n/a | PostToolUse / invocation pass-through allow (out of claw-hooks scope) |
| `executionNum` / `terminationReason` / `fullyIdle` | n/a | Stop |

> **Extension hooks**: Antigravity's `PostToolUse` carries `toolCall` (`name` and `args`), so the edited path is recoverable from `args.TargetFile` for `write_to_file` / `replace_file_content` / `multi_replace_file_content`. Add `--event PostToolUse` to that hook entry — the payload is shape-identical to `PreToolUse`, so without the flag claw-hooks infers `PreToolUse` and the post-edit hooks stay inactive. The official output is fixed at `{}`, so formatters and linters run but their diagnostics can't be returned; run project-wide lint/typecheck as Stop hooks and surface failures via `"decision":"continue"` when you need the text. `PostToolUse` for `run_command` passes through — the command already ran, and blocking it afterwards is neither possible nor meaningful. The output JSON shapes are listed in [Input/Output Reference](#inputoutput-reference). Explicitly named unsupported events pass through as allow; an unidentifiable nameless payload fails closed because no event-specific response shape can be selected safely.

### Codex CLI (`--format codex`)

Standard `hook_event_name` + `tool_name` + `tool_input` schema. `apply_patch`'s `tool_input.command` is parsed for the `*** Add/Update/Move to File:` headers to drive extension hooks (delete-only patches are skipped).

Validation is limited to the fields claw-hooks actually reads: the event name, `tool_name` / `tool_input` (plus `tool_input.command` for `Bash` and the patch body for `apply_patch`), and `stop_hook_active` on `Stop`. The official docs present `session_id`, `cwd`, `model`, `transcript_path`, `turn_id`, and `permission_mode` as the shared fields you will usually see rather than as a strict schema — their own `SessionEnd` example payload omits `model` — so requiring them meant a single absent field could fail closed on every hook call. Out-of-scope pass-through events are not validated at all.

`PostToolUse` for non-file tools (for example `Bash`) is also passed through without strict validation, because a Codex `PostToolUse` block *replaces the real tool output* with the hook message: failing closed there would hide the command's own output from the model while gaining nothing, since only file paths matter for post-edit hooks. Fields that claw-hooks does read still fail closed with the event's native deny/block response when they are missing or mistyped.

`Interrupt` and MCP/function tools that claw-hooks does not inspect, including `mcp__*`, are out of scope. They pass through with the neutral `{}` response rather than an explicit allow, so claw-hooks does not override Codex's own permission flow or another hook's decision.

| hook_event_name | Internal Mapping |
|-----------------|------------------|
| `SessionStart` / `SessionEnd` / `UserPromptSubmit` / `PreCompact` / `PostCompact` / `Interrupt` | pass-through allow |
| `PreToolUse` | BeforeCommand |
| `PermissionRequest` | command guard before approval prompts (deny for dangerous Bash, `{}` for safe) |
| `PostToolUse` | AfterFileEdit (`Bash` pass-through; `apply_patch` → MultiEdit) |
| `Stop` | Stop |

Codex returns all decisions — allow, block, and fail-closed — with exit code `0`; non-zero is treated as hook infrastructure failure. See [Input/Output Reference](#inputoutput-reference) for the per-event output JSON.

### Grok CLI (`--format grok`)

camelCase schema with an explicit `hookEventName` field:

```jsonc
{
  "hookEventName": "PreToolUse",
  "sessionId": "…",
  "cwd": "/path/to/project",
  "workspaceRoot": "/path/to/project",
  "toolName": "Bash",
  "toolInput": { "command": "rm -rf /tmp/test" }
}
```

| hookEventName | `toolInput` shape | Internal Mapping |
|---|---|---|
| `PreToolUse` | `command` | BeforeCommand (the only event Grok lets a hook block) |
| `PreToolUse` | file path, or neither | pass-through allow |
| `PostToolUse` | `file_path` / `filePath` | AfterFileEdit (extension hooks) |
| `PostToolUse` | `command`, or neither | pass-through allow |
| `Stop` | n/a | Stop |
| `SessionStart` / `SessionEnd` / `UserPromptSubmit` / `PostToolUseFailure` / `PermissionDenied` / `StopFailure` / `Notification` / `PreCompact` / `PostCompact` | n/a | pass-through allow |

claw-hooks dispatches on the **shape of `toolInput`, not on `toolName`**. Grok states that it maps Claude tool names such as `Bash` and `Edit` onto its own, but the mapped names are not part of the published spec, so matching by name would let an unanticipated shell tool slip past the command filter. A payload carrying `command` therefore goes to the command filters and one carrying `file_path` / `filePath` goes to the extension hooks; anything else passes through. `toolName` and `toolInput` are both **optional** for the same reason: neither is what the decision is made from, so requiring them would deny unrelated tool calls (a tool without arguments omits `toolInput` entirely), and `PreToolUse` is the one path where claw-hooks can hard-block on Grok. Legacy snake_case keys (`hook_event_name`, `session_id`, `tool_name`, `tool_input`) are accepted as well, because Grok also reads Claude Code and Cursor hook files.

Grok's contract is fail-open: exit `0` allows, exit `2` denies, and every other outcome — timeout, crash, malformed stdout — records a failure but lets the tool call proceed. claw-hooks therefore blocks with the deny JSON **and** exit code `2` so the decision holds under either interpretation, and never exits `1` on a fail-closed path. Allowed commands return `{}` rather than an `allow` decision, since `deny` is the only documented `decision` value.

### Event Mapping Summary

```mermaid
graph LR
    subgraph Before Command
        CC1[Claude: PreToolUse + Bash]
        CU1[Cursor: preToolUse Shell / beforeShellExecution]
        WS1[Windsurf: pre_run_command]
        AG1[Antigravity: PreToolUse + run_command]
        CX1[Codex: PreToolUse + Bash]
        GR1[Grok: PreToolUse + command]
    end
    CH1[🛡️ Validate & suggest alternatives]
    CC1 --> CH1
    CU1 --> CH1
    WS1 --> CH1
    AG1 --> CH1
    CX1 --> CH1
    GR1 --> CH1

    subgraph After File Save
        CC2[Claude: PostToolUse + Write/Edit]
        CU2[Cursor: afterFileEdit]
        WS2[Windsurf: post_write_code]
        CX2[Codex: PostToolUse + apply_patch]
        GR2[Grok: PostToolUse + file path]
    end
    CH2["🔧 Run commands by extension<br>then the * key (all files)"]
    CC2 --> CH2
    CU2 --> CH2
    WS2 --> CH2
    CX2 --> CH2
    GR2 --> CH2

    subgraph Agent Stop
        CC3[Claude: Stop]
        CU3[Cursor: stop]
        WS3[Windsurf: post_cascade_response]
        AG3[Antigravity: Stop]
        CX3[Codex: Stop]
        GR3[Grok: Stop]
    end
    CH3[⏹️ Lint / notifications / cleanup]
    CC3 --> CH3
    CU3 --> CH3
    WS3 --> CH3
    AG3 --> CH3
    CX3 --> CH3
    GR3 --> CH3
```

Codex `PostToolUse` with `Bash` is omitted from the "After File Save" flow because it is command-output feedback. Only `apply_patch` payloads are treated as file-write events. Antigravity CLI joins the "After File Save" flow only when its hook entry passes `--event PostToolUse`; claw-hooks recovers the edited path from `toolCall.args.TargetFile`. Its output remains fixed at `{}`, so use Stop hooks when the lint text itself must reach the agent. Grok CLI appears in all three groups, but only its `PreToolUse` can block; the other two are post-hooks whose output Grok ignores, so their work is real but their feedback is not.

## Input/Output Reference

Stdin: the agent's native hook JSON (see [Format Detection Logic](#format-detection-logic) for per-agent payloads). Stdout/stderr: one of the JSON bodies below, picked by `(format, event)`.

| Agent | Event | Allow | Block / fail-closed |
|---|---|---|---|
| Claude Code | PreToolUse | `{}` (no decision — the normal permission flow still applies), or `…additionalContext:"…"`, still without a decision, when a command hook returns context | `…permissionDecision:"deny", permissionDecisionReason:"…"` (exit 0). Parse errors: plain text on **stderr**, exit 2 |
| Claude Code | PostToolUse | `{}` or `…additionalContext:"…"` (lint feedback) | `{"decision":"block","reason":"…"}` |
| Claude Code | Stop | `{}` | `{"decision":"block","reason":"…"}` |
| Cursor | preToolUse / beforeShellExecution | `{}` | `{"permission":"deny","user_message":"…","agent_message":"…"}` (exit 0 — Cursor reads the stdout JSON only on exit 0) |
| Cursor | stop | `{}` | `{"followup_message":"…"}` |
| Windsurf | pre_run_command | `{}` | exit code 2 + **stderr** plain text (not JSON) |
| Windsurf | post_write_code | `{}` (no findings) | exit code 2 + **stderr** plain text (lint findings; post-hooks cannot block, so the edit stands) |
| Windsurf | post_cascade_response | `{}` | `{}` (best-effort post-hook; cannot block) |
| Antigravity | PreToolUse | `{"decision":"allow"}` | `{"decision":"deny","reason":"…"}` |
| Antigravity | PostToolUse / PreInvocation / PostInvocation | `{}` | `{}` (spec defines no block path) |
| Antigravity | Stop | `{"decision":"stop"}` | `{"decision":"continue","reason":"…"}` (re-enters the agent loop, `reason` injected as a system message) |
| Codex CLI | any | `{}` or `…additionalContext:"…"` | PreToolUse: `…permissionDecision:"deny",…`. PermissionRequest: `…decision:{behavior:"deny",message:"…"}`. PostToolUse / Stop: `{"decision":"block","reason":"…"}` |
| Grok CLI | PreToolUse | `{}` | `{"decision":"deny","reason":"…"}` **and** exit 2 |
| Grok CLI | PostToolUse / Stop / other events | `{}` | `{}` (post-hook stdout is ignored; cannot block) |

`additionalContext` carries lint feedback to Claude `PostToolUse` and Codex `PostToolUse`, and command hook context to Claude `PreToolUse` and Codex `PreToolUse`. Windsurf has no such field, so `post_write_code` findings go out as exit 2 + stderr. Antigravity has no `additionalContext` channel — emit lint feedback via Stop `"decision":"continue"` instead. Grok CLI has no channel at all for post-hooks: the tools run, but their output stays out of the transcript.

claw-hooks never emits an `allow` decision for Claude Code, Cursor, or Grok CLI. `{}` + exit `0` means "claw-hooks has no objection", so the agent's own permission prompts and rules still decide. Antigravity's event schemas require explicit decisions: safe `PreToolUse` returns `"allow"`, while an allowed Stop returns the non-continuing value `"stop"`.

### Exit Codes

| Agent | Allow | Block | Fail-closed parse error |
|---|---|---|---|
| Claude Code | `0` (decision in stdout JSON) | `0` (decision in stdout JSON) | `2` + **stderr** plain text |
| Cursor | `0` | `0` (deny JSON in stdout; exit `2` would make Cursor discard the message) | `2` |
| Windsurf | `0` | `2` (BeforeCommand writes plain text to stderr; AfterFileEdit uses the same channel to report lint findings without blocking; Stop stays `0`) | `2` (`pre_run_command` only; post-hooks return `{}` + `0`) |
| Antigravity CLI | `0` (decision in stdout JSON) | `0` (decision in stdout JSON) | `0` + event-specific deny JSON |
| Codex CLI | `0` (decision in stdout JSON) | `0` (decision in stdout JSON) | `0` + event-specific deny/block JSON (non-zero is treated as hook infra failure and discarded) |
| Grok CLI | `0` | `2` + deny JSON in stdout (PreToolUse only; other events return `0`) | `2` (never `1` — Grok treats anything other than `2` as fail-open) |

The "fail-closed parse error" column applies to the pre-execution gates only. Every other event — stop events, post-edit hooks, and the lifecycle events claw-hooks passes through — returns a neutral `{}` + exit `0` instead of the deny shown above. See below.

### Fail-Closed Behavior

**Pre-execution gates fail closed.** When the payload cannot be parsed, stdin is empty or oversized, or a field claw-hooks actually reads is missing, the command-blocking events (`PreToolUse`, `beforeShellExecution`, `pre_run_command`, `PermissionRequest`) return the agent's native deny response. A broken hook never turns into a silent approval.

**A broken config denies too, instead of disabling protection.** If the TOML config fails to load or validate, claw-hooks answers with that same deny response, writes the diagnostic to stderr, and suggests running `claw-hooks check`. It does not exit `1` with empty stdout — Codex CLI and Antigravity CLI read that as "the hook failed, ignore its decision", so one typo in `config.toml` would switch off command blocking entirely. Logging is diagnostics rather than a control, so a logger that cannot be initialized only prints a warning and claw-hooks keeps running without logs.

**Stop events allow instead.** On a stop event, "block" does not mean deny — it means *don't stop, here is a new prompt*: `decision:"block"` for Claude Code and Codex CLI, `decision:"continue"` for Antigravity CLI, and Cursor's `followup_message` is auto-submitted as the next user message. Returning that for a malformed payload or a broken config is self-sustaining: fail → continue → `Stop` fires again → same failure. None of the loop guards (`stop_hook_active`, `loop_count`, `CLAW_HOOKS_STOP_ACTIVE`) can break the cycle, because all of them only engage after a successful parse. Stop is not a pre-execution gate, so claw-hooks returns the event-specific stop-allow response + exit `0` (`{"decision":"stop"}` for Antigravity, `{}` for the other agents). This adds no new side effects, whereas auto-continuing would invite more tool calls.

**Events claw-hooks never inspects allow too.** Denying an event whose contents claw-hooks never looks at buys no safety and costs real work: a `UserPromptSubmit` deny erases the user's prompt, a Codex `PostToolUse` deny replaces the actual tool output with the hook's message, and Cursor's `beforeReadFile` carries the whole file body — so a large file trivially exceeds the 4 MiB stdin limit and the read would be blocked by a tool that has no opinion on reads. Windsurf's post-hooks and every Grok event except `PreToolUse` cannot block at all, so a deny there only injects a spurious error. These all return `{}` + exit `0`.

**An unidentifiable payload still blocks.** The rules above are keyed on the event name. When the payload is damaged badly enough that claw-hooks cannot recover the event name, it falls back to the deny response — so a truncated or oversized `PreToolUse` is still blocked.

**Command hook checkers follow `on_error` instead.** A checker that crashes, times out, or cannot be started is not covered by the rules above. Its hook's `on_error` decides, and the default lets the command through. See [Command Hook Protocol](#command-hook-protocol).

## Command Hook Protocol

A command hook (`[[command_hooks]]`, see [Configuration](configuration.md#command-hooks)) runs its checker once for each matching call in a shell command. This section is the contract between claw-hooks and the checker.

### When Checkers Run

Checkers run only for shell tool calls (`Bash` / `PowerShell`) on the events in the "Before Command" group of the [event mapping](#event-mapping-summary), and on Codex CLI's `PermissionRequest`. They run after the built-in `rm` / `kill` / `dd` filters and the custom filters, so a command that one of those blocks never reaches a checker.

Calls are checked in the order they appear in the command, and several hooks matching the same call run in config order. A check with the same hook and the same input as an earlier one is not repeated. The first block ends the event: later checkers do not run, and any context collected so far is dropped.

Calls behind wrappers (`sudo`, `env`, `timeout`, …), in shell strings (`bash -c`, `eval`, `env -S`, `trap`), in here-documents and here-strings fed to a shell, and under `xargs` / `find -exec` are calls of their own. `sudo gws docs …`, for example, yields a `sudo` call and a `gws` call.

### Input

claw-hooks writes one line of JSON to the checker's stdin and closes it.

```json
{
  "version": 1,
  "agent": "claude-code",
  "event": "PreToolUse",
  "tool_name": "Bash",
  "session_id": "abc",
  "cwd": "/path/to/project",
  "analysis": "complete",
  "context_delivery": true,
  "argv": [
    {"value": "gws", "static": true, "cardinality": "one"},
    {"value": "docs", "static": true, "cardinality": "one"},
    {"value": null, "static": false, "cardinality": "zero_or_more"}
  ],
  "stdin": null
}
```

| Field | Meaning |
|---|---|
| `version` | Always `1` |
| `agent` | The agent that sent the event: `claude-code`, `codex`, `cursor`, `windsurf`, `antigravity`, or `grok` |
| `event` | claw-hooks' own name for the event: `PreToolUse` for every agent's pre-execution command event (Cursor's `beforeShellExecution` and Windsurf's `pre_run_command` included), `PermissionRequest` for Codex CLI's `PermissionRequest` |
| `tool_name` | `Bash` or `PowerShell` |
| `session_id` | The agent's session ID, or `null` |
| `cwd` | The directory the command runs in, as reported by the agent. If the agent reports none, claw-hooks' own working directory; `null` if that cannot be read either |
| `analysis` | `complete` or `uncertain` (see below) |
| `context_delivery` | Whether stdout on exit `0` reaches the agent: `true` for Claude Code and Codex CLI on `PreToolUse`, `false` otherwise. A checker can use it to skip writing context that would be dropped |
| `argv` | The words of the call, program name first. Each element has `value`, `static` and `cardinality` |
| `stdin` | What the call reads on standard input (see below) |

The command string itself is not passed. It can hold secrets, or text that has nothing to do with the program being checked, so the checker sees the matching call only.

With Codex CLI, when both `PreToolUse` and `PermissionRequest` are registered, a command that asks for approval reaches the checker at each of the two events. `event` tells them apart.

**`argv` elements.** `value` is the argument's run-time value after quote removal when claw-hooks can determine it statically, and `null` when it cannot (`$VAR`, `$(date)`). `static` is `true` exactly when `value` is not `null`. `cardinality` is `"one"` when the element becomes exactly one argument at run time, and `"zero_or_more"` when it can expand to any number of arguments, including none: an unquoted expansion, a glob, a brace expansion, or the arguments `xargs` appends. After a `zero_or_more` element, the positions of the remaining arguments are not fixed.

Two cases are worth knowing when writing a checker:

- Inside double quotes, `$(cat <<'EOF' … EOF)` (a `cat` with no arguments reading a single here-document) is static. Its value is the here-document body with the trailing newlines removed, as command substitution does. This covers the `gws … --json "$(cat <<'EOF' … EOF)"` form.
- Under `xargs`, the inner call ends with one extra `zero_or_more` element that stands for the arguments `xargs` appends. With `-I`, nothing is appended; instead, each word containing the replacement string is not static. The `{}` of `find -exec` is not static either.

**`analysis`.** `"complete"` means the call is certain from the command's syntax. `"uncertain"` means the call is a candidate whose words may not be exactly what runs: it was found by re-parsing a string that is not static (`bash -c "gws docs $ARGS"`), in a command that contains syntax errors, or in a `PowerShell` tool command, which claw-hooks reads with shell grammar. Candidates that claw-hooks builds only for dangerous-command detection, such as the one made by folding a brace expansion in the program name to its first choice, are never passed to a checker.

**`stdin`.** `null` when the call has no input redirection and is not fed by a pipe, so it inherits the agent's standard input. `{"value": "…", "static": true}` when the input is a here-document or here-string whose body is known statically, as in `gws … <<'EOF'`. `{"value": null, "static": false}` when something is fed in but its content is unknown: a pipe, `< file`, or a here-document or here-string that is not static (`<<< "$TEXT"`).

### Output and Exit Code

| Checker result | What claw-hooks does |
|---|---|
| Exit `0`, stdout empty or whitespace only | Nothing |
| Exit `0`, output on stdout | No objection. Where `context_delivery` is `true`, the output goes to the agent as context, as `[<label>] <output>`; elsewhere it is dropped |
| Exit `2` | Blocks the command with `[<label>] <reason>`. The reason is stderr; if stderr is empty, stdout; if both are empty, `blocked by command hook` |
| Any other exit code, a signal, a start failure, a timeout, or a reported working directory that is not a directory | A checker failure, handled by `on_error`. `allow` lets the command through and records a warning in the debug log. `block` blocks it with `[<label>] command hook failed: <cause>`, where `<cause>` is, for example, `timed out after 5s`, `exit code 1`, `terminated by a signal`, or `could not be started` |

`<label>` is the basename of the program in `run`: `noslop` for `run = "noslop hook command"`. ANSI escape sequences are removed from the checker's output and surrounding whitespace is trimmed; the text sent to the agent is then cut to `output_max_length` like any other hook output. claw-hooks keeps at most 4 MiB each of stdout and stderr. A checker that exits without reading its stdin is not treated as failing.

### Where Context Reaches the Agent

| Agent | Context from exit `0` |
|---|---|
| Claude Code | `PreToolUse`: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"…"}}` with no `permissionDecision`, so the normal permission flow still applies |
| Codex CLI | `PreToolUse`: the same shape. `PermissionRequest`: dropped, and `{}` is returned |
| Cursor, Windsurf, Antigravity CLI, Grok CLI | Dropped; the usual allow response is returned |

Blocks work on every agent and use the same deny response as the built-in filters (see [Input/Output Reference](#inputoutput-reference)). Context is returned only when no checker blocked the command.

### Limits

- **At most 32 checker runs per hook event.** Runs beyond that are not made, and each skipped hook's `on_error` decides; `block` blocks with `[<label>] command hook skipped: too many invocations to check`.
- **Each run is limited by its hook's `timeout` alone.** Command hooks do not use `hook_timeout`, which a project config can override (see [Configuration](configuration.md#command-hooks)). Together with the 32-run cap, one hook event spends at most 32 runs of `timeout` seconds each (160 seconds with the default).
- **Commands too large to analyze.** When a command is too long or nested too deeply for the parser, no checker runs. The command is blocked with `[<label>] command hook could not analyze the command` if any command hook sets `on_error = "block"`, and allowed otherwise.

### Working Directory and Environment

The checker runs in the working directory the agent reported for the command, when that is an existing directory. When the reported path is not a directory, the checker is not started and `on_error` decides. When the agent reported none, the checker inherits claw-hooks' working directory.

The checker inherits claw-hooks' environment plus `CLAW_HOOKS_COMMAND_HOOK_ACTIVE=1`. A claw-hooks process that starts with that variable set skips command hooks entirely, so a checker that drives an agent whose hooks call claw-hooks again cannot recurse. The other filters still run in that process.

### Calls That Are Not Checked

A hook matches a call by its program name, so the name has to be written out in the command. When it is only known at run time, as in `$CMD args` or `"$(which gws)" docs …`, claw-hooks cannot tell which program will run, and the call is not checked. Command hooks are therefore no defense against an agent that deliberately hides the program name.

### Logging

Debug logs keep only the checker's program name, the number of matching calls, exit codes, byte counts, durations and the `on_error` policy. The arguments, the input JSON and the checker's output are not written to the log.
