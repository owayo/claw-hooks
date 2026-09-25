# 設定

デフォルトの場所: `~/.config/claw-hooks/config.toml`（全プラットフォーム共通）

```toml
# コマンドブロック
rm_block = true                    # rm/rmdir/del/eraseをブロック（デフォルト: true）
kill_block = true                  # kill/pkill/killall/taskkillをブロック（デフォルト: true）
dd_block = true                    # ddコマンドをブロック（デフォルト: true）

# カスタムメッセージ（推奨: safe-rm/safe-killツールと併用）
# safe-rm: https://github.com/owayo/safe-rm
# safe-kill: https://github.com/owayo/safe-kill
rm_block_message = "🚫 Use safe-rm instead: safe-rm <file> (validates Git status and path containment). Only clean/ignored files in project allowed."
kill_block_message = "🚫 Use safe-kill instead: safe-kill <PID> or safe-kill -n <name> (like pkill). Use -s <signal> for signal."
dd_block_message = "🚫 dd command blocked for safety."

# デバッグログ
debug = false
# log_path = "~/.config/claw-hooks/logs"  # デフォルト: config.tomlと同じディレクトリ
# 先頭の "~" はホームディレクトリへ展開されます。展開しない相対パスはフックプロセスの
# 作業ディレクトリ（= 編集中のリポジトリ）を基準に解決されてしまうためです。
# デバッグログにはフックイベントの概要と実行ファイルの basename のみを記録します。
# フックの引数、実行ファイルのディレクトリ、ファイル本文、エージェントメッセージは保存しません

# フックコマンドタイムアウト（秒）（デフォルト: 60、最大: 86400）
# report=true のStopフックと拡張子フックコマンドに適用されます。
# このタイムアウトを超えたコマンドはkill（SIGKILL）され、失敗として報告されます。
# report=false のStopフックはデタッチ起動され、完了を待ちません。
# hook_timeout = 60

# 出力最大長（文字数）（デフォルト: 1000、0 = 無制限）
# AIエージェントのコンテキストウィンドウ溢れを防止
# output_max_length = 1000

# カスタムコマンドフィルター（正規表現対応）
[[custom_filters]]
command = "yarn"
message = "`yarn`の代わりに`pnpm`を使用してください"

# argsモード: コマンド（正規表現） + 引数マッチング
[[custom_filters]]
command = "npm"
args = ["install", "i", "add"]         # ブロック対象: npm install, npm i, npm add
message = "`npm`の代わりに`pnpm`を使用してください"

[[custom_filters]]
command = "pip3?"                       # 正規表現: pip または pip3 にマッチ
args = ["install", "uninstall"]
message = "`uv pip`を使用してください"

# 正規表現のみモード（argsを指定しない場合）
[[custom_filters]]
command = "python[23]? -m pip"         # より複雑なパターン
message = "`uv pip`を使用してください"

[[custom_filters]]
command = "docker"
args = ["rm", "rmi", "system prune"]   # ブロック対象: docker rm, docker rmi
message = "ユーザーに直接実行を依頼してください"

# 拡張子フック（ファイル書き込み/編集時にトリガー）
# マップ形式: ".ext" = ["cmd1 {file}", "cmd2 {file}"]
# 出力（stdout/stderr）は、対応するフックランタイムでは additionalContext としてAIエージェントに送信
# 各コマンドテンプレートは {file} をちょうど1回含める必要があります
# 親ディレクトリ遡りパス（../）は安全のため拒否されます
# シェルのリダイレクトメタ文字（<, >）を含むパスは安全のため拒否されます
# タブ/改行/NUL は引数分割や不正なパスを防ぐため拒否されます
# Windows では `cmd /c` のメタ文字（%, !, ^, "）も変数展開インジェクション防止のため拒否されます
[extension_hooks]
".css" = ["biome format --write {file}", "biome lint --write {file}"]
".py" = ["ruff format --check {file}", "ruff check --preview --select=I,F,DOC {file}"]
".rs" = ["rustfmt {file}"]
".ts" = ["biome check {file}"]
".tsx" = ["biome check {file}"]

# Stopフック（エージェントループ終了時にトリガー）
# 配列内のすべてのコマンドは並列実行されます。
# conditionなしのフックはデフォルトで report=false となりデタッチ起動されます。
# stdout/stderr は破棄されるため、必要ならコマンド側でリダイレクトしてください。
# [[stop_hooks]]
# commands = ["afplay /System/Library/Sounds/Glass.aiff"]  # macOS通知音

# [[stop_hooks]]
# commands = ["notify-send 'エージェント完了'"]  # Linux通知

# 条件付きStopフック（Stop時にプロジェクト全体のlintを実行）
# プロジェクト構成ファイルの存在とツールの利用可能性を検出し、lint/typecheckを実行。
# 失敗時は、Stop時フィードバックに対応したエージェントでは結果をAIへ返し、
# エージェントが問題を修正します（Windsurf と Grok CLI はベストエフォート）。
# conditionフィールド（AND条件）: file_exists, file_not_exists, command_exists, command_not_exists
[[stop_hooks]]
commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
condition = { file_exists = "Cargo.toml" }

[[stop_hooks]]
commands = ["pnpm exec tsc --noEmit"]
condition = { file_exists = "tsconfig.json" }

[[stop_hooks]]
commands = ["ruff format .", "ruff check --preview --fix --select=I,F,DOC --unsafe-fixes"]
condition = { file_exists = "pyproject.toml", command_exists = "ruff" }

[[stop_hooks]]
commands = ["biome check --write ."]
condition = { file_exists = "package.json" }
```

