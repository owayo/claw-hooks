---
title: "Cascade Hooks"
url: "https://docs.windsurf.com/windsurf/cascade/hooks"
---

Cascade

# Cascade Hooks

Execute custom shell commands at key points in Cascade’s workflow for logging, security controls, validation, and enterprise governance with pre and post hooks.

Cascade Hooks enable you to execute custom shell commands at key points during Cascade’s workflow. This powerful extensibility feature allows you to log operations, enforce guardrails, run validation checks, or integrate with external systems.

Hooks are designed for power users and enterprise teams who need fine-grained control over Cascade’s behavior. They require basic shell scripting knowledge.

## [​](#what-you-can-build)
What You Can Build
Hooks unlock a wide range of automation and governance capabilities:

* **Logging & Analytics**: Track every file read, code change, command executed, user prompt, or Cascade response for compliance and usage analysis
* **Security Controls**: Block Cascade from accessing sensitive files, running dangerous commands, or processing policy-violating prompts
* **Quality Assurance**: Run linters, formatters, or tests automatically after code modifications
* **Custom Workflows**: Integrate with issue trackers, notification systems, or deployment pipelines
* **Team Standardization**: Enforce coding standards and best practices across your organization

## [​](#how-hooks-work)
How Hooks Work
Hooks are shell commands that run automatically when specific Cascade actions occur. Each hook:

1. **Receives context** (details about the action being performed) via JSON as standard input
2. **Executes your script** - Python, Bash, Node.js, or any executable
3. **Returns a result** via exit code and output streams
For **pre-hooks** (executed before an action), your script can **block the action** by exiting with exit code `2`. This makes pre-hooks ideal for implementing security policies or validation checks.
## [​](#configuration)
Configuration
Hooks are configured in JSON files that can be placed at three different levels. Cascade loads and merges hooks from all locations, giving teams flexibility in how they distribute and manage hook configurations.
#### [​](#system-level)
System-Level
System-level hooks are ideal for organization-wide policies enforced on shared development machines. For example, you can use them to enforce security policies, compliance requirements, or mandatory code review workflows.

* **macOS**: `/Library/Application Support/Windsurf/hooks.json`
* **Linux/WSL**: `/etc/windsurf/hooks.json`
* **Windows**: `C:\ProgramData\Windsurf\hooks.json`

#### [​](#user-level)
User-Level
User-level hooks are perfect for personal preferences and optional workflows.

* **Windsurf IDE**: `~/.codeium/windsurf/hooks.json`
* **JetBrains Plugin**: `~/.codeium/hooks.json`

#### [​](#workspace-level)
Workspace-Level
Workspace-level hooks allow teams to version control project-specific policies alongside their code. They may include custom validation rules, project-specific integrations, or team-specific workflows.

* **Location**: `.windsurf/hooks.json` in your workspace root

Hooks from all three locations are **merged together**. If the same hook event is configured in multiple locations, all hooks will execute in order: system → user → workspace.

### [​](#basic-structure)
Basic Structure
Here is an example of the basic structure of the hooks configuration:

```
{
  "hooks": {
    "pre_read_code": [
      {
        "command": "python3 /path/to/your/script.py",
        "show_output": true
      }
    ],
    "post_write_code": [
      {
        "command": "python3 /path/to/another/script.py",
        "show_output": true
      }
    ]
  }
}
```

### [​](#configuration-options)
Configuration Options
Each hook accepts the following parameters:

|Parameter|Type|Description|
|---|---|---|
|`command`|string|The shell command to execute. Can be any valid executable with arguments.|
|`show_output`|boolean|Whether to display the hook’s stdout/stderr output on the user-facing Cascade UI. Useful for debugging.|
|`working_directory`|string|Optional. The directory to execute the command from. Defaults to your workspace root.|

## [​](#hook-events)
Hook Events
Cascade provides eleven hook events that cover the most critical actions in the agent workflow.
### [​](#common-input-structure)
Common Input Structure
All hooks receive a JSON object with the following common fields:

