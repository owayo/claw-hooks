.PHONY: build release install clean test fmt check msrv help

# 既定ターゲット
.DEFAULT_GOAL := help

# 変数
BINARY_NAME := claw-hooks
INSTALL_PATH := /usr/local/bin

## ビルドコマンド

build: ## デバッグ版をビルド
	cargo build

release: ## リリース版をビルド
	cargo build --release

## インストール

# 上書きコピーではなく一時ファイル + rename で置き換える。
# macOS はコード署名の検証結果をパス/inode 単位でキャッシュするため、実行中または
# 直前に実行されたバイナリへ cp で上書きすると、キャッシュ済みの CDHash と中身が
# 食い違って新しいバイナリが起動直後に SIGKILL される (exit 137)。claw-hooks は
# フックイベントのたびに起動するので署名は常にキャッシュに載っており、これを
# 確実に踏む。rename はディレクトリエントリを差し替えて新しい inode を与えるため、
# 古い inode のキャッシュが適用されない。
install: release ## リリース版をビルドして /usr/local/bin にインストール
	cp target/release/$(BINARY_NAME) $(INSTALL_PATH)/$(BINARY_NAME).new
	mv -f $(INSTALL_PATH)/$(BINARY_NAME).new $(INSTALL_PATH)/$(BINARY_NAME)

## 開発

test: ## テストを実行
	cargo test

fmt: ## コードをフォーマット
	cargo fmt

check: ## clippy と cargo check を実行
	cargo clippy --all-targets --all-features -- -D warnings
	cargo check

msrv: ## MSRV(Rust 1.85)でビルド確認
	rustup run 1.85.0 cargo check --locked --all-features
	rustup run 1.85.0 cargo check --locked --no-default-features

clean: ## ビルド成果物を削除
	cargo clean

## ヘルプ

help: ## このヘルプを表示
	@echo "$(BINARY_NAME) ビルドコマンド"
	@echo ""
	@echo "使い方: make [target]"
	@echo ""
	@echo "ターゲット:"
	@grep -E '^[a-zA-Z_-]+:.*?## .*$$' $(MAKEFILE_LIST) | awk 'BEGIN {FS = ":.*?## "}; {printf "  \033[36m%-20s\033[0m %s\n", $$1, $$2}'
	@echo ""
	@echo "リリース:"
	@echo "  GitHub Actions > Release > Run workflow を使用"