## プロジェクトごとの設定

claw-hooksはデフォルトでグローバル設定ファイル（`~/.config/claw-hooks/config.toml`）を使用します。プロジェクトごとに動作をカスタマイズする方法は3つあります:

**1. `.claw-hooks.toml` — 自動検出されるプロジェクト設定（推奨）**

プロジェクトルートに `.claw-hooks.toml` を配置するだけです。claw-hooksはカレントディレクトリ直下の `.claw-hooks.toml` を自動検出し、グローバル設定とマージします。`--config` フラグは不要です。

```toml
# my-project/.claw-hooks.toml

# このプロジェクトで必要なガードを有効化する（有効化は常に許可される）
dd_block = true

# グローバルのフィルターに追加する
[[custom_filters]]
command = "yarn"
message = "Use pnpm instead"
```

**マージルール。** `.claw-hooks.toml` は「エージェントが clone してきたリポジトリの中のファイル」でもあり得るため、**未信頼の入力**として扱います。プロジェクト設定は防御を**強める**ことはできますが、弱めることはできず、新しいコマンド実行を持ち込むこともできません。

| フィールド | ルール | 動作 |
|-----------|--------|------|
| `rm_block`, `kill_block`, `dd_block` | **有効化のみ** | `true` は反映。`false` への上書きは警告を出して無視 |
| `custom_filters` | **追加のみ** | プロジェクトの定義を追加。グローバルの削除・置換は不可 |
| `stop_hooks` | **無視** | エージェント停止時に任意コマンドが走るため |
| `extension_hooks` | **無視** | ファイル編集のたびに任意コマンドが走るため |
| `*_block_message`, `hook_timeout`, `output_max_length` | **上書き** | プロジェクトの値が優先（いずれもブロック判定を弱めない） |
| `debug`, `log_path`, `nano_buddy` | **グローバル専用** | エラーとして拒否 |

省略されたフィールドはグローバルの値を維持します。無視した項目は警告として報告されるため、「設定したのに効かない」状態が見えないまま残ることはありません。`stop_hooks` と `extension_hooks` は適用されずに破棄されるため、**内容の検証も行いません**。書式が壊れた項目も正しい項目と同じように無視されるだけで、設定読み込み全体を失敗させることはありません（失敗させると、clone したリポジトリに置かれた 2 行でそのディレクトリの全コマンドが deny になってしまいます）。グローバルの `config.toml` は厳密に検証します。

`claw-hooks check` で検証できます — プロジェクト設定の有無と妥当性に加えて、無視される項目と未知の（タイポした）キーを報告します。

> **プロジェクト単位のフォーマッター・リンター。** `extension_hooks` と `stop_hooks` はグローバルの `config.toml` に書き、`condition = { file_exists = "…" }` で対象を絞ってください。プロジェクトごとに挙動を変えつつ、「リポジトリ側が実行内容を決められる」状態を避けられます。プロジェクト設定に書いたものは無視され、`claw-hooks check` が報告します。

> **`hook_timeout = 0` は拒否します。** 「無制限」の意味ではなく（`0` を無制限として扱うのは `output_max_length` だけです）、全フックが即座にタイムアウトする設定になります。claw-hooks は不正な設定でフェイルクローズするため、この設定があるとすべてのコマンドが拒否されます。`claw-hooks check` が原因を指摘します。

> **カスタムフィルターはコマンド名を正規化します。** 組み込みの `rm`/`kill`/`dd` フィルターと同じ扱いで、`command = "npm"` のフィルターは `/usr/bin/npm` / `./npm` / `NPM` / `npm.cmd` にもマッチします。

