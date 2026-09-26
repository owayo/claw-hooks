# CLI リファレンス

claw-hooks は stdin からフックイベントを 1 件読み、呼び出し元のエージェント固有の形式で応答します。このページでは、サブコマンドとオプション、`--format` ごとのペイロードの読み方、イベントごとの出力、終了コード、フェイルクローズドの規則、コマンドフックの判定器とのやり取りを説明します。各エージェントへの登録方法は [エージェント統合](integrations.ja.md) にあります。

## コマンド

| コマンド | 説明 |
|---------|------|
| `hook` (別名: `run`) | stdinからフックイベントを処理 |
| `init` | デフォルト設定を生成 |
| `check` | 設定を検証 |
| `version` | バージョンを表示 |

## オプション

| オプション | 短縮形 | 説明 |
|-----------|--------|------|
| `--format` | `-f` | 入力形式: `claude` (デフォルト), `cursor`, `windsurf`, `agy` (Antigravity CLI), `codex`, `grok` (Grok CLI) |
| `--event` | `-e` | フックイベント名（例: `PostToolUse`）。ペイロードにイベント名フィールドが無く `PreToolUse` / `PostToolUse` の形状が同一な Antigravity CLI 用。他エージェントでは不要 |
| `--config` | `-c` | 設定ファイルのパス |
| `--trace` | `-t` | トレースモード: 生の入力・パース結果・出力を stderr に出力（ディスクには保存しない） |
| `--path` | `-p` | `init` 専用: 設定ファイルの作成先（既定は `~/.config/claw-hooks/config.toml`） |
| `--debug` | — | この実行だけデバッグログを有効にする（設定の `debug = true` と同じ） |
| `--quiet` | `-q` | `init` と `check` の状況メッセージを出さない |
| `--help` | `-h` | ヘルプを表示 |
| `--version` | `-V` | バージョンを表示 |

## 例

```bash
# Claude Codeフックを処理（デフォルト）
claw-hooks hook

# Cursorフックを処理
claw-hooks hook --format cursor

# Windsurfフックを処理
claw-hooks hook --format windsurf

# Antigravity CLIフックを処理（ペイロードにイベント名が無いため --event を指定する）
claw-hooks hook --format agy --event PreToolUse
claw-hooks hook --format agy --event PostToolUse

# Codex CLIフックを処理
claw-hooks hook --format codex

# Grok CLIフックを処理
claw-hooks hook --format grok

# カスタム設定を使用
claw-hooks hook --config /path/to/config.toml
```

## フォーマット検出ロジック

各AIエージェントは異なるJSON構造を送信します。claw-hooksは`--format`を使用してパース方法を決定します。

### Claude Code (`--format claude`)

Claude Code公式フック仕様を使用:

```jsonc
// PreToolUse/PostToolUseイベント
{
  "hook_event_name": "PreToolUse",
  "tool_name": "Bash",
  "tool_input": { "command": "..." },
  "session_id": "...",
  "cwd": "/path/to/project"
}

// Stopイベント（tool_name/tool_inputなし）
{
  "hook_event_name": "Stop",
  "stop_hook_active": true,
  "session_id": "..."
}
```

処理対象のフックイベント: `PreToolUse`、`PostToolUse`、`Stop`、`SubagentStart`、`SubagentStop`。claw-hooks のスコープ外である既知のライフサイクルイベント（`Notification`、`PermissionRequest`、`UserPromptSubmit`、`SessionStart`、`SessionEnd` 等）は判定を返さずパススルーします。

Claude の `Stop` では `stop_hook_active` が必須です。欠落または型不正なら壊れたペイロードとして扱い、stop hook を実行せずセッションの停止を許可します（`{}` + exit `0`）。読み取れないガードを `false` とみなすと stop hook が実行され、報告対象フックの失敗によって `Stop` が無限に再発火し得るためです。

### Cursor (`--format cursor`)

`hook_event_name` フィールドでイベントを判定します:

| `hook_event_name` | 必須フィールド | 内部マッピング |
|-------------------|----------------|----------------|
| `preToolUse`（全ツール） | なし（`tool_input.command` があればコマンド経路、無ければパススルー） | PreToolUse + Bash |
| `beforeShellExecution` | `command` | PreToolUse + Bash |
| `afterFileEdit` / `afterTabFileEdit` | `file_path` / `filePath` | PostToolUse + Write |
| `stop` | なし（`loop_count` は存在すれば読む） | Stop |