|Field|Type|Description|
|---|---|---|
|`agent_action_name`|string|The hook event name (e.g., “pre\_read\_code”, “post\_write\_code”)|
|`trajectory_id`|string|Unique identifier for the overall Cascade conversation|
|`execution_id`|string|Unique identifier for the single agent turn|
|`timestamp`|string|ISO 8601 timestamp when the hook was triggered|
|`tool_info`|object|Event-specific information (varies by hook type)|
In the following examples, the common fields are omitted for brevity. There are eleven major types of hook events:
### [​](#pre_read_code)
pre\_read\_code
Triggered **before** Cascade reads a code file. This may block the action if the hook exits with code 2. **Use cases**: Restrict file access, log read operations, check permissions **Input JSON**:

```
{
  "agent_action_name": "pre_read_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py"
  }
}
```
This `file_path` may be a directory path when Cascade reads a directory recursively.
### [​](#post_read_code)
post\_read\_code
Triggered **after** Cascade successfully reads a code file. **Use cases**: Log successful reads, track file access patterns **Input JSON**:

```
{
  "agent_action_name": "post_read_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py"
  }
}
```
This `file_path` may be a directory path when Cascade reads a directory recursively.
### [​](#pre_write_code)
pre\_write\_code
Triggered **before** Cascade writes or modifies a code file. This may block the action if the hook exits with code 2. **Use cases**: Prevent modifications to protected files, backup files before changes **Input JSON**:

```
{
  "agent_action_name": "pre_write_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py",
    "edits": [
      {
        "old_string": "def old_function():\n    pass",
        "new_string": "def new_function():\n    return True"
      }
    ]
  }
}
```

### [​](#post_write_code)
post\_write\_code
Triggered **after** Cascade writes or modifies a code file. **Use cases**: Run linters, formatters, or tests; log code changes **Input JSON**:

```
{
  "agent_action_name": "post_write_code",
  "tool_info": {
    "file_path": "/Users/yourname/project/file.py",
    "edits": [
      {
        "old_string": "import os",
        "new_string": "import os\nimport sys"
      }
    ]
  }
}
```

### [​](#pre_run_command)
pre\_run\_command
Triggered **before** Cascade executes a terminal command. This may block the action if the hook exits with code 2. **Use cases**: Block dangerous commands, log all command executions, add safety checks **Input JSON**:

```
{
  "agent_action_name": "pre_run_command",
  "tool_info": {
    "command_line": "npm install package-name",
    "cwd": "/Users/yourname/project"
  }
}
```

### [​](#post_run_command)
post\_run\_command
Triggered **after** Cascade executes a terminal command. **Use cases**: Log command results, trigger follow-up actions **Input JSON**:

```
{
  "agent_action_name": "post_run_command",
  "tool_info": {
    "command_line": "npm install package-name",
    "cwd": "/Users/yourname/project"
  }
}
```

### [​](#pre_mcp_tool_use)
pre\_mcp\_tool\_use
Triggered **before** Cascade invokes an MCP (Model Context Protocol) tool. This may block the action if the hook exits with code 2. **Use cases**: Log MCP usage, restrict which MCP tools can be used **Input JSON**:

```
{
  "agent_action_name": "pre_mcp_tool_use",
  "tool_info": {
    "mcp_server_name": "github",
    "mcp_tool_arguments": {
      "owner": "code-owner",
      "repo": "my-cool-repo",
      "title": "Bug report",
      "body": "Description of the bug here"
    },
    "mcp_tool_name": "create_issue"
  }
}
```

### [​](#post_mcp_tool_use)
post\_mcp\_tool\_use
Triggered **after** Cascade successfully invokes an MCP tool. **Use cases**: Log MCP operations, track API usage, see MCP results **Input JSON**:

```
{
  "agent_action_name": "post_mcp_tool_use",
  "tool_info": {
    "mcp_result": "...",
    "mcp_server_name": "github",
    "mcp_tool_arguments": {
      "owner": "code-owner",
      "perPage": 1,
      "repo": "my-cool-repo",
      "sha": "main"
    },
    "mcp_tool_name": "list_commits"
  }
}
```

