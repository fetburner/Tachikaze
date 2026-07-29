//! テストフィクスチャ (tests/fixtures/sample.mp4) を扱うための共通ヘルパ。
//!
//! フィクスチャは `tests/fixtures/gen.sh` で生成する必要があり、リポジトリには
//! コミットされていない。フィクスチャが無い環境でもテストを失敗させず、
//! 早期returnでスキップできるようにする。

use std::path::{Path, PathBuf};

/// `tests/fixtures/<name>` の絶対パスを返す。
pub fn fixture_path_named(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name)
}

/// `tests/fixtures/sample.mp4`（H.264 + Opus）の絶対パスを返す。
pub fn fixture_path() -> PathBuf {
    fixture_path_named("sample.mp4")
}

/// `tests/fixtures/sample_aac.mp4`（H.264 + AAC / Mp4a）の絶対パスを返す。
///
/// 映像パラメータは Opus 版と同一（`tests/fixtures/gen.sh`）。音声 Codec だけが
/// 異なるため `tests/data/sample.dtvi` をそのまま流用できる。
// audio_e2e からのみ使う（common は各テストバイナリで個別にコンパイルされる）。
#[allow(dead_code)]
pub fn aac_fixture_path() -> PathBuf {
    fixture_path_named("sample_aac.mp4")
}

/// 指定したフィクスチャが存在しない場合に true を返す。
pub fn skip_if_fixture_missing_at(path: &Path) -> bool {
    if path.exists() {
        return false;
    }
    eprintln!(
        "{} が無いためスキップします。`tests/fixtures/gen.sh` を実行してください。",
        path.display()
    );
    true
}

/// フィクスチャ（Opus 版）が存在しない場合に true を返す。
///
/// 呼び出し側は次のように使う:
/// ```ignore
/// if common::skip_if_fixture_missing() {
///     return;
/// }
/// ```
pub fn skip_if_fixture_missing() -> bool {
    skip_if_fixture_missing_at(&fixture_path())
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