Shell 以外の `preToolUse` を含む未対応の Cursor イベントは、`{"permission":"allow"}` ではなく空オブジェクト（`{}`）として透過されます。Cursor は複数ソースのフック応答をマージし、優先度の高い `allow` が他フックの `deny` を上書きし得るため、claw-hooks は中身を検査していないイベント（`beforeReadFile`、`beforeMCPExecution`、`beforeTabFileRead`、`sessionStart`、`postToolUse` 等）に対して許可を表明しません。許可したコマンドで `{}` を返すのも同じ理由です。

ブロックは stdout の `{"permission":"deny", …}` + exit code `0` で返します。Cursor は exit `0` のときだけ stdout の JSON を解釈するため、exit `2` で終了すると「safe-rm を使ってください」という代替案を運ぶ `user_message` が破棄されてしまいます。Claude Code は異なり、現行仕様では全終了コードで有効な stdout JSON を読みますが、exit `2` のブロック効果は上書きできません。

`stop` では Cursor の `loop_count` フィールド（stop hook が自動フォローアップを発火した回数、0 始まり）をループ防止に使用します。1 以上の場合は全 stop hook をスキップします — Claude Code の `stop_hook_active` と同じ役割で、lint 失敗のフィードバックは Cursor の `loop_limit` までループせず 1 回だけエージェントに返ります。

