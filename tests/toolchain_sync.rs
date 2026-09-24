//! mise.toml で固定した Rust toolchain と Cargo.toml の rust-version の一致を検証する。
//!
//! CI は mise.toml の版だけでビルド・テストするため、rust-version がそれとずれると
//! 検証していない版を対応版として宣言することになる。依存更新ツールが片方だけを
//! 上げたときに、ここで止める。

use std::path::Path;

fn read_manifest(name: &str) -> toml::Table {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(name);
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|e| panic!("Failed to read {}: {e}", path.display()));
    text.parse::<toml::Table>()
        .unwrap_or_else(|e| panic!("Failed to parse {}: {e}", path.display()))
}

#[test]
fn rust_version_matches_mise_toolchain() {
    let mise = read_manifest("mise.toml");
    let rust = mise
        .get("tools")
        .and_then(|tools| tools.get("rust"))
        .expect("mise.toml has no [tools] rust entry");
    // `rust = "1.98.1"` と `rust = { version = "1.98.1", ... }` の両方の書き方を受け付ける。
    let toolchain = match rust {
        toml::Value::String(version) => version.as_str(),
        toml::Value::Table(options) => options
            .get("version")
            .and_then(toml::Value::as_str)
            .expect("mise.toml tools.rust has no version string"),
        other => panic!("Unexpected mise.toml tools.rust value: {other}"),
    };

    let cargo = read_manifest("Cargo.toml");
    let rust_version = cargo
        .get("package")
        .and_then(|package| package.get("rust-version"))
        .and_then(toml::Value::as_str)
        .expect("Cargo.toml has no package.rust-version string");

    assert_eq!(
        rust_version, toolchain,
        "Cargo.toml rust-version ({rust_version}) must match the mise.toml rust toolchain ({toolchain})"
    );
}