### [​](#pre_user_prompt)
pre\_user\_prompt
Triggered **before** Cascade processes the text of a user’s prompt. This may block the action if the hook exits with code 2. **Use cases**: Log all user prompts for auditing, block potentially harmful or policy-violating prompts **Input JSON**:

```
{
  "agent_action_name": "pre_user_prompt",
  "tool_info": {
    "user_prompt": "can you run the echo hello command"
  }
}
```
The `show_output` configuration option does not apply to this hook.
### [​](#post_cascade_response)
post\_cascade\_response
Triggered **after** Cascade completes a response to a user’s prompt. This hook receives the full Cascade response ever since the last user input. **Use cases**: Log all Cascade responses for auditing, analyze response patterns, send responses to external systems for compliance review **Input JSON**:

```
{
  "agent_action_name": "post_cascade_response",
  "tool_info": {
    "response": "### Planner Response\n\nI'll help you create that file.\n\n*Created file `/path/to/file.py`*\n\n### Planner Response\n\nThe file has been created successfully."
  }
}
```
The `response` field contains the markdown-formatted content of Cascade’s response since the last user input. This includes planner responses, tool actions (file reads, writes, commands), and any other steps Cascade took. The `show_output` configuration option does not apply to this hook.

The `response` content is derived from trajectory data and may contain sensitive information from your codebase or conversations. Handle this data according to your organization’s security and privacy policies.

### [​](#post_setup_worktree)
post\_setup\_worktree
Triggered **after** a new [git worktree](./worktrees) is created and configured. The hook is executed inside the new **worktree** directory. **Use cases**: Copy `.env` files or other untracked files into the worktree, install dependencies, run setup scripts **Environment Variables**:

|Variable|Description|
|---|---|
|`$ROOT_WORKSPACE_PATH`|The absolute path to the original workspace. Use this to access files or run commands relative to the original repository.|
**Input JSON**:

```
{
  "agent_action_name": "post_setup_worktree",
  "tool_info": {
    "worktree_path": "/Users/me/.windsurf/worktrees/my-repo/abmy-repo-c123",
    "root_workspace_path": "/Users/me/projects/my-repo"
  }
}
```

## [​](#exit-codes)
Exit Codes
Your hook scripts communicate results through exit codes:

|Exit Code|Meaning|Effect|
|---|---|---|
|`0`|Success|Action proceeds normally|
|`2`|Blocking Error|The Cascade agent will see the error message from stderr. For pre-hooks, this **blocks** the action.|
|Any other|Error|Action proceeds normally|

Only **pre-hooks** (pre\_user\_prompt, pre\_read\_code, pre\_write\_code, pre\_run\_command, pre\_mcp\_tool\_use) can block actions using exit code 2. Post-hooks cannot block since the action has already occurred.
Keep in mind that the user can see any hook-generated standard output and standard error in the Cascade UI if `show_output` is true.
## [​](#example-use-cases)
Example Use Cases

### [​](#logging-all-cascade-actions)
Logging All Cascade Actions
Track every action Cascade takes for auditing purposes. **Config**:

```
{
  "hooks": {
    "post_read_code": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_write_code": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_run_command": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_mcp_tool_use": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py",
        "show_output": true
      }
    ],
    "post_cascade_response": [
      {
        "command": "python3 /Users/yourname/hooks/log_input.py"
      }
    ]
  }
}
```
**Script** (`log_input.py`):

```
#!/usr/bin/env python3

import sys
import json

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()
    
    # Parse the JSON
    try:
        data = json.loads(input_data)
        
        # Write formatted JSON to file
        with open("/Users/yourname/hooks/input.txt", "a") as f:
            f.write('\n' + '='*80 + '\n')
            f.write(json.dumps(data, indent=2, separators=(',', ': ')))
            f.write('\n')
    
        print(json.dumps(data, indent=2))
    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```
This script appends every hook invocation to a log file, creating an audit trail of all Cascade actions. You may transform the input data or perform custom logic as you see fit.
### [​](#restricting-file-access)
Restricting File Access
Prevent Cascade from reading files outside a specific directory. **Config**:

