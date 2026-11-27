//! `mark` サブコマンドの E2E。
//!
//! 既定で走るテスト（ffmpeg・フィクスチャ不要）は、ffmpeg に届く前に止まる入力だけを使う。
//! `#[ignore]` 付きのテストはフィクスチャと ffmpeg / ffprobe が必要。

#[allow(dead_code)]
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use common::{make_tmp_dir, skip_if_fixture_missing_at};
use tachikaze::marker::{self, Markers};
use tachikaze::trim::TrimList;
use tachikaze::{dtvi, ffprobe, tools};

/// `mark` を子プロセスで起動する。`cache` を与えると `--cache-dir` を付ける。
fn run_mark(cache: Option<&Path>, args: &[&str]) -> Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
    if let Some(cache) = cache {
        cmd.arg("--cache-dir").arg(cache);
    }
    cmd.arg("mark").args(args);
    cmd.output().expect("tachikaze mark の起動に失敗した")
}

fn stderr_of(out: &Output) -> String {
    String::from_utf8_lossy(&out.stderr).into_owned()
}

/// 中身を使わない入力ファイル（検査で止まるまでの経路だけを通すため）。
fn dummy_input(dir: &Path) -> PathBuf {
    let input = dir.join("IN.mp4");
    fs::write(&input, b"not really an mp4").expect("入力の作成");
    input
}

/// Trim の内容を `dir/trim.avs` に書き、そのパスを返す（`--trim` はファイルパスを取る）。
fn write_trim(dir: &Path, text: &str) -> PathBuf {
    let path = dir.join("trim.avs");
    fs::write(&path, text).expect("trim.avs の作成");
    path
}

/// 表示順 `[0, 599)` を、Trim の補集合の CM が前後と間に挟む Trim。
const TRIM: &str = "Trim(60,299) ++ Trim(420,539)";

/// `sample.mp4`（599 フレーム）に対応する `.dtvi`。
fn full_dtvi_path() -> PathBuf {
    common::no_logo_dtvi_path()
}

/// [`TRIM`] と [`full_dtvi_path`] から [`marker::markers`] が作る期待値。
fn expected_markers() -> Markers {
    let dtvi = dtvi::parse(&fs::read_to_string(full_dtvi_path()).unwrap()).unwrap();
    let trim = TrimList::parse(TRIM).unwrap();
    marker::markers(&trim, &dtvi).expect("期待値の生成")
}

// --- ffmpeg / フィクスチャを使わないテスト ---

#[test]
fn output_equal_to_input_is_rejected_before_anything_is_written() {
    let dir = make_tmp_dir("mark-e2e-same-file");
    let input = dummy_input(&dir);
    let input_str = input.to_str().unwrap();
    let out = run_mark(
        Some(&dir.join("cache")),
        &[
            input_str,
            "--trim",
            write_trim(&dir, "Trim(0,4)").to_str().unwrap(),
            "--dtvi",
            full_dtvi_path().to_str().unwrap(),
            "-o",
            input_str,
        ],
    );
    assert!(!out.status.success(), "入力と同じ -o は拒否されるべき");
    assert!(
        stderr_of(&out).contains("入力と同じ"),
        "{}",
        stderr_of(&out)
    );
    assert_eq!(fs::read(&input).unwrap(), b"not really an mp4");
}

#[test]
fn dash_output_is_rejected() {
    let dir = make_tmp_dir("mark-e2e-dash");
    let input = dummy_input(&dir);
    let out = run_mark(
        Some(&dir.join("cache")),
        &[
            input.to_str().unwrap(),
            "--trim",
            write_trim(&dir, "Trim(0,4)").to_str().unwrap(),
            "--dtvi",
            full_dtvi_path().to_str().unwrap(),
            "-o",
            "-",
        ],
    );
    assert!(!out.status.success());
    assert!(stderr_of(&out).contains("`-`"), "{}", stderr_of(&out));
}

