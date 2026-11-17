//! `review` サブコマンドの E2E。
//!
//! 既定で走るテスト（フィクスチャ・ffmpeg 不要）と、`#[ignore]` 付き（フィクスチャと
//! ffmpeg が必要）のテストがある。ffmpeg の有無に関係なく `cargo test` が通ること。

#[allow(dead_code)]
mod common;

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use common::{make_tmp_dir, skip_if_fixture_missing_at};
use tachikaze::{dtvi, ffprobe, workdir};

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

/// 既定で走る（ffmpeg 不要）: フレーム表が切れた `.dtvi`（ヘッダ599・行40）では、
/// 重なりエラーより先に `frame_count` の不一致で止まり、`review/` と一時ディレクトリを残さない。
/// 検査が snap より後ろに動くと、この `stderr` は「重な」を含んで落ちる。
#[test]
fn review_with_truncated_dtvi_reports_frame_count_mismatch_before_overlap() {
    let root = make_tmp_dir("review-e2e-truncated");
    let cache = root.join("cache");
    let input = dummy_input(&root);
    seed_cache(
        &cache,
        &input,
        &common::dtvi_path(),
        "Trim(10,109) ++ Trim(370,469)",
    );

    let output = run_review(&cache, &input, Some(""));
    assert_eq!(output.status.code(), Some(1), "exit code 1 のはず");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("frame_count"), "{stderr}");
    assert!(stderr.contains("（599）"), "{stderr}");
    assert!(stderr.contains("（40）"), "{stderr}");
    assert!(
        !stderr.contains("重な"),
        "重なりエラーより先に出ること: {stderr}"
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

/// `.dtvi` のフレーム表で、表示順フレーム番号 `frame` の行の flags（最終列）からキーフラグを外す。
/// 行は1列目で探す（文字列の置換で狙い撃ちしない）。
fn clear_key_flag(dtvi_path: &Path, frame: u32) {
    let text = fs::read_to_string(dtvi_path).expect("dtvi の読み込み");
    let mut in_frames = false;
    let mut hits = 0;
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        if line == "FRAMES" {
            in_frames = true;
            lines.push(line.to_string());
            continue;
        }
        let mut cols: Vec<String> = line.split('\t').map(str::to_string).collect();
        if in_frames && cols.first().map(String::as_str) == Some(&frame.to_string()) {
            let flags: u8 = cols
                .last()
                .expect("flags 列")
                .parse()
                .expect("flags は10進の整数");
            let cleared = flags & !dtvi::FLAG_KEY_PACKET;
            *cols.last_mut().expect("flags 列") = cleared.to_string();
            hits += 1;
            lines.push(cols.join("\t"));
        } else {
            lines.push(line.to_string());
        }
    }
    assert_eq!(hits, 1, "フレーム {frame} の行が1つだけあるはず");
    let mut out = lines.join("\n");
    out.push('\n');
    fs::write(dtvi_path, out).expect("dtvi の書き戻し");
}

/// 完了条件: サムネイルの枚数が `.dtvi` のキーフレーム数と食い違うと、`index.html` を書かずに止まる。
/// 前回の `review/` はそのまま残り、一時ディレクトリも残さない。
#[test]
#[ignore = "要 ffmpeg とフィクスチャ（bash tests/fixtures/gen.sh）"]
fn review_with_thumbnail_count_mismatch_stops_and_keeps_previous_screen() {
    let fixture = common::logo_fixture_path();
    if skip_if_fixture_missing_at(&fixture) {
        return;
    }
    let root = make_tmp_dir("review-e2e-thumb-mismatch");
    let cache = root.join("cache");
    seed_cache(&cache, &fixture, &common::logo_dtvi_path(), "Trim(0,598)");

    let first = run_review(&cache, &fixture, None);
    assert!(
        first.status.success(),
        "1回目の review が失敗: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let dtvi_path = workdir::cached_dtvi_path(Some(&cache), &fixture).expect("dtvi のパス");
    let review_dir = workdir::cached_review_dir(Some(&cache), &fixture).expect("review のパス");
    let index = review_dir.join("index.html");
    let before = fs::read(&index).expect("1回目の index.html");

    // mp4 自体のキーフレームは5個のままなので、ffmpeg はサムネイルを5枚書き出す。
    // ずれるのは `.dtvi` 側で、そのキーフレームは4個になる。
    clear_key_flag(&dtvi_path, 480);
    let second = run_review(&cache, &fixture, None);
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(!second.status.success(), "枚数の不一致で失敗するはず");
    assert!(stderr.contains("書き出されたサムネイルが 5 枚"), "{stderr}");
    assert!(stderr.contains("キーフレームは 4 枚"), "{stderr}");

    let after = fs::read(&index).expect("前回の index.html が残っているはず");
    assert!(after == before, "前回の index.html が書き換えられている");

    let leftovers: Vec<_> = fs::read_dir(review_dir.parent().expect("親"))
        .expect("キャッシュ一覧")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with("review.tmp"))
        .collect();
    assert!(leftovers.is_empty(), "一時ディレクトリを残さないこと");

    let _ = fs::remove_dir_all(&root);
}