```
{
  "hooks": {
    "pre_read_code": [
      {
        "command": "python3 /Users/yourname/hooks/block_read_access.py",
        "show_output": true
      }
    ]
  }
}
```
**Script** (`block_read_access.py`):

```
#!/usr/bin/env python3

import sys
import json

ALLOWED_PREFIX = "/Users/yourname/my-project/"

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "pre_read_code":
            tool_info = data.get("tool_info", {})
            file_path = tool_info.get("file_path", "")
            
            if not file_path.startswith(ALLOWED_PREFIX):
                print(f"Access denied: Cascade is only allowed to read files under {ALLOWED_PREFIX}", file=sys.stderr)
                sys.exit(2)  # Exit code 2 blocks the action
            
            print(f"Access granted: {file_path}", file=sys.stdout)

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```
When Cascade attempts to read a file outside the allowed directory, this hook blocks the operation and displays an error message.
### [​](#blocking-dangerous-commands)
Blocking Dangerous Commands
Prevent Cascade from executing potentially harmful commands. **Config**:

```
{
  "hooks": {
    "pre_run_command": [
      {
        "command": "python3 /Users/yourname/hooks/block_dangerous_commands.py",
        "show_output": true
      }
    ]
  }
}
```
**Script** (`block_dangerous_commands.py`):

```
#!/usr/bin/env python3

import sys
import json

DANGEROUS_COMMANDS = ["rm -rf", "sudo rm", "format", "del /f"]

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "pre_run_command":
            tool_info = data.get("tool_info", {})
            command = tool_info.get("command_line", "")

            for dangerous_cmd in DANGEROUS_COMMANDS:
                if dangerous_cmd in command:
                    print(f"Command blocked: '{dangerous_cmd}' is not allowed for safety reasons.", file=sys.stderr)
                    sys.exit(2)  # Exit code 2 blocks the command
            
            print(f"Command approved: {command}", file=sys.stdout)

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```
This hook scans commands for dangerous patterns and blocks them before execution.
### [​](#blocking-policy-violating-prompts)
Blocking Policy-Violating Prompts
Prevent users from submitting prompts that violate organizational policies. **Config**:

```
{
  "hooks": {
    "pre_user_prompt": [
      {
        "command": "python3 /Users/yourname/hooks/block_bad_prompts.py"
      }
    ]
  }
}
```
**Script** (`block_bad_prompts.py`):

```
#!/usr/bin/env python3

import sys
import json

BLOCKED_PATTERNS = [
    "something dangerous",
    "bypass security",
    "ignore previous instructions"
]

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "pre_user_prompt":
            tool_info = data.get("tool_info", {})
            user_prompt = tool_info.get("user_prompt", "").lower()

            for pattern in BLOCKED_PATTERNS:
                if pattern in user_prompt:
                    print(f"Prompt blocked: Contains prohibited content. The user cannot ask the agent to do bad things.", file=sys.stderr)
                    sys.exit(2)  # Exit code 2 blocks the prompt

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```
This hook examines user prompts before they are processed and blocks any that contain prohibited patterns. When a prompt is blocked, the user sees an error message in the Cascade UI.
### [​](#logging-cascade-responses)
Logging Cascade Responses
Track all Cascade responses for compliance auditing or analytics. **Config**:

```
{
  "hooks": {
    "post_cascade_response": [
      {
        "command": "python3 /Users/yourname/hooks/log_cascade_response.py"
      }
    ]
  }
}
```
**Script** (`log_cascade_response.py`):

