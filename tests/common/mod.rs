//! E2E テストの共通ヘルパ（フィクスチャのパス、一時ディレクトリ、外部ツールの有無）。
//!
//! フィクスチャは `tests/fixtures/gen.sh` で生成する必要があり、リポジトリには
//! コミットされていない。フィクスチャが無い環境でもテストを失敗させず、
//! 早期returnでスキップできるようにする。
//!
//! **ほとんどの関数に `#[allow(dead_code)]` が付いているのは、このモジュールが
//! 各テストバイナリで個別にコンパイルされるため。** どのバイナリからも
//! 「自分が使わない関数」は未使用として警告になる。どのファイルが何を使うかを
//! ここに列挙はしない（使い始めた時点で嘘になるため）。

use std::path::{Path, PathBuf};
use std::process::Command;

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

// --- `cut` に渡す `.dtvi` ---

/// `cut` に渡す `.dtvi`。実 `dtvindex` 出力の抜粋（`tests/data/sample.dtvi`）で、
/// `src/mp4io/order_map.rs` のテストが同じものをフィクスチャとの全行一致検証に
/// 使っている。
#[allow(dead_code)]
pub fn dtvi_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.dtvi")
}

// --- 一時ディレクトリ ---
//
// 「消してから作る」(`make_tmp_dir`)と「毎回一意にする」(`make_scratch_dir`)の
// 2種類を用意する。目的が違うので1つに統合しないこと:
// - `make_tmp_dir`: 同じ名前を再実行のたびに使い回す前提で、前回の残骸
//   （異常終了などで消し忘れたディレクトリ）を確実に消してから作り直す。
// - `make_scratch_dir`: nanos とリトライで毎回一意な名前を選び、既存を消さない。
//   カレントディレクトリを切り替えるテスト（`prepare_e2e.rs` の
//   `prepare_strips_edit_list_with_relative_input_and_relative_cache_dir`）など、
//   ディレクトリの削除タイミングに依存したくない場合に使う。

/// 一時ディレクトリを、既存の残骸があれば消してから作る。
///
/// `label` には呼び出し元のテストファイル・テストを識別する接頭辞を必ず含めること
/// （例: `"auto-e2e-full-success"`）。テストは並列実行されるため、別のテストと
/// 同じディレクトリ名になると、片方が他方のディレクトリを `remove_dir_all` で
/// 消してしまい不安定になる。
#[allow(dead_code)]
pub fn make_tmp_dir(label: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("tachikaze-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("一時ディレクトリを作れること");
    dir
}

/// 一時ディレクトリを、毎回一意な名前で作る（既存を消さない・`tempfile` クレートに
/// 依存しない素朴な方式）。
///
/// `make_tmp_dir` との違いは上のモジュール doc comment参照。`label` には
/// 呼び出し元を識別する接頭辞を含めること（例: `"prepare-e2e-cache-elst"`）。
#[allow(dead_code)]
pub fn make_scratch_dir(label: &str) -> PathBuf {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..100 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!("tachikaze-{label}-{pid}-{nanos}-{attempt}"));
        if std::fs::create_dir_all(&candidate).is_ok() {
            return candidate;
        }
    }
    panic!("scratch dir の作成に失敗しました");
}

// --- 外部ツール ---

/// 指定した実行ファイルが `PATH` に無ければ true を返し、その旨を stderr に出す。
#[allow(dead_code)]
pub fn skip_if_missing(bin: &str) -> bool {
    match Command::new(bin).arg("-version").output() {
        Ok(output) if output.status.success() => false,
        _ => {
            eprintln!("{bin} が無いためスキップします。");
            true
        }
    }
}

/// `ffmpeg` / `ffprobe` の両方が `PATH` にあるか確認する（無ければ false、その旨を
/// stderr に出す）。
#[allow(dead_code)]
pub fn tools_available() -> bool {
    for bin in ["ffmpeg", "ffprobe"] {
        match Command::new(bin).arg("-version").output() {
            Ok(output) if output.status.success() => {}
            _ => {
                eprintln!("{bin} が無いためスキップします。");
                return false;
            }
        }
    }
    true
}

// --- 偽ツールのスクリプト作成（`auto_e2e.rs` が使う） ---

/// `path` に `script` の内容を書き、実行権限を付与する。
///
/// `auto` の E2E が `dtvindex` / `chapter_exe` / `join_logo_scp` の偽ツール
/// （シェルスクリプト）を作るために使う。現状の利用箇所は `auto_e2e.rs` のみだが、
/// 他のテストが外部ツールを偽装する必要が生じたときのために置き場所は `common` に
/// している。
#[allow(dead_code)]
#[cfg(unix)]
pub fn write_executable_script(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).expect("スクリプトを書けること");
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("実行権限を付与できること");
}

/// `dir` を既存の `PATH` の先頭に前置した文字列を返す（子プロセスの `PATH` に
/// 偽ツールを注入するためのヘルパ）。
#[allow(dead_code)]
pub fn prepend_path(dir: &Path) -> std::ffi::OsString {
    let mut value = dir.as_os_str().to_os_string();
    if let Some(existing) = std::env::var_os("PATH") {
        value.push(":");
        value.push(existing);
    }
    value
}