**2. `--config` — 設定ファイルの完全置換**

`--config` を使用して完全な設定ファイルを指定し、グローバル設定を完全に置き換えます:

```toml
# my-project/.claude/claw-hooks.toml
rm_block = true
kill_block = true
dd_block = false  # このプロジェクトでは dd を許可

[extension_hooks]
".rs" = ["rustfmt {file}"]
```

```json
// my-project/.claude/settings.json
{
  "hooks": {
    "PreToolUse": [{
      "matcher": "Bash|PowerShell",
      "hooks": [{ "type": "command", "command": "claw-hooks hook --config .claude/claw-hooks.toml" }]
    }],
    "PostToolUse": [{
      "matcher": "Write|Edit|MultiEdit|NotebookEdit",
      "hooks": [{ "type": "command", "command": "claw-hooks hook --config .claude/claw-hooks.toml" }]
    }],
    "Stop": [{
      "matcher": "",
      "hooks": [{ "type": "command", "command": "claw-hooks hook --config .claude/claw-hooks.toml" }]
    }]
  }
}
```

**3. 条件付きStopフック — プロジェクト自動検出**

`file_exists` 条件付きのStopフックは、作業ディレクトリに基づいてプロジェクトタイプを自動判定します。単一のグローバル設定で複数のプロジェクトタイプに対応できます:

```toml
# ~/.config/claw-hooks/config.toml

# Rustプロジェクトのみで実行（Cargo.toml が存在する場合）
[[stop_hooks]]
commands = ["cargo clippy -- -D warnings"]
condition = { file_exists = "Cargo.toml" }

# TypeScriptプロジェクトのみで実行（tsconfig.json が存在する場合）
[[stop_hooks]]
commands = ["pnpm exec tsc --noEmit"]
condition = { file_exists = "tsconfig.json" }
```

3つのアプローチはすべて組み合わせ可能です。グローバル設定で共通ルールを定義し、`.claw-hooks.toml` でプロジェクト固有の上書きを行い、条件付きStopフックでプロジェクトタイプの自動検出を活用できます。

## 条件付きStopフック（プロジェクト全体Lint）

`condition`フィールドを持つStopフックは、プロジェクトの構成ファイルに応じてlint/typecheckコマンドを実行します。`commands`配列内のすべてのコマンドは**並列実行**されます。失敗したコマンドの出力はすべて収集され、AIエージェントにブロック理由としてまとめて返されます。

**タイムアウトの扱い:** `hook_timeout` は最大 `86400` 秒まで指定できます。報告対象のStopフック（`report = true`）では、`hook_timeout` を超えたコマンドを claw-hooks がプロセスツリーごと強制終了（SIGKILL）し、タイムアウトをブロック理由として返します。直接の子プロセスが終了しても、バックグラウンド孫プロセスが stdout/stderr パイプを保持している場合もタイムアウト扱いにするため、`sh -c 'sleep 60 &'` のようなコマンドでフックタイムアウトを回避できません。通常のコマンド失敗（終了コード `124` を自ら返す場合を含む）も引き続きブロック対象です。`report = false` のStopフックは stdin/stdout/stderr を null にしてデタッチ起動されるため、claw-hooks は完了待ちも `hook_timeout` の強制も行いません。必要ならコマンド自体を timeout ツールで包んでください。

ただし Windsurf と Grok CLI は例外です。Windsurf の `post_cascade_response` は非同期の事後フックであり、Grok も `PreToolUse` 以外はすべて stdout が無視される事後フックです。どちらも Stopフック自体は実行されますが、失敗はベストエフォート扱いとなり、AI エージェントへのブロックとしては返されません。

**Stopフックのフィールド:**

| フィールド | 型 | デフォルト | 説明 |
|-----------|------|-----------|------|
| `commands` | `string[]` | (必須) | 実行するコマンド（同じstage内で並列実行） |
| `condition` | `object` | (なし) | 実行条件（AND条件: `file_exists`, `file_not_exists`, `command_exists`, `command_not_exists`） |
| `stage` | `1-5` | `5` | 実行順序。小さいstageが先に実行される。同じstage内のフックは並列実行。 |
| `report` | `bool` | (自動) | 結果をAIエージェントに返すかどうか。デフォルト: `condition`ありなら`true`、なしなら`false`。 |
| `session_scope` | `"primary"` \| `"delegated"` \| `"all"` | `"primary"` | このフックを実行するセッション種別。`primary` = メインセッションのみ、`delegated` = 委譲エージェントセッション（Claude Code の teammate 等）のみ、`all` = 両方。 |