```
#!/usr/bin/env python3

import sys
import json
from datetime import datetime

def main():
    # Read the JSON data from stdin
    input_data = sys.stdin.read()

    # Parse the JSON
    try:
        data = json.loads(input_data)

        if data.get("agent_action_name") == "post_cascade_response":
            tool_info = data.get("tool_info", {})
            cascade_response = tool_info.get("response", "")
            trajectory_id = data.get("trajectory_id", "unknown")
            timestamp = data.get("timestamp", datetime.now().isoformat())

            # Log to file
            with open("/Users/yourname/hooks/cascade_responses.log", "a") as f:
                f.write(f"\n{'='*80}\n")
                f.write(f"Timestamp: {timestamp}\n")
                f.write(f"Trajectory ID: {trajectory_id}\n")
                f.write(f"Response:\n{cascade_response}\n")

            print(f"Logged Cascade response for trajectory {trajectory_id}")

    except json.JSONDecodeError as e:
        print(f"Error parsing JSON: {e}", file=sys.stderr)
        sys.exit(1)

if __name__ == "__main__":
    main()
```
This hook logs every Cascade response to a file, creating an audit trail of all AI-generated content. You can extend this to send data to external logging systems, databases, or compliance platforms.
### [​](#running-code-formatters-after-edits)
Running Code Formatters After Edits
Automatically format code files after Cascade modifies them. **Config**:

```
{
  "hooks": {
    "post_write_code": [
      {
        "command": "bash /Users/yourname/hooks/format_code.sh",
        "show_output": false
      }
    ]
  }
}
```
**Script** (`format_code.sh`):

```
#!/bin/bash

# Read JSON from stdin
input=$(cat)

# Extract file path using jq
file_path=$(echo "$input" | jq -r '.tool_info.file_path')

# Format based on file extension
if [[ "$file_path" == *.py ]]; then
    black "$file_path" 2>&1
    echo "Formatted Python file: $file_path"
elif [[ "$file_path" == *.js ]] || [[ "$file_path" == *.ts ]]; then
    prettier --write "$file_path" 2>&1
    echo "Formatted JS/TS file: $file_path"
elif [[ "$file_path" == *.go ]]; then
    gofmt -w "$file_path" 2>&1
    echo "Formatted Go file: $file_path"
fi

exit 0
```
This hook automatically runs the appropriate formatter based on the file type after each edit.
### [​](#setting-up-worktrees)
Setting Up Worktrees
Copy environment files and install dependencies when a new worktree is created. **Config** (in `.windsurf/hooks.json`):

```
{
  "hooks": {
    "post_setup_worktree": [
      {
        "command": "bash $ROOT_WORKSPACE_PATH/hooks/setup_worktree.sh",
        "show_output": true
      }
    ]
  }
}
```
**Script** (`hooks/setup_worktree.sh`):

```
#!/bin/bash

# Copy environment files from the original workspace
if [ -f "$ROOT_WORKSPACE_PATH/.env" ]; then
    cp "$ROOT_WORKSPACE_PATH/.env" .env
    echo "Copied .env file"
fi

if [ -f "$ROOT_WORKSPACE_PATH/.env.local" ]; then
    cp "$ROOT_WORKSPACE_PATH/.env.local" .env.local
    echo "Copied .env.local file"
fi

# Install dependencies
if [ -f "package.json" ]; then
    npm install
    echo "Installed npm dependencies"
fi

exit 0
```
This hook ensures each worktree has the necessary environment configuration and dependencies installed automatically.
## [​](#best-practices)
Best Practices

### [​](#security)
Security

**Use Cascade Hooks at Your Own Risk**: Hooks execute shell commands automatically with your user account’s full permissions. You are entirely responsible for the code you configure. Poorly designed or malicious hooks can modify files, delete data, expose credentials, or compromise your system.

* **Validate all inputs**: Never trust the input JSON without validation, especially for file paths and commands.
* **Use absolute paths**: Always use absolute paths in your hook configurations to avoid ambiguity.
* **Protect sensitive data**: Avoid logging sensitive information like API keys or credentials.
* **Review permissions**: Ensure your hook scripts have appropriate file system permissions.
* **Audit before deployment**: Review every hook command and script before adding to your configuration.
* **Test in isolation**: Run hooks in a test environment before enabling them on your primary development machine.

### [​](#performance-considerations)
Performance Considerations

* **Keep hooks fast**: Slow hooks will impact Cascade’s responsiveness. Aim for sub-100ms execution times.
* **Use async operations**: For non-blocking hooks, consider logging to a queue or database asynchronously.
* **Filter early**: Check the action type at the start of your script to avoid unnecessary processing.