/// `DATA.keep`（表示順フレーム番号の半開区間）から trim 文字列を作る。
/// 隣接する区間（前の end と次の start が等しい）は1つにまとめ、各区間 `[s, e)` を
/// `Trim(s,e-1)` にして `" ++ "` で連結する。`src/review.html` の `command()` / `blocks()` と同じ規則。
fn trim_from_keep(keep: &[(u64, u64)]) -> String {
    let mut merged: Vec<(u64, u64)> = Vec::new();
    for &(start, end) in keep {
        match merged.last_mut() {
            Some(last) if last.1 == start => last.1 = end,
            _ => merged.push((start, end)),
        }
    }
    merged
        .iter()
        .map(|&(start, end)| format!("Trim({start},{})", end - 1))
        .collect::<Vec<_>>()
        .join(" ++ ")
}

/// `cut` を子プロセスで実行する。`trim_text` は標準入力から読ませる（`--trim -`）。
fn cut_with_stdin_trim(cache: &Path, input: &Path, trim_text: &str, output: &Path) -> Output {
    let mut child = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(cache)
        .arg("cut")
        .arg(input)
        .args(["--trim", "-", "-o"])
        .arg(output)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tachikaze cut の起動に失敗した");
    // 画面のコマンドは `<<<` で渡すので、末尾に改行を付ける。
    let mut stdin = child.stdin.take().expect("標準入力");
    stdin
        .write_all(format!("{trim_text}\n").as_bytes())
        .expect("標準入力への書き込み");
    drop(stdin);
    child.wait_with_output().expect("cut の終了待ち")
}

/// 完了条件: 画面の初期状態から出るコマンドで `cut` した結果が、同じ `trim.avs` を `cut` に渡した
/// 結果とパケット単位で一致する。キーフレーム境界に乗らない trim を使い、スナップが実際に起きることを確かめる。
#[test]
#[ignore = "要 ffmpeg とフィクスチャ（bash tests/fixtures/gen.sh）"]
fn review_copied_command_cuts_same_packets_as_trim_file() {
    let fixture = common::logo_fixture_path();
    if skip_if_fixture_missing_at(&fixture) {
        return;
    }
    let root = make_tmp_dir("review-e2e-command");
    let cache = root.join("cache");
    seed_cache(
        &cache,
        &fixture,
        &common::logo_dtvi_path(),
        // キーフレームは 0,120,240,360,480。総フレーム数は 599。
        "Trim(10,200) ++ Trim(450,598)",
    );

    let output = run_review(&cache, &fixture, None);
    assert!(
        output.status.success(),
        "review が失敗: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let index = PathBuf::from(String::from_utf8_lossy(&output.stdout).trim());
    let html = fs::read_to_string(&index).expect("index.html の読み込み");
    let start = html.find("const DATA = ").expect("DATA の開始") + "const DATA = ".len();
    let end = start + html[start..].find(";</script>").expect("DATA の終端");
    let data: serde_json::Value =
        serde_json::from_str(&html[start..end]).expect("DATA の JSON パース");
    let keep: Vec<(u64, u64)> = data["keep"]
        .as_array()
        .expect("keep は配列")
        .iter()
        .map(|pair| {
            (
                pair[0].as_u64().expect("start"),
                pair[1].as_u64().expect("end"),
            )
        })
        .collect();
    let from_screen = trim_from_keep(&keep);

    // 上の規則は画面の JS と同じ書き方でなければ意味がない。JS 側の式が変わったら気づくようにする。
    let review_js = include_str!("../src/review.html");
    assert!(
        review_js.contains("Trim(${frameAt(a)},${frameAt(b) - 1})"),
        "src/review.html の command() の書き方が変わった"
    );
    assert_eq!(
        from_screen, "Trim(0,239) ++ Trim(360,598)",
        "スナップが起きていない（元の trim と同じでは検査にならない）"
    );

    let cache_trim = workdir::cached_trim_path(Some(&cache), &fixture).expect("trim のパス");
    let out_screen = root.join("from_review.mp4");
    let out_trim = root.join("from_trim.mp4");
    let by_screen = cut_with_stdin_trim(&cache, &fixture, &from_screen, &out_screen);
    assert!(
        by_screen.status.success(),
        "画面のコマンド相当の cut が失敗: {}",
        String::from_utf8_lossy(&by_screen.stderr)
    );
    let by_file = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache)
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&cache_trim)
        .arg("-o")
        .arg(&out_trim)
        .output()
        .expect("tachikaze cut の起動に失敗した");
    assert!(
        by_file.status.success(),
        "trim.avs を渡した cut が失敗: {}",
        String::from_utf8_lossy(&by_file.stderr)
    );

    // md5 は使わない（SPS/PPS の再挿入でずれる）。ffprobe のパケット単位の CRC32 で比べる。
    for stream in ["v:0", "a:0"] {
        let entries = "packet=stream_index,pts,dts,duration,flags,data_hash";
        let a = ffprobe::csv_rows(Path::new("ffprobe"), &out_screen, stream, entries)
            .expect("画面由来の出力の ffprobe");
        let b = ffprobe::csv_rows(Path::new("ffprobe"), &out_trim, stream, entries)
            .expect("trim.avs 由来の出力の ffprobe");
        assert!(!a.is_empty(), "{stream} のパケットが空");
        // 時刻と長さだけの比較に落ちないよう、CRC32 が各行に入っていることを確かめる。
        assert!(
            a.iter().all(|row| row.contains("CRC32:")),
            "{stream} に CRC32 が無い行がある（data_hash の指定が落ちている）: {a:?}"
        );
        assert_eq!(a, b, "{stream} のパケットが一致しない");
    }

    let _ = fs::remove_dir_all(&root);
}