不正な `stop` ペイロードは、フェイルクローズドにせず停止を許可します（`{}` + exit `0`）。`followup_message` は次のユーザーメッセージとして自動送信されるため、パースできなかったペイロードに対してこれを返すと同じ失敗が延々と再発火します。詳細は [フェイルクローズド動作](#フェイルクローズド動作) を参照してください。

### Windsurf (`--format windsurf`)

`agent_action_name`フィールドを使用:

| agent_action_name | 内部マッピング |
|-------------------|----------------|
| `pre_run_command` | PreToolUse + Bash |
| `post_write_code` | PostToolUse + Write |
| `post_cascade_response` | Stop |

未対応の Windsurf アクションは `allow` として透過されます。

### Antigravity CLI (`--format agy`)

camelCase スキーマ。代表的な PreToolUse ペイロード:

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

Antigravity の公式ペイロードにはイベント名フィールドが無く、さらに `PreToolUse` と `PostToolUse` は**形状が同一**です（どちらも `toolCall` と `stepIdx` を持ち、差は Optional な `error` のみ）。どちらのイベントかは `--event <name>` で指定してください。`hooks.json` はイベントごとに別エントリで登録するため、呼び出し側は必ず知っています。判別順は `--event` → 空白でない `hook_event_name` / `event`（旧版互換）→ 形状推定（`toolCall` は PreToolUse、Stop 固有フィールドは Stop、invocation 固有フィールドは Pre/PostInvocation）です。推定では PreToolUse / PostToolUse を区別できないため **PreToolUse に倒し**、コマンドブロックを維持します（逆に倒すと未実行のコマンドを素通しします）。`error` は判別条件に使いません — 公式仕様で Optional（「成功時は空」）とされており、これを鍵にすると**成功した**ツール呼び出しを誤判別するためです。

必須フィールドの検証は「claw-hooks が判定に使うもの」に限定します。公式仕様で **Required** マークが付くのは Stop の `fullyIdle`（と出力の `decision`）だけなので、`stepIdx` / `executionNum` / `terminationReason` はいずれも任意扱いです。これらを必須にすると、`run_command` すべてに deny（Antigravity では「即時ハードブロック」）が返り、Stop では**全 stop hook が黙ってスキップ**されます。Stop のパースエラーは `{"decision":"stop"}` + exit 0 に倒し、必須の出力スキーマを満たしながら再投入ループを起こす `continue` を避けます。`toolCall.args` を必須とするのは `run_command` のときだけです（公式仕様は引数を持たないツールを列挙し、`matcher: ""` / `"*"` も認めているため）。必須フィールドの欠落・空白・型不正は、判別したイベント固有の応答でフェイルクローズドになります。

| 判別に使うイベント形状 | toolCall.name | 内部マッピング |
|---|---|---|
| `toolCall` + `stepIdx`（PreToolUse） | `run_command` | BeforeCommand（`toolCall.args.CommandLine` → Bash） |
| `toolCall` + `stepIdx`（PreToolUse） | その他（`write_to_file`、`replace_file_content`、…） | パススルー allow |
| `toolCall` の無い `stepIdx`、または invocation 固有フィールド | n/a | PostToolUse / invocation のパススルー allow（claw-hooks のスコープ外） |
| `executionNum` / `terminationReason` / `fullyIdle` | n/a | Stop |

> **拡張子フック**: Antigravity の `PostToolUse` は `toolCall`（`name` と `args`）を含むため、`write_to_file` / `replace_file_content` / `multi_replace_file_content` の `args.TargetFile` から編集対象を復元できます。hook エントリに `--event PostToolUse` を付けてください（ペイロードは `PreToolUse` と形状が同一のため、未指定だと `PreToolUse` と推定され保存後フックが動きません）。公式仕様の出力は `{}` 固定なので formatter/linter は実行されますが診断は返せません。lint の本文が必要な場合は Stop hooks で回して `"decision":"continue"` で再投入してください。`run_command` の `PostToolUse` はパススルーします（コマンドは実行済みで、事後のブロックは不可能かつ無意味なため）。出力 JSON は [入出力リファレンス](#入出力リファレンス) を参照。明示名を持つ未対応イベントは allow でパススルーし、イベントを判別できない名前なしペイロードは安全な応答形式を選べないためフェイルクローズドになります。

### Codex CLI (`--format codex`)

`hook_event_name` + `tool_name` + `tool_input` の標準スキーマ。`apply_patch` の `tool_input.command` から `*** Add/Update/Move to File:` ヘッダを抽出して拡張子フックを駆動します（削除のみの patch はスキップ）。

検証対象は claw-hooks が実際に読むフィールドだけです: イベント名、`tool_name` / `tool_input`（および `Bash` の `tool_input.command`、`apply_patch` の patch 本文）、`Stop` の `stop_hook_active`。公式ドキュメントの `session_id` / `cwd` / `model` / `transcript_path` / `turn_id` / `permission_mode` は「通常使うことになる共通フィールド」の紹介であって厳密なスキーマではなく（公式の `SessionEnd` 実例ペイロードには `model` がありません）、これらを必須にすると 1 フィールドの欠落で全フック呼び出しがフェイルクローズドに倒れてしまいます。スコープ外のパススルーイベントは一切検証しません。

非ファイル系ツール（例: `Bash`）の `PostToolUse` も厳密検証せずパススルーします。Codex の `PostToolUse` では `decision:"block"` が**実際のツール出力をフックのメッセージで置き換える**動作になるため、ここでフェイルクローズドにするとモデルは本来のコマンド出力を一切見られなくなり、しかも保存後フックに必要なのはファイルパスだけなので得るものがありません。claw-hooks が実際に読むフィールドについては、欠落・型不正ならイベント固有の deny/block 応答でフェイルクローズドになります。

`Interrupt` と、`mcp__*` など claw-hooks が検査しない MCP / 関数ツールはスコープ外です。他のフックや Codex 本来の権限判断を上書きしないよう、明示的な allow ではなく中立応答 `{}` でパススルーします。

| hook_event_name | 内部マッピング |
|-----------------|------------------|
| `SessionStart` / `SessionEnd` / `UserPromptSubmit` / `PreCompact` / `PostCompact` / `Interrupt` | パススルー allow |
| `PreToolUse` | BeforeCommand |
| `PermissionRequest` | 承認プロンプト前のコマンドガード（危険な Bash は deny、安全なら `{}`） |
| `PostToolUse` | AfterFileEdit（`Bash` パススルー、`apply_patch` → MultiEdit） |
| `Stop` | Stop |

Codex は許可・ブロック・フェイルクローズドすべてを exit code `0` で返します（非ゼロはフックインフラ失敗扱い）。イベントごとの出力 JSON は [入出力リファレンス](#入出力リファレンス) を参照。

### Grok CLI (`--format grok`)

camelCase スキーマで、`hookEventName` フィールドを明示的に持ちます:

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

| hookEventName | `toolInput` の形 | 内部マッピング |
|---|---|---|
| `PreToolUse` | `command` | BeforeCommand（Grok がフックにブロックを許す唯一のイベント） |
| `PreToolUse` | ファイルパス、またはどちらも無し | パススルー allow |
| `PostToolUse` | `file_path` / `filePath` | AfterFileEdit（拡張子フック） |
| `PostToolUse` | `command`、またはどちらも無し | パススルー allow |
| `Stop` | n/a | Stop |
| `SessionStart` / `SessionEnd` / `UserPromptSubmit` / `PostToolUseFailure` / `PermissionDenied` / `StopFailure` / `Notification` / `PreCompact` / `PostCompact` | n/a | パススルー allow |

claw-hooks は **`toolName` ではなく `toolInput` の形**で処理を振り分けます。Grok は `Bash` / `Edit` のような Claude のツール名を自前のツール名へマッピングすると明記していますが、マッピング後の名前は公開仕様に列挙されていないため、名前で判定すると想定外の名前のシェル実行ツールがコマンドフィルターを素通りしてしまいます。そこで `command` を持つペイロードはコマンドフィルターへ、`file_path` / `filePath` を持つペイロードは拡張子フックへ回し、それ以外はパススルーします。`toolName` と `toolInput` はどちらも**任意**です。理由は同じで、判定に使わないフィールドを必須化すると無関係なツール呼び出しまで拒否してしまうためです（引数を持たないツールは `toolInput` をキーごと送りません）。Grok では `PreToolUse` が唯一のハードブロック経路なので、ここでの誤 deny は影響が大きくなります。Grok は Claude Code / Cursor のフック設定も読み込むため、snake_case のキー（`hook_event_name`、`session_id`、`tool_name`、`tool_input`）も受理します。

Grok の契約はフェイルオープンです: exit `0` は許可、exit `2` は拒否、それ以外の結末（タイムアウト・クラッシュ・不正な stdout）は失敗として記録されるだけでツール呼び出しは続行されます。そのため claw-hooks はブロック時に deny JSON **と** exit code `2` の両方を返してどちらの解釈でも判定が成立するようにし、フェイルクローズド経路でも exit `1` は使いません。許可時は `allow` 判定ではなく `{}` を返します（公式に文書化された `decision` の値は `deny` のみのため）。

### イベントマッピング

```mermaid
graph LR
    subgraph コマンド実行前
        CC1[Claude: PreToolUse + Bash]
        CU1[Cursor: preToolUse Shell / beforeShellExecution]
        WS1[Windsurf: pre_run_command]
        AG1[Antigravity: PreToolUse + run_command]
        CX1[Codex: PreToolUse + Bash]
        GR1[Grok: PreToolUse + command]
    end
    CH1[🛡️ 検証・代替ツール提案]
    CC1 --> CH1
    CU1 --> CH1
    WS1 --> CH1
    AG1 --> CH1
    CX1 --> CH1
    GR1 --> CH1

    subgraph ファイル保存後
        CC2[Claude: PostToolUse + Write/Edit]
        CU2[Cursor: afterFileEdit]
        WS2[Windsurf: post_write_code]
        CX2[Codex: PostToolUse + apply_patch]
        GR2[Grok: PostToolUse + file path]
    end
    CH2["🔧 拡張子ごとのコマンド実行<br>その後に * のコマンド（全ファイル）"]
    CC2 --> CH2
    CU2 --> CH2
    WS2 --> CH2
    CX2 --> CH2
    GR2 --> CH2

    subgraph エージェント終了
        CC3[Claude: Stop]
        CU3[Cursor: stop]
        WS3[Windsurf: post_cascade_response]
        AG3[Antigravity: Stop]
        CX3[Codex: Stop]
        GR3[Grok: Stop]
    end
    CH3[⏹️ Lint / 通知 / クリーンアップ]
    CC3 --> CH3
    CU3 --> CH3
    WS3 --> CH3
    AG3 --> CH3
    CX3 --> CH3
    GR3 --> CH3
```

Codex の `PostToolUse` + `Bash` はコマンド出力フィードバックのため、「ファイル保存後」フローには含めません。ファイル書き込みイベントとして扱うのは `apply_patch` のみです。Antigravity CLI は `--event PostToolUse` を指定した hook エントリでのみ「ファイル保存後」グループに入ります（`toolCall.args.TargetFile` から編集対象を復元）。ただし出力は `{}` 固定で診断を返せないため、lint の本文が必要な場合は Stop hooks でプロジェクト全体の lint/typecheck を回してください。Grok CLI は 3 つのグループすべてに登場しますが、ブロックできるのは `PreToolUse` だけです。残り 2 つは Grok が出力を無視する事後フックのため、処理自体は実行されてもフィードバックは返りません。

## 入出力リファレンス

Stdin はエージェント固有のフック JSON（イベント別のペイロードは [フォーマット検出ロジック](#フォーマット検出ロジック) を参照）。Stdout / stderr は `(format, event)` ごとに以下の JSON を返します。

| エージェント | イベント | 許可 | ブロック / フェイルクローズド |
|---|---|---|---|
| Claude Code | PreToolUse | `{}`（判定を返さず、通常の権限フローに委ねる）。コマンドフックが補足を返したときは、判定を付けずに `…additionalContext:"…"` | `…permissionDecision:"deny", permissionDecisionReason:"…"`（exit 0）。パースエラー時は **stderr** にプレーンテキスト、exit 2 |
| Claude Code | PostToolUse | `{}` または `…additionalContext:"…"`（lint フィードバック） | `{"decision":"block","reason":"…"}` |
| Claude Code | Stop | `{}` | `{"decision":"block","reason":"…"}` |
| Cursor | preToolUse / beforeShellExecution | `{}` | `{"permission":"deny","user_message":"…","agent_message":"…"}`（exit 0 — Cursor は exit 0 のときだけ stdout の JSON を読む） |
| Cursor | stop | `{}` | `{"followup_message":"…"}` |
| Windsurf | pre_run_command | `{}` | exit code 2 + **stderr** プレーンテキスト（JSON ではない） |
| Windsurf | post_write_code | `{}`（指摘なし） | exit code 2 + **stderr** プレーンテキスト（lint の指摘。事後フックはブロックできないため、編集はそのまま残る） |
| Windsurf | post_cascade_response | `{}` | `{}`（非同期事後フックのためブロック不可） |
| Antigravity | PreToolUse | `{"decision":"allow"}` | `{"decision":"deny","reason":"…"}` |
| Antigravity | PostToolUse / PreInvocation / PostInvocation | `{}` | `{}`（仕様上ブロックパス無し） |
| Antigravity | Stop | `{"decision":"stop"}` | `{"decision":"continue","reason":"…"}`（エージェントループへ再投入、`reason` が system message として注入される） |
| Codex CLI | 任意 | `{}` または `…additionalContext:"…"` | PreToolUse: `…permissionDecision:"deny",…`。PermissionRequest: `…decision:{behavior:"deny",message:"…"}`。PostToolUse / Stop: `{"decision":"block","reason":"…"}` |
| Grok CLI | PreToolUse | `{}` | `{"decision":"deny","reason":"…"}` **と** exit 2 |
| Grok CLI | PostToolUse / Stop / その他のイベント | `{}` | `{}`（事後フックの stdout は無視されるためブロック不可） |

`additionalContext` は、Claude と Codex の `PostToolUse` には lint フィードバックを、Claude と Codex の `PreToolUse` にはコマンドフックの補足を送るチャネルです。Antigravity には `additionalContext` チャネルが無いため、Stop の `"decision":"continue"` で lint フィードバックを送ります。Grok CLI の事後フックには送る手段自体が無く、ツールは実行されても出力はトランスクリプトに残りません。

claw-hooks は Claude Code / Cursor / Grok CLI に対して `allow` 判定を返しません。`{}` + exit `0` は「claw-hooks としては異議なし」を意味し、実際の可否はエージェント本来の権限プロンプト・権限ルールが決めます。Antigravity のイベントスキーマは明示的な判定が必須で、安全な `PreToolUse` は `"allow"`、停止を許可する Stop は再投入しない値 `"stop"` を返します。

### 終了コード

| エージェント | 許可 | ブロック | フェイルクローズドのパースエラー |
|---|---|---|---|
| Claude Code | `0`（stdout JSON で判定） | `0`（stdout JSON で判定） | `2` + **stderr** プレーンテキスト |
| Cursor | `0` | `0`（stdout の deny JSON。exit `2` だとメッセージが破棄される） | `2` |
| Windsurf | `0` | `2`（BeforeCommand は stderr にプレーンテキストを書き込み、AfterFileEdit も同じ経路で lint 診断をブロックせずに伝え、Stop は `0` のまま） | `2`（`pre_run_command` のみ。事後フックは `{}` + `0`） |
| Antigravity CLI | `0`（stdout JSON で判定） | `0`（stdout JSON で判定） | `0` + イベント固有の deny JSON |
| Codex CLI | `0`（stdout JSON で判定） | `0`（stdout JSON で判定） | `0` + イベント固有の deny/block JSON（非ゼロはフックインフラ失敗扱いで判定が無視される） |
| Grok CLI | `0` | `2` + stdout の deny JSON（PreToolUse のみ。他のイベントは `0`） | `2`（`1` は使わない — Grok は `2` 以外をすべてフェイルオープン扱いにするため） |

「フェイルクローズドのパースエラー」列が当てはまるのは実行前ゲートだけです。それ以外のイベント（Stop 系、保存後フック、claw-hooks がパススルーするライフサイクル系）は、上記の拒否ではなく中立の `{}` + exit `0` を返します（次節を参照）。

### フェイルクローズド動作

**実行前ゲートはフェイルクローズドです。** ペイロードをパースできない、stdin が空または上限超過、claw-hooks が実際に読むフィールドが欠けている、といった場合、コマンドブロック系イベント（`PreToolUse`、`beforeShellExecution`、`pre_run_command`、`PermissionRequest`）はエージェント固有の拒否応答を返します。フックが壊れても、それが黙認（許可）に化けることはありません。

**設定の破損時も、保護を無効化せず拒否します。** TOML 設定の読み込み・検証に失敗した場合も同じ拒否応答を返し、診断は stderr に出して `claw-hooks check` の実行を案内します。exit `1` + stdout 空では終了しません（Codex CLI / Antigravity CLI はこれを「フック失敗＝判定を無視」と解釈するため、そうすると `config.toml` のタイポ 1 つでコマンドブロックが丸ごと無効になります）。ロギングはセキュリティ制御ではなく診断機能なので、ログの初期化に失敗しても警告を出すだけでログ無しのまま処理を続行します。

**Stop 系イベントは逆に許可します。** Stop イベントにおける「ブロック」は拒否ではなく *停止せずに新しいプロンプトを渡す* 指示です（Claude Code / Codex CLI は `decision:"block"`、Antigravity CLI は `decision:"continue"`、Cursor の `followup_message` は次のユーザーメッセージとして自動送信される）。壊れたペイロードや設定エラーに対してこれを返すと、失敗 → 継続 → `Stop` 再発火 → 同じ失敗、という自己維持ループになります。ループ防止層（`stop_hook_active` / `loop_count` / `CLAW_HOOKS_STOP_ACTIVE`）はいずれもパース成功後にしか働かないため、この循環を断てません。Stop は危険操作の実行前ゲートではないので、イベント固有の停止許可 + exit `0`（Antigravity は `{"decision":"stop"}`、他エージェントは `{}`）を返します。これは新しい副作用を発生させず、自動継続による追加のツール実行を避けます。

**claw-hooks が中身を見ないイベントも許可します。** 検査していないイベントを拒否しても安全性は 1 ミリも上がらず、害だけが残ります。`UserPromptSubmit` の拒否は**ユーザーのプロンプト自体を消去**し、Codex の `PostToolUse` の拒否は**実際のツール出力をフックのメッセージで置き換え**ます。Cursor の `beforeReadFile` はファイル全文を入力に含むため、大きなファイルでは容易に stdin の 4 MiB 上限を超え、ファイル読み取りに何の意見も持たない claw-hooks がその読み取りを止めてしまいます。Windsurf の事後フックと Grok の `PreToolUse` 以外のイベントはそもそもブロック不可なので、拒否は無用なエラーを注入するだけです。これらはすべて `{}` + exit `0` を返します。

**イベントを特定できないペイロードはブロックを維持します。** 上記の判断はイベント名を基準にしています。ペイロードの破損が激しくイベント名すら復元できない場合は拒否応答に倒すため、切り詰められた / 上限を超えた `PreToolUse` はブロックされます。

**コマンドフックの判定器は `on_error` に従います。** 判定器のクラッシュ・時間切れ・起動失敗には上記の規則を当てはめず、そのフックの `on_error` で扱いを決めます。既定ではコマンドを通します。詳しくは [コマンドフックのプロトコル](#コマンドフックのプロトコル) を参照してください。

## コマンドフックのプロトコル

コマンドフック（`[[command_hooks]]`、[設定](configuration.ja.md#コマンドフック) を参照）は、シェルコマンドの中で一致した呼び出し 1 つにつき 1 回、判定器を起動します。この節では claw-hooks と判定器のあいだの取り決めを説明します。

### 判定器が走る場面

判定器が走るのは、シェルツール（`Bash` / `PowerShell`）の呼び出しのうち、[イベントマッピング](#イベントマッピング) の「コマンド実行前」グループのイベントと、Codex CLI の `PermissionRequest` だけです。組み込みの `rm` / `kill` / `dd` フィルターとカスタムフィルターの後に走るので、それらが拒否したコマンドは判定器に届きません。

呼び出しはコマンド内の出現順に調べ、同じ呼び出しに複数のフックが一致したときは設定の順に走らせます。同じフックに同じ入力を渡す判定は 1 回しか走らせません。最初に拒否が出た時点で打ち切り、残りの判定器は走らせず、それまでに集めた補足も捨てます。

ラッパー（`sudo`、`env`、`timeout` など）の後ろにある呼び出しは、独立した呼び出しとして扱います。シェルに渡す文字列（`bash -c`、`eval`、`env -S`、`trap`）と、シェルに流し込むヒアドキュメントや here-string は中身を解析し直し、`xargs` / `find -exec` が起動する呼び出しも取り出します。たとえば `sudo gws docs …` からは、`sudo` と `gws` の 2 つの呼び出しが得られます。

### 入力

claw-hooks は判定器の stdin に 1 行の JSON を書き込んでから閉じます。

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

| フィールド | 意味 |
|---|---|
| `version` | 常に `1` |
| `agent` | イベントを送ったエージェント。`claude-code`、`codex`、`cursor`、`windsurf`、`antigravity`、`grok` のいずれか |
| `event` | claw-hooks 側でそろえたイベント名。各エージェントのコマンド実行前のイベント（Cursor の `beforeShellExecution` や Windsurf の `pre_run_command` も含む）は `PreToolUse`、Codex CLI の `PermissionRequest` は `PermissionRequest` |
| `tool_name` | `Bash` または `PowerShell` |
| `session_id` | エージェントのセッション ID。無ければ `null` |
| `cwd` | エージェントが報告した、コマンドを実行するディレクトリ。報告が無ければ claw-hooks 自身の作業ディレクトリ、それも取得できなければ `null` |
| `analysis` | `complete` または `uncertain`（後述） |
| `context_delivery` | exit `0` の stdout がエージェントへ届くかどうか。Claude Code と Codex CLI の `PreToolUse` では `true`、それ以外では `false`。判定器は、捨てられるだけの補足を作らずに済ませるのに使える |
| `argv` | 呼び出しを構成する語。先頭がプログラム名で、各要素は `value`・`static`・`cardinality` を持つ |
| `stdin` | 呼び出しが標準入力から読むもの（後述） |

コマンド文字列そのものは渡しません。一致した呼び出し以外の部分には、秘密の値や、調べるプログラムと関係の無い文章が含まれ得るからです。判定器に見せるのは一致した呼び出しだけです。

Codex CLI で `PreToolUse` と `PermissionRequest` の両方を登録していると、承認を求めるコマンドは 2 つのイベントのそれぞれで判定器に届きます。どちらのイベントかは `event` で見分けられます。

**`argv` の要素。** `value` は、クォート除去後の実行時の値を claw-hooks が静的に決められればその文字列で、`$VAR` や `$(date)` のように決められなければ `null` です。`static` は `value` が `null` でないときだけ `true` になります。`cardinality` は、実行時にちょうど 1 個の引数になる要素なら `"one"`、0 個を含む任意の個数の引数に展開され得る要素なら `"zero_or_more"` です。クォートしていない展開、グロブ、ブレース展開、`xargs` が付け足す引数が後者に当たります。`zero_or_more` の要素より後ろにある引数は、実行時の位置が定まりません。

判定器を書くときは、次の 2 つを知っておくと役に立ちます。

- ダブルクォートの中の `$(cat <<'EOF' … EOF)`（引数を持たない `cat` がヒアドキュメントを 1 つ読むもの）は静的です。値はヒアドキュメントの本文で、コマンド置換と同じく末尾の改行を取り除きます。`gws … --json "$(cat <<'EOF' … EOF)"` という書き方がこれに当たります。
- `xargs` の内側の呼び出しでは、`xargs` が付け足す引数を末尾の `zero_or_more` の要素 1 つで表します。`-I` を指定したときは何も付け足さず、代わりに置換文字列を含む語が静的でなくなります。`find -exec` の `{}` も静的ではありません。

**`analysis`。** `"complete"` は、呼び出しがコマンドの構文から確定していることを表します。`"uncertain"` の呼び出しは候補で、語が実際に実行されるものと完全には一致しないかもしれません。静的でない文字列を解析し直して見つけた場合（`bash -c "gws docs $ARGS"`）、構文エラーを含むコマンドから見つけた場合、`PowerShell` ツールのコマンドをシェルの文法で読んだ場合がこれに当たります。危険コマンドの検出のためだけに作る候補（プログラム名のブレース展開を最初の選択肢に畳んだものなど）は、判定器に渡しません。

**`stdin`。** 入力のリダイレクトが無く、パイプからも受け取らない呼び出しは `null` で、エージェントの標準入力をそのまま引き継ぎます。本文が静的に決まるヒアドキュメントや here-string（`gws … <<'EOF'` など）は `{"value": "…", "static": true}` になります。何かが流れ込むものの中身が分からない場合、つまりパイプ、`< file`、静的でないヒアドキュメントや here-string（`<<< "$TEXT"`）では `{"value": null, "static": false}` になります。

### 出力と終了コード

| 判定器の結果 | claw-hooks の動作 |
|---|---|
| exit `0`、stdout が空か空白のみ | 何もしない |
| exit `0`、stdout に出力あり | 異議なし。`context_delivery` が `true` なら出力を `[<label>] <出力>` の形でエージェントへの補足にし、そうでなければ捨てる |
| exit `2` | `[<label>] <理由>` でコマンドを拒否する。理由は stderr で、stderr が空なら stdout、どちらも空なら `blocked by command hook` |
| それ以外の終了コード、シグナルによる終了、起動失敗、時間切れ、作業ディレクトリとして報告されたパスがディレクトリでない | 判定器の失敗として `on_error` に従う。`allow` ならコマンドを通してデバッグログに警告を残し、`block` なら `[<label>] command hook failed: <原因>` で拒否する。`<原因>` は `timed out after 5s`、`exit code 1`、`terminated by a signal`、`could not be started` など |

`<label>` は `run` のプログラムの basename で、`run = "noslop hook command"` なら `noslop` です。判定器の出力からは ANSI エスケープシーケンスを取り除き、前後の空白を削ります。エージェントへ送る文字列は、ほかのフックの出力と同じく `output_max_length` で切り詰めます。stdout と stderr はそれぞれ 4 MiB までしか保持しません。判定器が stdin を読まずに終了しても、失敗とはみなしません。

### 補足が届くエージェント

| エージェント | exit `0` の補足 |
|---|---|
| Claude Code | `PreToolUse`: `{"hookSpecificOutput":{"hookEventName":"PreToolUse","additionalContext":"…"}}`。`permissionDecision` を付けないので、通常の権限フローはそのまま働く |
| Codex CLI | `PreToolUse`: 同じ形。`PermissionRequest`: 捨てて `{}` を返す |
| Cursor、Windsurf、Antigravity CLI、Grok CLI | 捨てて、通常の許可応答を返す |

拒否はどのエージェントでも効き、組み込みフィルターと同じ拒否応答を使います（[入出力リファレンス](#入出力リファレンス) を参照）。補足を返すのは、どの判定器もコマンドを拒否しなかったときだけです。

### 上限

- **判定器の実行は 1 回のフックイベントにつき 32 回まで。** 超えた分は走らせず、走らなかったフックそれぞれの `on_error` に従います。`block` のフックは `[<label>] command hook skipped: too many invocations to check` で拒否します。
- **各回の時間はフックの `timeout` だけで決まる。** コマンドフックは、プロジェクト設定から上書きできる `hook_timeout` を使いません（[設定](configuration.ja.md#コマンドフック) を参照）。32 回の上限と合わせ、1 回のフックイベントにかかる時間は最大でも `timeout` の 32 回分（既定なら 160 秒）です。
- **解析しきれないコマンド。** コマンドが長すぎるか入れ子が深すぎてパーサーが解析できないときは、判定器を走らせません。`on_error = "block"` のコマンドフックが 1 つでもあれば `[<label>] command hook could not analyze the command` で拒否し、無ければ通します。

### 作業ディレクトリと環境変数

エージェントが報告したコマンドの作業ディレクトリが実在するディレクトリなら、判定器はそこで走ります。報告されたパスがディレクトリでなければ判定器は起動せず、`on_error` に従います。報告が無ければ、claw-hooks の作業ディレクトリを引き継ぎます。

判定器の環境変数は、claw-hooks の環境変数に `CLAW_HOOKS_COMMAND_HOOK_ACTIVE=1` を加えたものです。この変数が設定された状態で起動した claw-hooks はコマンドフックをまるごと飛ばします。判定器がエージェントを動かし、そのエージェントのフックから claw-hooks が呼ばれても、判定器が再帰的に呼ばれることはありません。そのプロセスでも、ほかのフィルターは通常どおり働きます。

### 調べない呼び出し

フックは呼び出しをプログラム名で照合するため、名前がコマンドにそのまま書かれている必要があります。`$CMD args` や `"$(which gws)" docs …` のように名前が実行時まで決まらない呼び出しでは、どのプログラムが起動するのかを claw-hooks は判断できず、その呼び出しは調べません。そのため、エージェントが意図してプログラム名を隠した場合の防御にはなりません。

### ログ

デバッグログに残すのは、判定器のプログラム名、一致した呼び出しの数、終了コード、バイト数、所要時間、`on_error` の方針だけです。引数、判定器への入力 JSON、判定器の出力はログに書きません。