**conditionフィールド**（AND条件 — 指定されたすべての条件が真である必要があります）:

| フィールド | 説明 |
|-----------|------|
| `file_exists` | 作業ディレクトリにこのファイルが存在する場合のみ実行 |
| `file_not_exists` | 作業ディレクトリにこのファイルが **存在しない** 場合のみ実行（例: 「このロックファイルが無い時のフォールバック」）|
| `command_exists` | このコマンドがPATH上に存在する場合のみ実行（Windows の `PATHEXT` を考慮。Unix では実行ビットが必要。`./tool` や `/usr/bin/tool` のような明示パスも判定可能） |
| `command_not_exists` | このコマンドがPATH上に **存在しない** 場合のみ実行 |

```toml
# ステージベースの実行: 分析 → lint → コミット
[[stop_hooks]]
commands = ["astro-sight impact --dir . --git"]
stage = 1        # 最初に実行
report = true    # 結果をAIに返す

[[stop_hooks]]
commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
condition = { file_exists = "Cargo.toml" }
stage = 3
# report 未指定 → condition あり → true（デフォルト）

[[stop_hooks]]
commands = ["pnpm exec tsc --noEmit"]
condition = { file_exists = "tsconfig.json" }
stage = 3

[[stop_hooks]]
commands = ["git-sc --all --yes --quiet"]
# stage 未指定 → 5（最後）
# report 未指定 → condition なし → false（fire-and-forget）
```

**ステージの実行順序:** ステージは1から5の順に逐次実行されます。同じステージ内のすべてのフックは並列実行されます。あるステージの全フックが完了してから次のステージに進みます。

**レポート動作:** `report = true`（または`condition`によるデフォルト`true`）の場合、コマンド失敗はAIエージェントにブロック理由として返されます。`report = false`（または`condition`なしによるデフォルト`false`）の場合、コマンドは fire-and-forget 方式で起動され、Hook応答をブロックしません。デタッチコマンドは stdin/stdout/stderr が null になるため、spawn 失敗はログに残りますが、コマンド出力と終了ステータスは収集されません。Windsurf と Grok CLI の Stop フックは常にベストエフォートです（Windsurf は基盤側が非同期、Grok は stdout が無視されるため）。

**セッションスコープ（エージェントセッションの抑止）:** claw-hooks は委譲エージェントのセッションとメインセッションを自動で判別します: 委譲側の Stop ペイロードには空白でない `agent_id` と `agent_type` の両方が含まれます（公式仕様では `agent_id` は「サブエージェント呼び出しの内側で発火したときだけ入る」と定義されています）。`--agent` で起動したメインセッションにも `agent_type` は入り得ますが、サブエージェント固有の `agent_id` は無いためメイン扱いを維持します。デフォルト（`session_scope = "primary"`）では Stop フックは**メインセッションの停止時のみ**実行されるため、大量の teammate が通知スパム・重複 lint・並列 `git` 自動コミットのレースを引き起こすことはありません。常に実行したいフックには `session_scope = "all"` を、エージェントセッション専用のフック（teammate ごとのクリーンアップ等）には `"delegated"` を指定します。判別フィールドが欠落・空白・非文字列の場合と、セッション種別のシグナルを持たないエージェント（Cursor / Windsurf / Codex CLI / Antigravity / Grok CLI）はメインセッションとして扱われます。

> **agent team の teammate はスコープ外です。** teammate はインプロセスで動き、完了は Claude Code の別イベント `TeammateIdle` で通知されますが、claw-hooks はこれを意図的に扱いません。このイベントにはループカウンタ（`stop_hook_active` / `loop_count` に相当するもの）が無く、失敗を伝える唯一の手段が「teammate に作業を継続させる」ことなので、恒久的に失敗する lint があると無限ループになります。**したがって teammate の idle 時には停止時 lint も通知も実行されません。**

```toml
# メインセッションの停止時のみ実行（デフォルト — フィールド指定不要）
[[stop_hooks]]
commands = ["cargo clippy --all-targets --all-features -- -D warnings"]
condition = { file_exists = "Cargo.toml" }

# メインセッションと委譲エージェントセッションの両方で実行
[[stop_hooks]]
commands = ["collect-metrics"]
report = false
session_scope = "all"
```

```toml
# その他の例:

# Python: pyproject.toml があり ruff がインストール済みの場合に ruff format/check を実行
[[stop_hooks]]
commands = ["ruff format .", "ruff check --preview --fix --select=I,F,DOC --unsafe-fixes"]
condition = { file_exists = "pyproject.toml", command_exists = "ruff" }

# JavaScript/TypeScript: package.json がある場合に biome check を実行
[[stop_hooks]]
commands = ["biome check --write ."]
condition = { file_exists = "package.json" }
```

