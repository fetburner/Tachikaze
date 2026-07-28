//! テストフィクスチャ (tests/fixtures/sample.mp4) を扱うための共通ヘルパ。
//!
//! フィクスチャは `tests/fixtures/gen.sh` で生成する必要があり、リポジトリには
//! コミットされていない。フィクスチャが無い環境でもテストを失敗させず、
//! 早期returnでスキップできるようにする。

use std::path::{Path, PathBuf};

/// `tests/fixtures/sample.mp4` の絶対パスを返す。
pub fn fixture_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sample.mp4")
}

/// フィクスチャが存在しない場合に true を返す。
///
/// 呼び出し側は次のように使う:
/// ```ignore
/// if common::skip_if_fixture_missing() {
///     return;
/// }
/// ```
pub fn skip_if_fixture_missing() -> bool {
    if fixture_path().exists() {
        return false;
    }
    eprintln!(
        "tests/fixtures/sample.mp4 が無いためスキップします。\
         `tests/fixtures/gen.sh` を実行してください。"
    );
    true
}

/// フィクスチャが無ければ早期returnするマクロ。
#[macro_export]
macro_rules! require_fixture {
    () => {
        if $crate::common::skip_if_fixture_missing() {
            return;
        }
    };
}
