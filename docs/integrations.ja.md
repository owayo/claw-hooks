# エージェント統合

各エージェントのフック設定ファイルに `claw-hooks hook` を登録します。このページでは、エージェントごとに設定ファイルの場所、登録するイベント、エージェントへ返せるものと返せないものを説明します。claw-hooks が各エージェントのペイロードをどう読み、何を返すかは [CLI リファレンス](cli-reference.ja.md) にあります。

## Claude Code

`~/.claude/settings.json`（ユーザー）または`.claude/settings.json`（プロジェクト）に追加:

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

`<project>/.cursor/hooks.json`（プロジェクト）または `~/.cursor/hooks.json`（ユーザー）に追加:

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

> **コマンドブロック用フックには `failClosed: true` を推奨します。** Cursor は既定でフェイルオープンです。正常なブロック（exit `0` + stdout の `{"permission":"deny", …}`）は `failClosed` なしでも機能しますが、claw-hooks 自体がクラッシュ・タイムアウトした場合、`failClosed: true` を設定していないと Cursor はコマンドを通してしまいます。`afterFileEdit`/`stop` では付けません（フォーマッター/lint のクラッシュでエージェントを止めるべきではないため）。

> **`preToolUse` には matcher を付けたままにしてください。** このフックは*すべての*ツールで発火するため、matcher が無いと巨大な `Write` も claw-hooks に届きます。stdin の 4 MiB 上限を超えるとパースできなくなり、`preToolUse` は実行前ゲートなのでフェイルクローズドの deny になって、claw-hooks が何の意見も持たないファイル書き込みを止めてしまいます。Cursor の matcher は正規表現なので、上の例はあえて広めにしてあります。誤って一致しても無害で（`tool_input.command` を持たない入力は従来どおりパススルーされます）、取りこぼしはツール名ではなくイベント単位でシェルを捉える `beforeShellExecution` が二重に受けます。

> **停止時 lint を使うならプロジェクトフックに置いてください。** Cursor はプロジェクトフック（`<project>/.cursor/hooks.json`）をプロジェクトルートで、ユーザーフック（`~/.cursor/hooks.json`）を `~/.cursor/` で実行します。claw-hooks の `condition = { file_exists = "Cargo.toml" }` による判定、`.claw-hooks.toml` の探索、各フックの作業ディレクトリはいずれもそのディレクトリを基準にするため、ユーザーレベルに登録するとプロジェクト種別の条件が無言で不成立になります。条件なしのフック（`git-sc` の自動コミット等）はリポジトリではなく Cursor の設定ディレクトリで走ります。

> **保存後の診断は Cursor へ返せません。** `afterFileEdit` には出力スキーマが定義されていないため、フォーマッターによるファイル書き換えは反映されますが、linter のテキストを渡す先がありません。診断が必要な場合はプロジェクト全体の lint を `stop` フックで回してください（`followup_message` 経由で返ります）。

## Windsurf (Cascade)

`~/.codeium/windsurf/hooks.json`（ユーザー）または`.windsurf/hooks.json`（プロジェクト）に追加:

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

`~/.gemini/config/hooks.json`（ユーザー）または `<project>/.agents/hooks.json`（プロジェクトワークスペース）に追加:

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