## Stopフックの環境変数

claw-hooksはStopフックの子プロセスに以下の環境変数を渡します:

| 変数名 | 説明 |
|--------|------|
| `CLAW_HOOKS_STOP_ACTIVE` | 常に `1` に設定。子プロセスが別のclaw-hooks Stopイベントをトリガーした際の再帰実行を防止します。 |
| `CLAW_HOOKS_AGENT_MESSAGE` | AIエージェントが停止前に残した最後のメッセージ（利用可能な場合）。エージェントが何を作業していたかの情報を含みます。 |

**`CLAW_HOOKS_AGENT_MESSAGE`** の取得元:
- **Claude Code**: Stopイベントの `last_assistant_message` フィールド
- **Windsurf**: `post_cascade_response` イベントの `response` フィールド
- **Cursor**: 利用不可

これはエージェントのコンテキストを活用できるツールに有用です。例えば、[git-sc](https://github.com/owayo/git-smart-commit)はこの情報を使ってより正確なコミットメッセージを生成します:

```toml
[[stop_hooks]]
commands = ["git-sc --all --yes --quiet"]
```

git-scがStopフックとして実行されると、`CLAW_HOOKS_AGENT_MESSAGE` を読み取り、エージェントのコンテキストをAIプロンプトに含めます。これにより、単なるdiffの説明ではなく、変更の意図を反映したコミットメッセージが生成されます。

## カスタムフィルターの動作

カスタムフィルターは2つのモードをサポートしています:

**正規表現モード**（デフォルト）: `command`のみ指定した場合、正規表現パターンとして扱われます。

```toml
[[custom_filters]]
command = "python[23]? -m pip"    # 複雑な正規表現パターン
message = "uv pipを使用してください"
```

**argsモード**: `args`を指定した場合、`command`は正規表現パターンとしてコマンド名に対してマッチされ、argsのいずれかにマッチするとフィルターが発動します。

```toml
[[custom_filters]]
command = "npm"                    # 正規表現パターン（コマンド名）
args = ["install", "i", "add"]     # 第1引数がこれらのいずれかにマッチ
message = "pnpmを使用してください"

[[custom_filters]]
command = "pip3?"                  # pip と pip3 両方にマッチ
args = ["install", "uninstall"]    # 第1引数がこれらのいずれかにマッチ
message = "uv pipを使用してください"
```

両モードとも `;`、`&&`、`||`、`|` でチェーンされたコマンドも検出します:

```bash
# ブロック: セミコロンの後の yarn を検出
echo "install"; yarn install
# → {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"`yarn`の代わりに`pnpm`を使用してください"}}

# 許可: "yarn" はクォート内（コマンドではない）、pnpm は OK
echo "not yarn install"; pnpm install
# → {}
```

クォート内のコマンドは無視されます（引数であり、コマンドではないため）。

## 拡張子フックのルール

- 各コマンドテンプレートには `{file}` プレースホルダーを 1 つだけ含めます。
- 保存後・編集後イベントのみ: Claude `PostToolUse` (`Write`/`Edit`)、Cursor `afterFileEdit`、Windsurf `post_write_code`、Codex `PostToolUse` + `apply_patch`、Grok `PostToolUse`（`toolInput` にファイルパスを含むもの）、Antigravity `PostToolUse`（hook エントリに `--event PostToolUse` を指定した場合。編集対象は `toolCall.args.TargetFile` から取得）。Antigravity の事後フック出力は `{}` 固定のため診断を返せません。lint の本文が必要な場合は Stop hooks を使ってください。
- Codex の `PostToolUse` + `Bash` はパススルー。`apply_patch` は変更ファイルパスを抽出して拡張子フックに渡します（削除のみの patch はスキップ）。
- Grok の `PostToolUse` は `toolInput` に `file_path` / `filePath` があればフックを実行するため、フォーマッターによるファイル書き換えは通常どおり行われます。ただし Grok は事後フックの stdout を無視するので、lint の本文自体はエージェントに返りません。
- パスに `../`、リダイレクトメタ文字（`<`、`>`）、タブ、改行、NUL バイトを含むものは拒否。必須フィールドを欠くペイロードはフェイルクローズドで拒否。
- 成功時の no-op 定型通知はエージェントへ返しません。ファイル書き換え、警告、失敗を示す出力は保持し、コマンドラベルには展開済みファイルパスや引数要約ではなく設定上のプログラム名だけを表示します。
