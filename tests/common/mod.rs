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

/// `tests/fixtures/sample_logo.mp4`（H.264 + Opus、疑似ロゴを「本編」区間だけに
/// 合成。#97 `analyze --logo` E2E 用）の絶対パスを返す。
#[allow(dead_code)]
pub fn logo_fixture_path() -> PathBuf {
    fixture_path_named("sample_logo.mp4")
}

/// `tests/fixtures/sample_logo_train.mp4`（同上、疑似ロゴを常時合成した学習専用
/// クリップ。`make-logo` で `.lgd` を作るのに使う）の絶対パスを返す。
#[allow(dead_code)]
pub fn logo_train_fixture_path() -> PathBuf {
    fixture_path_named("sample_logo_train.mp4")
}

/// `tests/data/sample_logo.dtvi`（`sample_logo.mp4` に対する実 `dtvindex build`
/// 出力そのもの、ヘッダ + 全599フレーム）の絶対パスを返す。`analyze --logo`
/// の E2E が使う偽 `dtvindex` の出力元として使う。
///
/// **レビュー指摘で判明: 以前は先頭40フレームの抜粋（キーフレーム1枚のみ）
/// だった**。`tests/data/sample.dtvi`（パーサのフォーマット理解を検証するだけ
/// の用途で抜粋で足りる）とは異なり、このファイルはロゴ検出（`detect_logo`）
/// のE2Eが実際にフレーム数・キーフレーム数の一致検査を通す必要があるため、
/// 抜粋では階層化方式（issue #154）のGOP構造やフレーム数検査を意味のある形で
/// 検証できなかった（キーフレーム1枚しか無いと精緻化GOPの選定が自明になり、
/// 検査対象のロジックを素通りしてしまう）。`bash tests/fixtures/gen.sh` で
/// `sample_logo.mp4` を作った上で `dtvindex build` を実行すれば同じ内容を
/// 再生成できる。
#[allow(dead_code)]
pub fn logo_dtvi_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample_logo.dtvi")
}

/// `tests/data/sample_no_logo.dtvi`（`sample.mp4` に対する実 `dtvindex build`
/// 出力そのもの、ヘッダ + 全599フレーム）の絶対パスを返す。
///
/// **レビュー指摘で追加**: `analyze --logo` の E2E がロゴを含まない
/// `sample.mp4` に対して検出のフォールバック（閾値未満）を確認するテスト
/// （`tests/analyze_logo_e2e.rs`）専用。当初は `sample.mp4` に対しても
/// `tests/data/sample.dtvi`（40フレームの抜粋、他の多くのテストが共有する
/// ため変更しない）や `sample_logo.dtvi`（別ファイル）の使い回しを試したが、
/// いずれも階層化方式（issue #154）のフレーム数検査で失敗した。`sample.dtvi`
/// はフレーム表が抜粋のため（`.dtvi` ヘッダの `frame_count` と食い違う）、
/// `sample_logo.dtvi` は `sample.mp4` とは別ファイルのため（**実測で判明**:
/// `width`/`height`/`frame_rate`/総フレーム数/キーフレームの表示順
/// frame_number は完全に一致するにもかかわらず、ffmpeg の `-ss` シークが
/// ファイルごとに微妙に異なる着地をすることがあり、末尾GOPの部分デコードで
/// 実際に読めた枚数が2フレームずれた。着地オラクル（`corr` の比較）はこの
/// 矩形の画素が全編にわたって不変だったため検出できず、blocker3で追加した
/// 「メディア側の真値」検査（読めた枚数の期待値との一致）でのみ検出できた）。
/// そのため `sample.mp4` 専用の完全な `.dtvi` をこのファイルとして別に持つ。
/// `bash tests/fixtures/gen.sh` で `sample.mp4` を作った上で `dtvindex build`
/// を実行すれば同じ内容を再生成できる。
#[allow(dead_code)]
pub fn no_logo_dtvi_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample_no_logo.dtvi")
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
// - `make_scratch_dir`: nanos で毎回一意な名前を選び、既存を消さない。一意性は
//   nanos だけが担保する（`create_dir_all` は既存ディレクトリでも `Ok` を返す
//   ため、衝突検出には使えない。`create_dir` を使うことで、万一 nanos が衝突
//   しても `AlreadyExists` になり、attempt のリトライが実際に機能する）。
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
        if std::fs::create_dir(&candidate).is_ok() {
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

/// `dir` を既存の `PATH` の先頭に前置した文字列を返す（外部ツールの解決先を
/// 差し替える唯一の手段が `PATH` の書き換えであるため、子プロセスの `PATH` に
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