### [​](#error-handling)
Error Handling

* **Always validate JSON**: Use try-catch blocks to handle malformed input gracefully.
* **Log errors properly**: Write errors to `stderr` so they’re visible when `show_output` is enabled.
* **Fail safely**: If your hook encounters an error, consider whether it should block the action or allow it to proceed.

### [​](#testing-your-hooks)
Testing Your Hooks

1. **Start with logging**: Begin by implementing a simple logging hook to understand the data flow.
2. **Use `show_output: true`**: Enable output during development to see what your hooks are doing.
3. **Test blocking behavior**: Verify that exit code 2 properly blocks actions in pre-hooks.
4. **Check all code paths**: Test both success and failure scenarios in your scripts.

## [​](#enterprise-distribution)
Enterprise Distribution
Enterprise organizations need to enforce security policies, compliance requirements, and development standards that individual users cannot bypass. Cascade Hooks supports this through **system-level configuration**, which takes precedence and cannot be disabled by end users without root permissions. Deploy your mandatory `hooks.json` configuration to these OS-specific locations: **macOS:**

```
/Library/Application Support/Windsurf/hooks.json
```
**Linux/WSL:**

```
/etc/windsurf/hooks.json
```
**Windows:**

```
C:\ProgramData\Windsurf\hooks.json
```
Place your hook scripts in a corresponding system directory (e.g., `/usr/local/share/windsurf-hooks/` on Unix systems).
### [​](#deployment-methods)
Deployment Methods
Enterprise IT teams can deploy system-level hooks using standard tools and workflows: **Mobile Device Management (MDM)**

* **Jamf Pro** (macOS) - Deploy via configuration profiles or scripts
* **Microsoft Intune** (Windows/macOS) - Use PowerShell scripts or policy deployment
* **Workspace ONE**, **Google Endpoint Management**, and other MDM solutions
**Configuration Management**

* **Ansible**, **Puppet**, **Chef**, **SaltStack** - Use your existing infrastructure automation
* **Custom deployment scripts** - Shell scripts, PowerShell, or your preferred tooling

### [​](#verification-and-auditing)
Verification and Auditing
After deployment, verify that hooks are properly installed and cannot be bypassed:

```
# Verify system hooks are present
ls -la /etc/windsurf/hooks.json  # Linux
ls -la "/Library/Application Support/Windsurf/hooks.json"  # macOS

# Test hook execution (should see hook output in Cascade)
# Have a developer trigger the relevant Cascade action

# Verify users cannot modify system hooks
sudo chown root:root /etc/windsurf/hooks.json
sudo chmod 644 /etc/windsurf/hooks.json
```

**Important**: System-level hooks are entirely managed by your IT or security team. Windsurf does not deploy or manage files at system-level paths. Ensure your internal teams handle deployment, updates, and compliance according to your organization’s policies.

### [​](#workspace-hooks-for-team-projects)
Workspace Hooks for Team Projects
For project-specific conventions, teams can use workspace-level hooks in version control:

```
# Add to your repository
.windsurf/
├── hooks.json
└── scripts/
    └── format-check.py

# Commit to git
git add .windsurf/
git commit -m "Add workspace hooks for code formatting"
```
This allows teams to standardize development practices. Be sure to keep security-critical policies at the system level, and be sure not to check in any sensitive information to version control.
## [​](#additional-resources)
Additional Resources

* **MCP Integration**: Learn more about [Model Context Protocol in Windsurf](/windsurf/cascade/mcp)
* **Workflows**: Discover how to combine hooks with [Cascade Workflows](/windsurf/cascade/workflows)
* **Analytics**: Track Cascade usage with [Team Analytics](/windsurf/accounts/analytics)

[Model Context Protocol (MCP)](/windsurf/cascade/mcp)[Usage](/windsurf/accounts/usage)

⌘I
[twitter](https://x.com/windsurf)[discord](https://discord.com/invite/3XFf78nAx5)

[Powered by](https://www.mintlify.com?utm_campaign=poweredBy&utm_medium=referral&utm_source=codeium)