#[test]
fn dtvi_with_mismatched_frame_count_is_rejected_without_writing() {
    // tests/data/sample.dtvi は抜粋で、ヘッダの frame_count と行数が合わない。
    let dir = make_tmp_dir("mark-e2e-frame-count");
    let input = dummy_input(&dir);
    let out = run_mark(
        Some(&dir.join("cache")),
        &[
            input.to_str().unwrap(),
            "--trim",
            write_trim(&dir, "Trim(0,4)").to_str().unwrap(),
            "--dtvi",
            common::dtvi_path().to_str().unwrap(),
            "--edl-only",
        ],
    );
    assert!(!out.status.success());
    assert!(
        stderr_of(&out).contains("frame_count"),
        "{}",
        stderr_of(&out)
    );
    assert!(
        !dir.join("IN.edl").exists(),
        "検査で止まった後に EDL を書いてはいけない"
    );
}

#[test]
fn missing_cached_trim_stops_with_analyze_hint() {
    let dir = make_tmp_dir("mark-e2e-no-trim");
    let input = dummy_input(&dir);
    let out = run_mark(
        Some(&dir.join("cache")),
        &[
            input.to_str().unwrap(),
            "--dtvi",
            full_dtvi_path().to_str().unwrap(),
            "--edl-only",
        ],
    );
    assert!(!out.status.success());
    let err = stderr_of(&out);
    assert!(err.contains("trim.avs"), "{err}");
    assert!(
        err.contains("analyze"),
        "analyze のコマンド例が出るべき: {err}"
    );
}

// --- ffmpeg / ffprobe とフィクスチャが要るテスト ---