注意点:
- **Antigravity では `--event` を指定してください。** Antigravity のペイロードにはイベント名フィールドが無く、`PreToolUse` と `PostToolUse` は形状で区別できません（どちらも `toolCall` と `stepIdx` を持ち、差は Optional な `error` のみ）。`hooks.json` はイベントごとに別エントリで登録するため、`--event` でどちらかを伝えます。未指定の場合は形状から推定し、区別できないケースは `PreToolUse` に倒します（コマンドブロックは維持されますが、保存後フックは動作しません）。
- `--event PostToolUse` を指定すると Antigravity でも拡張子フックが動作します。編集対象は `toolCall.args.TargetFile` から復元します。ただし公式仕様で `PostToolUse` の出力は `{}` 固定のため、formatter/linter は**実行されますが診断結果をエージェントへ返せません**。診断を伝えたい場合は従来どおり Stop hooks でプロジェクト全体の lint/typecheck を回し、失敗を `{"decision":"continue","reason":"..."}` で再投入してください。
- **matcher は `run_command` に加えて `manage_task` も対象にします。** `manage_task` は `Action: "send_input"` のとき `Input` を実行中プロセスの標準入力へ書き込みます。`run_command` + `RunPersistent: true` で永続シェルを起動すれば、以降のコマンドは `CommandLine` を一度も通らずに `send_input` から届くため、`manage_task` を matcher から外すと rm/kill/dd フィルターを完全に迂回できてしまいます。それ以外のアクション（`list` / `status` / `kill`）はエージェント自身のバックグラウンドタスク管理（シェルの `kill` コマンドとは別物）なのでパススルーします。
- Antigravity には `stop_hook_active`（Claude/Codex）や `loop_count`（Cursor）に相当する入力がありません（`executionNum` は実行試行の連番で、通常の初回停止でも `1` です）。そのため恒久的に失敗する stop hook によるループを claw-hooks 側では遮断できません。report=true の stop hook には自己完結する終了条件を持たせてください。
- `PreInvocation` / `PostInvocation` は claw-hooks のスコープ外（モデル呼び出し前後のオーケストレーション）なので、自動的にパススルーされます。これらのイベントは hook 登録不要です。
- Antigravity hooks 公式仕様: <https://antigravity.google/docs/customizations/hooks>

## Codex CLI

`~/.codex/hooks.json`（ユーザー）に追加:

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

Codex hooks はデフォルトで有効です。明示的に機能フラグを設定する場合は、現行の `[features] hooks` キーを使用してください。旧 `codex_hooks` エイリアスは非推奨です。

## Grok CLI

`~/.grok/hooks/`（個人）または `<project>/.grok/hooks/`（プロジェクト）配下に JSON ファイルを追加:

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

注意点:
- `matcher` はツール名に対する正規表現で、省略すると全ツールにマッチします。Grok は `Bash` / `Edit` のような Claude 形式のツール名を自前のツール名へ自動マッピングしますが、マッピング後の名前は公開されていないため、`matcher` を省略する方が安全です。claw-hooks はペイロードの形から処理対象を判断し、対象外はすべてパススルーします（[フォーマット検出ロジック](cli-reference.ja.md#フォーマット検出ロジック) を参照）。
- `timeout` の単位は**秒**で、デフォルトは `5` です。フォーマッターやプロジェクト全体 lint には短いため、上記のように延ばしてください。
- プロジェクトのフックはリポジトリを信頼するまで実行されません。`/hooks-trust` を一度実行するか、`--trust` 付きで Grok を起動してください。
- Grok は Claude Code（`.claude/settings.json`）と Cursor（`.cursor/hooks.json`）のフック設定も読み込みます。すでにそちらへ claw-hooks を登録している場合は、1 イベントにつき二重実行にならないよう登録を 1 か所にまとめてください。
- claw-hooks はツール名ではなく `toolInput` の**形**で判定します。`command` フィールドがあればシェルコマンド、`file_path` / `filePath` / `notebook_path` / `notebookPath` があればファイル編集、どちらも無ければパススルーです。`toolName` と `toolInput` を必須にしないのも同じ理由で、判定に使わないフィールドを必須化すると無関係なツール呼び出しまで拒否されます（引数を持たないツールは `toolInput` をキーごと送りません）。
- Grok がブロックできるのは `PreToolUse` だけです。それ以外はすべて stdout が無視される事後フックなので、拡張子フックによるファイル整形も Stop hooks の lint も実行はされますが、その出力をエージェントへ返すことはできません（Windsurf の `post_cascade_response` と同じ制約です）。
- Grok は明示的な拒否以外すべてフェイルオープンです。タイムアウト・クラッシュ・不正出力はフック失敗として記録され、ツール呼び出しはそのまま実行されます。そのため claw-hooks はブロック時に deny JSON **と** exit code `2` の両方を返し、フェイルクローズド経路でも（`1` ではなく）exit `2` を使うことで、どちらの解釈でもブロックが成立するようにしています。
