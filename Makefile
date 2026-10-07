# claw-hooks の開発タスク。引数なしの `make` でターゲット一覧を表示する。
#
# ツールの版は mise.toml で固定する。mise があれば常に `mise exec --` を通すため、
# IDE や GUI から起動してシェル側で mise が有効でなくても、固定した版を使える。
# SYSTEM_TOOLS=1 は PATH 上のツールを使う（この場合、版は保証しない）。
#
# macOS に付属する GNU Make 3.81 で使える構文に限る。
# .ONESHELL、.SHELLFLAGS、$(file ...)、!= は使わない。

.DEFAULT_GOAL := help

BINARY_NAME := claw-hooks
INSTALL_PATH ?= /usr/local/bin
# コミットした Cargo.lock を使い、CI と同じ依存関係で実行する。
CARGO_FLAGS ?= --locked

# ---- ツールチェーン --------------------------------------------------------------
# GUI 起動時はシェルの PATH を継承しない場合があるため、PATH の次に一般的な配置先を探す。
# make MISE=/path/to/mise で指定できる。mise が無い場合の動作は MISE_CANDIDATES= で確認する。
MISE_CANDIDATES ?= $(HOME)/.local/bin/mise /opt/homebrew/bin/mise /usr/local/bin/mise
ifeq ($(SYSTEM_TOOLS),1)
RUN :=
else
ifndef MISE
MISE := $(firstword $(shell command -v mise 2>/dev/null) $(wildcard $(MISE_CANDIDATES)))
endif
ifeq ($(MISE),)
ifneq ($(filter-out help,$(or $(MAKECMDGOALS),help)),)
$(error mise was not found. Install it from https://mise.jdx.dev, or add SYSTEM_TOOLS=1 to use the tools on PATH)
endif
endif
RUN := $(if $(MISE),$(MISE) exec --,)
endif

.PHONY: help setup build release run test lint fmt fmt-check check ci install uninstall clean

## 準備

setup: ## ツールチェーン（mise）と依存関係を準備
	@if [ -n "$(MISE)" ]; then "$(MISE)" install; fi
	$(RUN) cargo fetch $(CARGO_FLAGS)

## ビルド

build: ## デバッグ用バイナリをビルド
	$(RUN) cargo build $(CARGO_FLAGS)

release: ## リリース用バイナリをビルド
	$(RUN) cargo build --release $(CARGO_FLAGS)

run: ## デバッグ用バイナリを実行（引数は ARGS="..."）
	$(RUN) cargo run $(CARGO_FLAGS) -- $(ARGS)

## 検査

# テストと clippy は all-features（tree-sitter の AST パーサー）と
# no-default-features（文字列フォールバックパーサー）の 2 構成で実行する。
# フォールバックは別のパーサーなので、AST の検査だけでは検出漏れを見逃す。
# 統合テストは共通の実行ファイルを使うため、異なる構成のビルドを別途並行実行しない。
test: ## テストを実行（all-features の後に no-default-features）
	$(RUN) cargo test $(CARGO_FLAGS) --all-features
	$(RUN) cargo test $(CARGO_FLAGS) --no-default-features

lint: ## 両構成の clippy を実行（警告はエラー扱い）
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets --all-features -- -D warnings
	$(RUN) cargo clippy $(CARGO_FLAGS) --all-targets --no-default-features -- -D warnings

fmt: ## コードを整形（ファイルを書き換える）
	$(RUN) cargo fmt --all

fmt-check: ## 整形を検査（書き換えなし）
	$(RUN) cargo fmt --all -- --check

# lint は両構成を検査する。唯一の機能 ast-parser がデフォルトなので、
# デフォルト構成は all-features と同じになり、別の検査は不要。
check: fmt-check lint ## 整形と lint を検査（書き換えなし）

ci: check test ## CI と同じ検査を実行（書き換えなし）

## インストール

# 一時ファイルを rename してバイナリを置き換える。macOS は inode ごとに署名検査を
# キャッシュするため、実行中または直前に実行したファイルへ上書きすると、次の起動で
# SIGKILL（終了コード 137）になる。毎イベント起動する claw-hooks では特に発生しやすい。
# 同じディレクトリに一時ファイルを置き、rename によって inode ごと入れ替える。
install: release ## INSTALL_PATH にインストール（既定は /usr/local/bin）
	@mkdir -p "$(INSTALL_PATH)"
	cp "target/release/$(BINARY_NAME)" "$(INSTALL_PATH)/$(BINARY_NAME).new"
	mv -f "$(INSTALL_PATH)/$(BINARY_NAME).new" "$(INSTALL_PATH)/$(BINARY_NAME)"

uninstall: ## INSTALL_PATH のバイナリを削除
	rm -f "$(INSTALL_PATH)/$(BINARY_NAME)"

clean: ## ビルド成果物を削除
	$(RUN) cargo clean

## ヘルプ

help: ## このヘルプを表示
	@echo "Development tasks for $(BINARY_NAME)"
	@echo ""
	@echo "Usage: make <target>"
	@echo ""
	@grep -E '^[a-zA-Z0-9_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-12s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "Tool versions are pinned in mise.toml. Run make setup first."
	@echo "Release: GitHub Actions > Release > Run workflow"