/// `ffprobe -show_chapters` の開始秒とタイトルを読む。
fn chapters_of(path: &Path) -> Vec<(f64, String)> {
    let out = Command::new(tools::resolve_tool(tools::FFPROBE).expect("ffprobe が必要"))
        .args(["-v", "error", "-show_chapters", "-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe の起動");
    assert!(out.status.success());
    let json: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    json["chapters"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["start_time"].as_str().unwrap().parse().unwrap(),
                c["tags"]["title"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

/// `ffprobe` の `stream=index,codec_type` の一覧（`index,codec_type` の組）。
fn stream_list(path: &Path) -> Vec<(String, String)> {
    let out = Command::new(tools::resolve_tool(tools::FFPROBE).expect("ffprobe が必要"))
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=index,codec_type",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe の起動");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|line| {
            let (idx, kind) = line.split_once(',').expect("index,codec_type");
            (idx.to_string(), kind.to_string())
        })
        .collect()
}

/// ストリームごとの、格納順のパケット CRC32 列。
fn packet_hashes(path: &Path) -> Vec<Vec<String>> {
    let probe = tools::resolve_tool(tools::FFPROBE).expect("ffprobe が必要");
    stream_list(path)
        .iter()
        .map(|(idx, _)| ffprobe::csv_rows(&probe, path, idx, "packet=data_hash").unwrap())
        .collect()
}

/// チャプター時刻の許容幅（秒）。
/// mp4 の chapter 時刻は muxer が timescale（実測の出力では `1/240000`）に丸める。
/// 実測（`sample.mp4` の5境界、`.dtvi` の time_base `1/30000`）の差は表示桁（`1/240000`）の
/// 範囲で0だった。許容幅 2 ms はその丸めに対する余裕で、フレーム1枚（約 33 ms）より十分小さい。
const CHAPTER_TOLERANCE_SECS: f64 = 0.002;

#[test]
#[ignore = "要 ffmpeg / ffprobe とフィクスチャ（bash tests/fixtures/gen.sh）"]
fn output_has_chapters_matching_dtvi_and_keeps_all_packets() {
    let fixture = common::fixture_path();
    if skip_if_fixture_missing_at(&fixture) {
        return;
    }
    let dir = make_tmp_dir("mark-e2e-full");
    let input = dir.join("IN.mp4");
    fs::copy(&fixture, &input).unwrap();
    let before = fs::read(&input).unwrap();
    let out_path = dir.join("OUT.mp4");

    let out = run_mark(
        Some(&dir.join("cache")),
        &[
            input.to_str().unwrap(),
            "--trim",
            write_trim(&dir, TRIM).to_str().unwrap(),
            "--dtvi",
            full_dtvi_path().to_str().unwrap(),
            "-o",
            out_path.to_str().unwrap(),
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));

    // 1. チャプター境界が .dtvi の表示順フレームの pts と許容幅で一致する。
    let dtvi = dtvi::parse(&fs::read_to_string(full_dtvi_path()).unwrap()).unwrap();
    let num: f64 = dtvi.header_value("time_base_num").unwrap().parse().unwrap();
    let den: f64 = dtvi.header_value("time_base_den").unwrap().parse().unwrap();
    let expected = expected_markers();
    let expected_starts: Vec<f64> = expected
        .ffmetadata
        .lines()
        .filter_map(|l| l.strip_prefix("START="))
        .map(|v| v.parse::<f64>().unwrap() * num / den)
        .collect();
    let expected_titles: Vec<&str> = expected
        .ffmetadata
        .lines()
        .filter_map(|l| l.strip_prefix("title="))
        .collect();
    let chapters = chapters_of(&out_path);
    assert_eq!(chapters.len(), expected_starts.len(), "{chapters:?}");
    for (i, (start, title)) in chapters.iter().enumerate() {
        assert!(
            (start - expected_starts[i]).abs() <= CHAPTER_TOLERANCE_SECS,
            "チャプター {i}: {start} と期待値 {} の差が許容幅を超えた",
            expected_starts[i]
        );
        assert_eq!(title, expected_titles[i]);
    }

    // 2. 入力の全ストリームのパケット CRC32 が出力と一致する（順序も含む）。出力には
    //    章トラック（mp4 では codec_type `data` の text トラック）が1本増えるので、その
    //    1本は末尾に来ることだけを確かめて比較からは外す。
    let in_streams = stream_list(&input);
    let out_streams = stream_list(&out_path);
    let n = in_streams.len();
    assert_eq!(
        out_streams.len(),
        n + 1,
        "入力のストリームに章トラック1本を足した数: {out_streams:?}"
    );
    assert_eq!(out_streams[n].1, "data", "増えたトラックは章トラックのはず");
    assert_eq!(
        in_streams.iter().map(|s| &s.1).collect::<Vec<_>>(),
        out_streams[..n].iter().map(|s| &s.1).collect::<Vec<_>>(),
        "入力と出力のストリーム種別の並び"
    );
    assert_eq!(packet_hashes(&input), packet_hashes(&out_path)[..n]);

    // 3. EDL は marker の出力と一致し、mp4 の隣に置かれる。一時ファイルは残らない。
    assert_eq!(
        fs::read_to_string(dir.join("OUT.edl")).expect("OUT.edl"),
        expected.edl
    );
    assert!(
        !dir.join(".OUT.mp4.tmp.mp4").exists(),
        "一時ファイルが残っている"
    );

    // 4. 入力は変わっていない。
    assert_eq!(fs::read(&input).unwrap(), before);
}

#[test]
#[ignore = "要 ffmpeg / ffprobe とフィクスチャ（bash tests/fixtures/gen.sh）"]
fn edl_only_writes_next_to_input_and_leaves_no_mp4() {
    let fixture = common::fixture_path();
    if skip_if_fixture_missing_at(&fixture) {
        return;
    }
    let dir = make_tmp_dir("mark-e2e-edl-only");
    let input = dir.join("REC.mp4");
    fs::copy(&fixture, &input).unwrap();

    let out = run_mark(
        Some(&dir.join("cache")),
        &[
            input.to_str().unwrap(),
            "--trim",
            write_trim(&dir, TRIM).to_str().unwrap(),
            "--dtvi",
            full_dtvi_path().to_str().unwrap(),
            "--edl-only",
        ],
    );
    assert!(out.status.success(), "{}", stderr_of(&out));
    assert_eq!(
        fs::read_to_string(dir.join("REC.edl")).expect("REC.edl"),
        expected_markers().edl
    );
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.ends_with(".mp4") || n.ends_with(".edl"))
        .collect();
    names.sort();
    assert_eq!(names, ["REC.edl", "REC.mp4"], "mp4 を作ってはいけない");
}
