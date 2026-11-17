//! `review` サブコマンドの E2E。
//!
//! 既定で走るテスト（フィクスチャ・ffmpeg 不要）と、`#[ignore]` 付き（フィクスチャと
//! ffmpeg が必要）のテストがある。ffmpeg の有無に関係なく `cargo test` が通ること。

#[allow(dead_code)]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{make_tmp_dir, skip_if_fixture_missing_at};
use tachikaze::{dtvi, workdir};

/// `review` を子プロセスで起動する。`env_path` を与えると `PATH` をそれで置き換える
/// （ツールが見つからない経路をテストするため）。
fn run_review(cache: &Path, input: &Path, env_path: Option<&str>) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
    cmd.arg("--cache-dir").arg(cache).arg("review").arg(input);
    if let Some(path) = env_path {
        cmd.env("PATH", path);
    }
    cmd.output().expect("tachikaze review の起動に失敗した")
}

/// 入力ファイル（中身は使わない。キャッシュの場所を決めるために存在だけ必要）を作る。
fn dummy_input(dir: &Path) -> PathBuf {
    let input = dir.join("IN.mp4");
    fs::write(&input, b"not really an mp4").expect("入力の作成");
    input
}

/// `analyze` が残すのと同じ規則で、キャッシュに `trim.avs` / `.dtvi` / `detail.jls` を置く。
fn seed_cache(cache: &Path, input: &Path, dtvi_source: &Path, trim_text: &str) {
    let trim = workdir::cached_trim_path(Some(cache), input).expect("trim のキャッシュパス");
    let dtvi_dst = workdir::cached_dtvi_path(Some(cache), input).expect("dtvi のキャッシュパス");
    let jls = workdir::cached_detail_jls_path(Some(cache), input).expect("jls のキャッシュパス");
    fs::create_dir_all(trim.parent().expect("親")).expect("キャッシュディレクトリ");
    fs::write(&trim, trim_text).expect("trim.avs の作成");
    fs::copy(dtvi_source, &dtvi_dst).expect("dtvi のコピー");
    fs::write(&jls, "").expect("detail.jls の作成");
}

/// 完了条件: キャッシュに `trim.avs` が無いとき、`analyze` の実行例を添えて exit code 1 で止まる。
#[test]
fn review_without_analyze_cache_suggests_analyze_and_exits_1() {
    let root = make_tmp_dir("review-e2e-no-cache");
    let cache = root.join("cache");
    let input = dummy_input(&root);

    let output = run_review(&cache, &input, None);
    assert_eq!(output.status.code(), Some(1), "exit code 1 のはず");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("tachikaze --cache-dir"),
        "--cache-dir を付けた実行例が無い: {stderr}"
    );
    assert!(
        stderr.contains(" analyze "),
        "analyze の実行例が無い: {stderr}"
    );
    assert!(output.stdout.is_empty(), "標準出力は空のはず");

    let _ = fs::remove_dir_all(&root);
}

/// キャッシュは揃っているのに ffmpeg が見つからないとき、ツール解決の失敗として止まる。
/// `PATH` を空にして条件を自分で作る（ffmpeg の有無に依存しない）。
#[test]
fn review_without_ffmpeg_fails_with_tool_error() {
    let root = make_tmp_dir("review-e2e-no-ffmpeg");
    let cache = root.join("cache");
    let input = dummy_input(&root);
    seed_cache(&cache, &input, &common::logo_dtvi_path(), "Trim(0,598)");

    let output = run_review(&cache, &input, Some(""));
    assert_eq!(output.status.code(), Some(1), "exit code 1 のはず");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("ffmpeg"),
        "ffmpeg が見つからない旨が無い: {stderr}"
    );
    let review_dir = workdir::cached_review_dir(Some(&cache), &input).expect("review のパス");
    assert!(!review_dir.exists(), "失敗時に review/ を作らないこと");
    let leftovers: Vec<_> = fs::read_dir(review_dir.parent().expect("親"))
        .expect("キャッシュ一覧")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("review.tmp"))
        .collect();
    assert!(leftovers.is_empty(), "一時ディレクトリを残さないこと");

    let _ = fs::remove_dir_all(&root);
}

/// 完了条件（ffmpeg が要る）: フィクスチャで `review` を走らせると、サムネイルの枚数が
/// `.dtvi` のキーパケット数と一致し、標準出力が `index.html` のパス1行だけになる。
#[test]
#[ignore = "要 ffmpeg とフィクスチャ（bash tests/fixtures/gen.sh）"]
fn review_writes_thumbnails_matching_keyframes_and_prints_index_path() {
    let fixture = common::logo_fixture_path();
    if skip_if_fixture_missing_at(&fixture) {
        return;
    }
    let root = make_tmp_dir("review-e2e-full");
    let cache = root.join("cache");
    seed_cache(
        &cache,
        &fixture,
        &common::logo_dtvi_path(),
        // 全599フレーム。保持区間は outward に広がる。
        "Trim(0,239) ++ Trim(480,598)",
    );

    let output = run_review(&cache, &fixture, None);
    assert!(
        output.status.success(),
        "review が失敗: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    assert_eq!(lines.len(), 1, "標準出力は1行のみ: {stdout}");
    let index = PathBuf::from(lines[0]);
    assert!(index.is_absolute(), "絶対パスのはず: {}", index.display());
    assert!(index.ends_with("review/index.html"), "{}", index.display());
    assert!(index.is_file());

    let html = fs::read_to_string(&index).expect("index.html の読み込み");
    assert!(
        !html.contains("__REVIEW_DATA__"),
        "プレースホルダが残っている"
    );

    let dtvi_text = fs::read_to_string(common::logo_dtvi_path()).expect("dtvi の読み込み");
    let parsed = dtvi::parse(&dtvi_text).expect("dtvi のパース");
    let keyframes = parsed.frames.iter().filter(|f| f.is_key_packet()).count();
    let thumbs = fs::read_dir(index.parent().unwrap().join("thumbs"))
        .expect("thumbs")
        .count();
    assert_eq!(
        thumbs, keyframes,
        "サムネイル枚数がキーパケット数と一致しない"
    );

    let _ = fs::remove_dir_all(&root);
}

/// `-o` に相対パスを渡すと、画面に埋める `output` は実行時のカレントディレクトリ基準の
/// 絶対パスになる（画面のコマンドはどこで実行されるか分からないため）。
#[test]
#[ignore = "要 ffmpeg とフィクスチャ（bash tests/fixtures/gen.sh）"]
fn review_output_option_is_embedded_as_absolute_path() {
    let fixture = common::logo_fixture_path();
    if skip_if_fixture_missing_at(&fixture) {
        return;
    }
    let root = make_tmp_dir("review-e2e-output");
    let cache = root.join("cache");
    seed_cache(&cache, &fixture, &common::logo_dtvi_path(), "Trim(0,598)");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .current_dir(&root)
        .arg("--cache-dir")
        .arg(&cache)
        .args(["review", "-o", "録画 1_CMcut.mp4"])
        .arg(&fixture)
        .output()
        .expect("tachikaze review の起動に失敗した");
    assert!(
        output.status.success(),
        "review が失敗: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let html = fs::read_to_string(&index).expect("index.html の読み込み");
    let expected = serde_json::to_string(
        &fs::canonicalize(&root)
            .expect("root の正規化")
            .join("録画 1_CMcut.mp4")
            .display()
            .to_string(),
    )
    .expect("JSON 文字列");
    assert!(
        html.contains(&format!("\"output\":{expected}")),
        "output が {expected} になっていない"
    );

    let _ = fs::remove_dir_all(&root);
}
