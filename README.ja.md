<p align="center">
  <img src="docs/images/app.png" width="128" alt="claw-hooks">
</p>

<h1 align="center">claw-hooks</h1>

<p align="center">
  シンプルなTOML設定でClaude Code・Cursor・Windsurf・Antigravity CLI・Codex CLI・Grok CLIに対応 - コマンドブロック、自動フォーマット、Stop時自動化
</p>

<!-- standard:badges:start -->
<h3 align="center">対応プラットフォーム</h3>

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

claw-hooks は、Claude Code・Cursor・Windsurf・Antigravity CLI・Codex CLI・Grok CLI のフックに組み込む単一バイナリです。エージェントに実行させないシェルコマンド、ファイル編集の後に走らせるフォーマッターとリンター、エージェントの停止時に実行する処理を、1 つの TOML ファイルで決めます。

## 機能

- **Rust製**: 低オーバーヘッド、軽量シングルバイナリ、超高速（起動<10ms）
- **Killコマンドブロック**: `kill`, `pkill`, `killall`, `taskkill`, PowerShell の `Stop-Process` とそのエイリアス `spps` をブロックし、[safe-kill](https://github.com/owayo/safe-kill)を提案
- **RMコマンドブロック**: `rm`, `rmdir`, `del`, `erase`, `rd`, PowerShell の `Remove-Item` をブロックし、[safe-rm](https://github.com/owayo/safe-rm)を提案
- **PowerShell ツール対応**: Claude Code の `PowerShell` ツールにも同じフィルターを適用。Git Bash の無い Windows では PowerShell が唯一のシェルツールになる。matcher は `Bash|PowerShell` を指定すること
- **DDコマンドブロック**: ディスク上書き事故を防ぐため、オプションで`dd`をブロック
- **AST解析**: [tree-sitter-bash](https://github.com/tree-sitter/tree-sitter-bash) でラッパー（`sudo`、`timeout`、`command`、`exec`、`pkexec`、`gosu`、`su`、`arch`、`systemd-run`、`script`）、サブシェル、パイプ、`eval`、`find -exec`、`bash -c`/`-lc`、コマンド置換、ブレースグループ、制御構文（`if`/`for`/`while`/`case`）、basename/拡張子/大文字小文字の正規化、シェル quote removal 形式を扱う。文字列フォールバックパーサー（非 `ast-parser` ビルド）も同等のカバレッジを維持
- **カスタムコマンドフィルター**: 正規表現サポート付きのカスタムフィルターを定義
- **拡張子フック**: `Write` / `Edit` / `MultiEdit` / `NotebookEdit` のファイル保存・編集完了後にのみ外部ツール（フォーマッター、リンター）を実行し、lint 出力を Claude Code / Codex CLI に `additionalContext`、Windsurf に exit 2 + stderr で送信。Antigravity CLI は `PostToolUse` エントリに `--event PostToolUse` を付けると `toolCall.args.TargetFile` を対象にツールが実行されるが、出力は `{}` 固定のためエージェントに伝わるのはファイル書き換えのみ。Grok CLI は編集ファイルパスが届くのでツール自体は通常どおり実行されるが、事後フックの stdout は無視されるため、エージェントに伝わるのはフォーマッターによるファイル書き換えのみ
- **Stopフック**: エージェントループ終了時にコマンドを実行（通知、git commit（[git-sc](https://github.com/owayo/git-smart-commit)等）、クリーンアップ等）
- **Stop時プロジェクト全体Lint**: プロジェクト構成ファイル（`Cargo.toml`、`tsconfig.json` 等）を自動検出して lint/typecheck を実行。失敗はエージェントに返却（Windsurf と Grok CLI はベストエフォート）
- **フックタイムアウト**: フックごとに設定可能（デフォルト 60 秒）。Unix ではプロセスグループ全体を SIGKILL するため、`sh -c '...'` 経由の孫プロセスも残らず停止
- **出力長制限**: エージェントのコンテキスト溢れを防ぐマルチバイト安全な切り詰め（デフォルト 1000 文字）
- **出力圧縮**: 装飾文字の連続（`.`、`=`、`-`、`─`、`━`、`^`、`·`、`→`、`_`）、`\r` で上書きされる進捗バー、cargo の繰り返し `Compiling`/`Blocking` ログ、共通絶対パスのプレフィックス、rustc/ruff/biome のマルチライン span 下線や枠線、Biome の空白可視化マーカーや重複行番号ペアを圧縮。成功時の `All checks passed!` や `1 file already formatted` など、何も変更していない formatter/linter の定型通知は省略し、ファイル変更・失敗の出力は保持。no-op 判定は**正規化後**の文字列で行うため、成功メッセージと毎回同じ設定警告を同時に出すツール（`ruff check --select D…` は毎回 stderr にルールセット非互換警告を出す）でも no-op と判定され、編集のたびに `All checks passed!` だけが返る事象を防ぐ。biome の `Checked N file(s) in <時間>. No fixes applied.` 集計行と、締めの `check ━` / `× Some errors were emitted while running checks.` は診断が併記されているときのみ除去し、出力全体がそれだけの場合は保持。ANSI 除去は `ESC` + 中間バイト形式（terminfo の `sgr0`、例: `\E(B\E[m`）と生の `SO`/`SI` にも対応（未対応だと色付き `cargo fmt --check` の差分行の先頭に文字が残り、上記の圧縮が一切効かなくなる）
- **ソース抜粋の再掲除去**: 同一診断の中で逐語一致するソース抜粋行（`3 │ code`、`> 3 │ code`、`12 | code`）は2回目以降を除去。biome は 1 件の診断をサブブロック（`!` メッセージ、`i` 補足、`i Safe fix:`）ごとに分けて同じ抜粋を再掲し、ruff も修正差分の中でコンテキストを再掲するが、これらの再掲には情報量が無い。差分行（`- old` / `+ new`）は修正内容そのものなので保持する。実出力での計測値: ruff −6%、biome −14%
- **診断をまたぐ抜粋の再掲除去**: 同じ箇所を指す診断が連続すると、抜粋が**まるごと**毎回再掲される（1 つの関数定義に `ANN201` / `D103` / `ANN001` / `ANN001`、1 つの `let` に `useConst` / `noUnusedVariables` が付くケース）。直前の診断と逐語一致する抜粋はブロックごと除去する。各診断のヘッダは残るのでファイル・行・列は失われず、間に別の抜粋を挟む診断は自分の抜粋を保持する。実出力での計測値: さらに ruff −15%、biome −8%。既定の 1000 文字の上限が同じコードの再掲で埋まって後続の診断が切り捨てられていたため、これはトークン量だけでなく**エージェントに届く情報量**を増やす
- **デバッグログ安全性**: 永続化するのはイベント/ツール/セッション、実行ファイルの basename、引数数、バイト数サマリーのみ。Stop/拡張子フックの引数と実行ファイルのディレクトリを除去し、生コマンド、ファイル本文、エージェントメッセージ、整形済み formatter/linter 出力はディスクに残さない（本文確認は `--trace` の stderr 経由のみ）
- **入出力サイズ上限**: stdin は 4 MiB 上限で、巨大ペイロードや不正 UTF-8 は OOM kill ではなくフェイルクローズドで停止。フック子プロセスの stdout/stderr もデッドロックを避けて最後まで排出しつつ各 4 MiB までしか保持しないため、大量出力する formatter/linter がエージェント向け切り詰め前にメモリを使い切ることを防止
- **フェイルクローズドのゲート**: コマンドブロックはパースエラー、読み取り不能な入力、設定の破損時に拒否を返す。`config.toml` のタイポ 1 つで保護が無効になることは無い。設定エラーでは exit `1` + stdout 空で終了せず、エージェント固有の拒否応答を返し、診断は stderr へ、あわせて `claw-hooks check` を案内する（exit 1 + stdout 空は一部のエージェントで「フック失敗＝判定を無視」と解釈されるため）。ただしフェイルクローズドにするのは実行前ゲートだけ。Stop 系での「ブロック」は「停止せず継続」を意味し、claw-hooks が中身を検査しないイベントでの拒否はユーザーのプロンプトを消去したり実際のツール出力を置き換えたりするだけで安全性を上げないため、いずれも許可に倒す。イベントを特定できないほど壊れたペイロードはブロックする
- **プロジェクト設定マージ**: プロジェクトルートに `.claw-hooks.toml` を配置してグローバル設定をプロジェクトごとに拡張。プロジェクト設定は未信頼の入力として扱われ（エージェントが clone したリポジトリにも置かれ得るため）、防御を**強める**方向のみ反映されます。ガードの有効化とフィルターの追加は反映され、ガードの無効化・グローバルフィルターの置換・stop/extension フックの宣言は警告付きで無視されます
- **マルチエージェント対応**: Claude Code、Cursor、Windsurf、Antigravity CLI、Codex CLI、Grok CLIに対応

## なぜ claw-hooks？

エージェント標準のフックは、危険コマンドのチェック 1 つ、フォーマッター 1 つに対しても Python/Bash スクリプトをエージェントごとに用意する必要があります。claw-hooks ならこれが TOML 設定だけで済みます。

```toml
# 危険コマンドをブロック
rm_block = true
rm_block_message = "🚫 Use safe-rm instead"

# 保存時に自動フォーマット
[extension_hooks]
".rs"  = ["rustfmt {file}"]
".py"  = ["ruff format --check {file}", "ruff check --preview --select=I,F,DOC {file}"]
".ts"  = ["biome check {file}"]
".tsx" = ["biome check {file}"]
```

…各エージェントのフック設定で claw-hooks を一度呼び出すだけ:

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

素朴な `grep -E '^rm '` では `sudo rm`、`cd /tmp && rm`、`bash -lc 'rm …'`、パイプ、`xargs`、ブレースグループ、プロセス置換、特権昇格ラッパー（`pkexec` / `gosu` / `su <user> cmd`）、シェル quote-removal 形式（`r\m`、`$'r\x6d'`）を取りこぼします。claw-hooks は tree-sitter-bash（同等カバレッジの文字列フォールバックパーサーつき）でこれらすべてに対処します。単一バイナリ、Python/jq 依存なし、Claude Code / Cursor / Windsurf / Antigravity / Codex / Grok で同じ挙動になります。

<details>
<summary>同等のネイティブ Python フックの例</summary>

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

これをエージェントごと、危険コマンドごと、フォーマッターごとに複製し、quote/ラッパー処理を毎回再実装することになります。
</details>

### 比較

| 機能 | ネイティブフック | claw-hooks |
|------|------------------|------------|
| 危険なコマンドをブロック | コマンドごとに25行以上のPython | TOML 1行 |
| カスタムフィルター | フィルターごとに新しいスクリプト | `[[custom_filters]]`に追加 |
| 拡張子フック（フォーマッター） | 複雑なファイル検出スクリプト | `[extension_hooks]`マップ |
| lint出力をエージェントに送信 | 手動でJSON構築 | 自動（Claude Code、Codex CLI）、Windsurf は exit 2 + stderr 経由*、Antigravity CLI は Stop hooks 経由*、Cursor は不可（`afterFileEdit` に出力スキーマが無いため）、Grok CLI は不可（事後フックの stdout が無視されるため） |
| マルチエージェント対応 | エージェントごとに異なるスクリプト | 単一バイナリ + `--format` |
| Stopフック（lint、通知等） | ユースケースごとにスクリプト作成 | `[[stop_hooks]]`設定 |

\* lint/フォーマッターの出力は、対応するフックランタイムでは `additionalContext` 経由で自動送信され、エージェントが警告を修正できます。Windsurf には相当する JSON フィールドが無いため、保存後の診断は終了コード 2 + stderr 本文で渡します。公式仕様ではブロックできるのは `pre_*` フックだけなので、この経路は編集を巻き戻さずに診断だけをエージェントへ届けます（`show_output` が `true` ならユーザーにも表示されます）。

## インストール

<!-- standard:install:start -->
### Homebrew (macOS/Linux)

```bash
brew install owayo/claw-hooks/claw-hooks
```

### Cargo

Rust 1.98.1 以上が必要です。

```bash
cargo install --git https://github.com/owayo/claw-hooks --locked
```

### GitHub Releases から

[Releases](https://github.com/owayo/claw-hooks/releases/latest) から自分の環境のアーカイブを取得して展開し、`claw-hooks` を `PATH` の通った場所に置きます。各リリースには、取得したファイルを確かめるための `SHA256SUMS` も添付しています。

| プラットフォーム | ファイル |
|---|---|
| Linux (x86_64) | `claw-hooks-x86_64-unknown-linux-gnu.tar.gz` |
| Linux (x86_64, musl) | `claw-hooks-x86_64-unknown-linux-musl.tar.gz` |
| Linux (ARM64) | `claw-hooks-aarch64-unknown-linux-gnu.tar.gz` |
| macOS (Intel) | `claw-hooks-x86_64-apple-darwin.tar.gz` |
| macOS (Apple Silicon) | `claw-hooks-aarch64-apple-darwin.tar.gz` |
| Windows (x86_64) | `claw-hooks-x86_64-pc-windows-msvc.zip` |

macOS でブラウザから取得した場合は、実行の前に隔離属性を外します: `xattr -d com.apple.quarantine claw-hooks`。

### ソースから

[mise](https://mise.jdx.dev/) が必要です (Rust のツールチェーンは `mise.toml` で固定しています)。

```bash
git clone https://github.com/owayo/claw-hooks.git
cd claw-hooks
make install
```

`make install` は `/usr/local/bin` に入れます。場所を変えるときは `INSTALL_PATH` を指定します (例: `make install INSTALL_PATH="$HOME/.local/bin"`)。
<!-- standard:install:end -->

## クイックスタート

```bash
# デフォルト設定を生成（既存の設定は決して上書きしない。
# 別の場所へ書き出すには --path / --config を渡す）
claw-hooks init

# 安全なコマンドでテスト（許可）
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"}}' | claw-hooks hook
# 出力: {}

# 危険なコマンドでテスト（ブロック）
echo '{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"}}' | claw-hooks hook
# 出力: {"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"🚫 Use safe-rm instead..."}}
```

> **claw-hooks は拒否専用（deny-only）です。** 許可時に返すのは空オブジェクト（`{}`）+ exit `0` で、これは「承認」ではなく「異議なし」を意味します。公式仕様では `permissionDecision: "allow"` は**権限プロンプトをスキップする**指示であり、claw-hooks がブロックしなかったコマンドをすべて自動承認してしまうため、claw-hooks は `allow` を返しません。ブロックしなかったものには、これまでどおり既存の権限プロンプト・権限ルールが適用されます。

## 使い方

`claw-hooks hook` は stdin からフックイベントを 1 件読み、エージェント固有の形式で応答します。`init` は既定の設定を書き出し、`check` は設定を検証し、`version` はバージョンを表示します。

```bash
# Claude Codeフックを処理（デフォルト）
claw-hooks hook

# 他のエージェントは --format を指定（cursor、windsurf、agy、codex、grok）
claw-hooks hook --format cursor

# Antigravity CLI のペイロードにはイベント名が無いため --event も指定する
claw-hooks hook --format agy --event PostToolUse

# カスタム設定を使用
claw-hooks hook --config /path/to/config.toml
```

全サブコマンドとオプション、`--format` ごとのペイロードの読み方、イベントごとの出力と終了コード、フェイルクローズドの規則: [docs/cli-reference.ja.md](docs/cli-reference.ja.md)

## エージェント統合

各エージェントのフック設定ファイルに `claw-hooks hook` を登録します。Claude Code 以外のエージェントでは `--format` を付けます。

| エージェント | フック設定ファイル（ユーザー / プロジェクト） | コマンド |
|---|---|---|
| Claude Code | `~/.claude/settings.json` / `.claude/settings.json` | `claw-hooks hook` |
| Cursor | `~/.cursor/hooks.json` / `<project>/.cursor/hooks.json` | `claw-hooks hook --format cursor` |
| Windsurf (Cascade) | `~/.codeium/windsurf/hooks.json` / `.windsurf/hooks.json` | `claw-hooks hook --format windsurf` |
| Antigravity CLI | `~/.gemini/config/hooks.json` / `<project>/.agents/hooks.json` | `claw-hooks hook --format agy --event <event>` |
| Codex CLI | `~/.codex/hooks.json` | `claw-hooks hook --format codex` |
| Grok CLI | `~/.grok/hooks/` / `<project>/.grok/hooks/` | `claw-hooks hook --format grok` |

エージェントごとのフック設定の JSON、登録するイベントと matcher、エージェントへ返せるもの（lint の出力がエージェントに届くかなど）: [docs/integrations.ja.md](docs/integrations.ja.md)

## 設定

設定ファイルは、どのプラットフォームでも `~/.config/claw-hooks/config.toml` です。`claw-hooks init` で既定の設定を書き出し（既存の設定は上書きしません）、`claw-hooks check` で検証します。

```toml
# 危険なコマンドをブロックし、安全な代替ツールを案内する
rm_block = true
kill_block = true
rm_block_message = "🚫 Use safe-rm instead: safe-rm <file>"

# 特定の引数を伴うときだけコマンドをブロックする（command は正規表現）
[[custom_filters]]
command = "npm"
args = ["install", "i", "add"]
message = "`npm`の代わりに`pnpm`を使用してください"

# ファイルの書き込み・編集の後にフォーマッターとリンターを実行する
[extension_hooks]
".rs" = ["rustfmt {file}"]
".ts" = ["biome check {file}"]

# エージェントの停止時にプロジェクト全体を lint する（Cargo.toml がある場所だけ）
[[stop_hooks]]
commands = ["cargo clippy --all-targets --all-features -- -D warnings", "cargo fmt --check"]
condition = { file_exists = "Cargo.toml" }
```

作業ディレクトリの `.claw-hooks.toml` はグローバル設定にマージされますが、反映されるのは防御を強める方向だけです。ガードの有効化とフィルターの追加は効き、ガードの無効化と Stop フック・拡張子フックの宣言は警告付きで無視されます。`--config <path>` を渡すと、グローバル設定の代わりにそのファイルを使います。

全設定項目と既定値、プロジェクト設定のマージルール、ステージとセッションスコープを使う Stop フック、Stop フックに渡す環境変数、カスタムフィルターのモード: [docs/configuration.ja.md](docs/configuration.ja.md)

## パフォーマンス

| 項目 | 値 |
|------|-----|
| 起動時間 | 10ms未満 |

## 開発

<!-- standard:dev:start -->
[mise](https://mise.jdx.dev/) が必要です。ツールの版は `mise.toml` で固定しています。

```bash
make setup   # ツールチェーン (mise) と依存を取得する
make ci      # CI と同じ検査 (書き換えない)
```

| コマンド | 説明 |
|---|---|
| `make setup` | ツールチェーン (mise) と依存を取得する |
| `make build` | デバッグ版をビルドする |
| `make release` | リリース版をビルドする |
| `make run` | デバッグ版を実行する (引数は ARGS="...") |
| `make test` | テストを実行する |
| `make lint` | clippy を警告ゼロで通す |
| `make fmt` | コードを整形する (書き換える) |
| `make fmt-check` | 整形済みかを確かめる (書き換えない) |
| `make check` | 整形と静的検査 (書き換えない) |
| `make ci` | CI と同じ検査 (書き換えない) |
| `make install` | リリース版を INSTALL_PATH (既定 /usr/local/bin) に入れる |
| `make uninstall` | INSTALL_PATH から取り除く |
| `make clean` | ビルド成果物を消す |

`make` でターゲットの一覧を表示します。リリースは GitHub Actions で行います (**Actions → Release → Run workflow**)。
<!-- standard:dev:end -->

`make test` と `make lint` は、全 feature（tree-sitter の AST パーサー）と `--no-default-features`（文字列のフォールバックパーサー）の 2 構成で回します。フォールバックのビルドは別のパーサーを使うためです。

## ライセンス

<!-- standard:license:start -->
[MIT](LICENSE)
<!-- standard:license:end -->
