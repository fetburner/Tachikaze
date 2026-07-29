//! [E6-3] 音声パケットのビットコピーを集合比較で検証する。
//!
//! 映像と違い、音声は境界パケットの選択が実装によって変わりうるため、出力を
//! 「列」として入力と比較すること（`assert_eq!(got, want)` のような完全一致）は
//! できない。代わりに docs/lossless-cut.md「正しい方法」節が示す通り、
//! **出力の全パケットが入力に存在するか**を集合比較で確認する:
//!
//! ```console
//! $ ffprobe ... IN.mp4  | sort -u > src.txt
//! $ ffprobe ... OUT.mp4 | sort -u > out.txt
//! $ comm -23 out.txt src.txt | wc -l      # 0 なら全パケットがビットコピー
//! ```
//!
//! `comm -23` 相当の処理はこのファイル内で `HashSet` を使って実装し
//! （[`packets_only_in_output`]）、ffprobe を必要とせず unit test で検証できるようにする。
//!
//! 集合比較はパケットの**順序や重複**を検出できないため、順序の正しさは
//! 「出力の音声パケットの dts が単調増加」を別途 assert して補う
//! （[`is_strictly_increasing`]）。
//!
//! # なぜ実際の cut 実行を伴う e2e テストが `#[ignore]` のままなのか
//!
//! この issue の時点で:
//! - `main.rs` の `Commands::Cut { .. }` はまだ `unimplemented!()` であり、CLI の
//!   `cut` サブコマンドは配線されていない。
//! - `Cargo.toml` に `[lib]` ターゲットが無いため、`tests/`（別クレート扱い）から
//!   `src/` の `pub` 関数（`mp4io::read` / `plan` / `audio::select_audio_segments` /
//!   `mp4io::write::write_mp4` など）を直接呼ぶことができない。
//!
//! そのため、実際にカット処理を実行して検証する部分
//! （[`cut_audio_is_bitwise_copy_and_matches_expected_count`]）は、配線後に
//! `tachikaze` バイナリ（`CARGO_BIN_EXE_tachikaze`）を起動する形の統合テストとして
//! 書きつつ `#[ignore]` のプレースホルダに留める。集合比較・パース・順序検証の
//! ロジック自体は本ファイル内で完結する独立関数として実装し、実ファイルが無くても
//! （`ffprobe` の出力を模した文字列だけで）unit test で検証できるようにしている。
//!
//! 加えて、`ffprobe` 呼び出しと CRC32 のパース処理そのものは
//! [`ffprobe_wrapper_round_trips_on_real_fixture`] で実フィクスチャ・実 `ffprobe`
//! を使って検証する（`cut` の配線を待たずに今すぐ実行できる）。

mod common;

use std::collections::HashSet;
use std::path::Path;
use std::process::Command;

// --- 集合比較・パース・順序検証（ffprobe 不要、実ファイル不要） ---

/// `ffprobe -show_entries packet=data_hash -show_data_hash CRC32 -of csv=p=0` の
/// 出力（1行1パケット、`CRC32:xxxxxxxx` 形式）をパースして集合にする。
///
/// 空行は無視する。`sort -u` 相当（重複は `HashSet` で自然に畳み込まれる）。
fn parse_crc_set(ffprobe_output: &str) -> HashSet<String> {
    ffprobe_output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect()
}

/// `comm -23 out.txt src.txt` 相当: `out` にあって `src` に無い要素を返す
/// （＝出力にしか存在しないパケット。0件なら全パケットがビットコピー）。
///
/// 結果は決定的な順序にするためソートして返す。
fn packets_only_in_output<'a>(out: &'a HashSet<String>, src: &'a HashSet<String>) -> Vec<&'a str> {
    let mut diff: Vec<&str> = out.difference(src).map(String::as_str).collect();
    diff.sort_unstable();
    diff
}

/// 値の列が厳密に単調増加であることを確認する。
///
/// 集合比較はパケットの順序や重複を検出できないため、音声パケットの
/// dts（またはpts）の並びが正しいことはこちらで別途確認する。
fn is_strictly_increasing(values: &[i64]) -> bool {
    values.windows(2).all(|pair| pair[0] < pair[1])
}

/// 差分（出力にしか無いパケット）が見つかったときの報告メッセージを組み立てる。
///
/// 「差分があった場合、出力側にしか存在するパケットの個数と最初の数個の CRC を出す」
/// という要件を満たすためのヘルパ。
fn describe_diff(diff: &[&str]) -> String {
    const PREVIEW_LEN: usize = 5;
    let preview: Vec<&str> = diff.iter().take(PREVIEW_LEN).copied().collect();
    format!(
        "出力にしか存在しない音声パケットが {} 件ある。先頭{}件: {:?}",
        diff.len(),
        preview.len(),
        preview
    )
}

/// `src/audio.rs::select_audio_segments` と同じ丸めアルゴリズムの、テスト側での
/// 独立再実装（累積誤差が蓄積しないことは src 側の unit test で既に検証済み。
/// ここでは e2e テストの期待値を、実装本体を呼ばずに算出するためだけに使う）。
///
/// 戻り値は各出力区間の終端パケット数（累積、`AudioSegment.end` 相当）の列。
/// 区間ごとの長さ（`AudioSegment.end - start` の合計）は最後の要素に等しい
/// （区間が先頭からテレスコープするため）。
fn reference_cumulative_audio_packet_ends(
    video_segment_durations: &[u64],
    video_timescale: u32,
    frame_size: u64,
    total_audio_samples: u64,
    audio_timescale: u32,
) -> Vec<u64> {
    let mut cumulative_video_time: u64 = 0;
    let mut ends = Vec::with_capacity(video_segment_durations.len());
    for &duration in video_segment_durations {
        cumulative_video_time += duration;
        let ideal_packets = (cumulative_video_time as f64 * audio_timescale as f64)
            / (video_timescale as f64 * frame_size as f64);
        let packet_count = (ideal_packets.round() as u64).min(total_audio_samples);
        ends.push(packet_count);
    }
    ends
}

#[cfg(test)]
mod pure_logic_tests {
    use super::*;

    #[test]
    fn parse_crc_set_dedupes_and_ignores_blank_lines() {
        let output = "CRC32:aaaa0001\nCRC32:aaaa0002\nCRC32:aaaa0001\n\n";
        let set = parse_crc_set(output);
        assert_eq!(set.len(), 2);
        assert!(set.contains("CRC32:aaaa0001"));
        assert!(set.contains("CRC32:aaaa0002"));
    }

    #[test]
    fn parse_crc_set_of_empty_input_is_empty() {
        assert!(parse_crc_set("").is_empty());
        assert!(parse_crc_set("\n\n   \n").is_empty());
    }

    #[test]
    fn packets_only_in_output_is_empty_when_output_is_subset_of_source() {
        let src: HashSet<String> = ["CRC32:1", "CRC32:2", "CRC32:3"]
            .into_iter()
            .map(String::from)
            .collect();
        let out: HashSet<String> = ["CRC32:2", "CRC32:1"]
            .into_iter()
            .map(String::from)
            .collect();

        let diff = packets_only_in_output(&out, &src);
        assert!(diff.is_empty(), "diff={diff:?}");
    }

    /// 完了条件: 「意図的に壊した出力で差分が検出されることを確認する」。
    /// 出力側の CRC32 集合に元ファイルに存在しない値を混ぜ、検出できることを示す。
    #[test]
    fn packets_only_in_output_detects_intentionally_corrupted_packet() {
        let src: HashSet<String> = ["CRC32:1", "CRC32:2", "CRC32:3"]
            .into_iter()
            .map(String::from)
            .collect();
        let out: HashSet<String> = ["CRC32:1", "CRC32:2", "CRC32:deadbeef"]
            .into_iter()
            .map(String::from)
            .collect();

        let diff = packets_only_in_output(&out, &src);
        assert_eq!(diff, vec!["CRC32:deadbeef"]);

        let message = describe_diff(&diff);
        assert!(message.contains('1'), "件数が出ていない: {message}");
        assert!(
            message.contains("CRC32:deadbeef"),
            "先頭のCRCが出ていない: {message}"
        );
    }

    #[test]
    fn packets_only_in_output_reports_multiple_corrupted_packets_sorted() {
        let src: HashSet<String> = ["CRC32:1"].into_iter().map(String::from).collect();
        let out: HashSet<String> = ["CRC32:1", "CRC32:zzz", "CRC32:aaa"]
            .into_iter()
            .map(String::from)
            .collect();

        let diff = packets_only_in_output(&out, &src);
        // ソート済み、決定的な順序であること。
        assert_eq!(diff, vec!["CRC32:aaa", "CRC32:zzz"]);
    }

    #[test]
    fn is_strictly_increasing_accepts_monotonic_sequence() {
        assert!(is_strictly_increasing(&[0, 960, 1920, 2880]));
        assert!(is_strictly_increasing(&[])); // 空・単一要素は自明に真
        assert!(is_strictly_increasing(&[42]));
    }

    #[test]
    fn is_strictly_increasing_rejects_duplicate_or_out_of_order() {
        // 集合比較では検出できない「重複」。
        assert!(!is_strictly_increasing(&[0, 960, 960, 1920]));
        // 集合比較では検出できない「順序の入れ替わり」。
        assert!(!is_strictly_increasing(&[0, 1920, 960]));
        // 逆順。
        assert!(!is_strictly_increasing(&[100, 50, 0]));
    }

    /// `src/audio.rs::select_audio_segments` の `clamps_to_total_audio_samples`
    /// テストと同じ入力を使い、本ファイルの再実装が同じ結果になることを確認する
    /// （テスト側オラクルの信頼性チェック）。
    #[test]
    fn reference_matches_select_audio_segments_clamps_to_total_case() {
        let video_timescale = 30_000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48_000u32;
        let frame_size = 960u64;
        let total_audio_samples = 10u64;

        let video_segment_durations = vec![frame_duration * 10_000];

        let ends = reference_cumulative_audio_packet_ends(
            &video_segment_durations,
            video_timescale,
            frame_size,
            total_audio_samples,
            audio_timescale,
        );

        // src/audio.rs の同名テストでは segments[0] == { start: 0, end: 10 } になる。
        assert_eq!(ends, vec![10]);
    }

    /// `src/audio.rs::select_audio_segments` の `works_with_44100_audio_timescale`
    /// テストと同じ入力（frame_size=882, audio_timescale=44100, 6区間×20フレーム）
    /// で、累積誤差が1パケット未満に収まる（区間数が一致し、単調非減少である）ことを
    /// 確認する。
    #[test]
    fn reference_matches_select_audio_segments_44100_case() {
        let video_timescale = 30_000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 44_100u32;
        let frame_size = 882u64;
        let total_audio_samples = 500u64;

        let frames_per_segment = 20u64;
        let segment_duration = frames_per_segment * frame_duration;
        let video_segment_durations = vec![segment_duration; 6];

        let ends = reference_cumulative_audio_packet_ends(
            &video_segment_durations,
            video_timescale,
            frame_size,
            total_audio_samples,
            audio_timescale,
        );

        assert_eq!(ends.len(), 6);
        let mut prev = 0u64;
        for end in &ends {
            assert!(*end >= prev, "非減少であること: ends={ends:?}");
            prev = *end;
        }
    }
}

// --- ffprobe ラッパ（実行には ffprobe が必要） ---

fn skip_if_missing(bin: &str) -> bool {
    match Command::new(bin).arg("-version").output() {
        Ok(output) if output.status.success() => false,
        _ => {
            eprintln!("{bin} が無いためスキップします。");
            true
        }
    }
}

/// 音声ストリームの全パケットの CRC32 集合を ffprobe で取得する。
fn ffprobe_audio_crc_set(path: &Path) -> HashSet<String> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=data_hash",
            "-show_data_hash",
            "CRC32",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe の起動に失敗した（PATH を確認）");
    assert!(
        output.status.success(),
        "ffprobe が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    parse_crc_set(&String::from_utf8_lossy(&output.stdout))
}

/// 音声ストリームの全パケットの dts を格納順に取得する。
fn ffprobe_audio_dts(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "a:0",
            "-show_entries",
            "packet=dts",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe の起動に失敗した");
    assert!(
        output.status.success(),
        "ffprobe が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.parse::<i64>()
                .unwrap_or_else(|_| panic!("dts が整数としてパースできない: {line:?}"))
        })
        .collect()
}

fn ffprobe_scalar_stream_entry(path: &Path, stream_selector: &str, entry: &str) -> String {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            stream_selector,
            "-show_entries",
            entry,
            "-of",
            "default=nk=1:nw=1",
        ])
        .arg(path)
        .output()
        .expect("ffprobe の起動に失敗した");
    assert!(
        output.status.success(),
        "ffprobe が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// `IN.mp4` に対して ffprobe の CRC32 ラッパと集合比較ロジックが実際に動くことを、
/// 実フィクスチャ・実 `ffprobe` を使って確認する。
///
/// `cut` サブコマンドの配線を待たずに今すぐ実行できる（fixture が無い環境では
/// 自動でスキップする。`tests/fixtures/gen.sh` を参照）。
#[test]
fn ffprobe_wrapper_round_trips_on_real_fixture() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffprobe") {
        return;
    }

    let fixture = common::fixture_path();

    let crc_set = ffprobe_audio_crc_set(&fixture);
    assert!(!crc_set.is_empty(), "音声パケットが1つも見つからない");

    // 自分自身との比較なので、差分は0件のはず（parse/集合比較ロジックの疎通確認）。
    let diff = packets_only_in_output(&crc_set, &crc_set);
    assert!(diff.is_empty(), "自己比較で差分が出た: {diff:?}");

    // 集合に無い値を1つ混ぜれば、必ず検出できる。
    let mut corrupted = crc_set.clone();
    corrupted.insert("CRC32:deadbeef".to_string());
    let diff = packets_only_in_output(&corrupted, &crc_set);
    assert_eq!(diff, vec!["CRC32:deadbeef"]);

    // dts は格納順に単調増加のはず。
    //
    // 注意: `crc_set` は `sort -u` 相当で重複を畳み込んだ集合なので、
    // 一定周波数のサイン波（このフィクスチャの音声）は同一内容のパケットを
    // 多数含みうり、`crc_set.len() < dts.len()`（総パケット数）になりうる。
    // これはバグではなく、まさにこの issue が「列としての完全一致ではなく
    // 集合としての部分集合関係」を検証方法に選んだ理由そのものである。
    let dts = ffprobe_audio_dts(&fixture);
    assert!(
        crc_set.len() <= dts.len(),
        "CRC32集合の要素数が総パケット数を超えている（あり得ないはず）: \
         crc_set.len()={}, dts.len()={}",
        crc_set.len(),
        dts.len()
    );
    assert!(
        is_strictly_increasing(&dts),
        "フィクスチャ自体の音声dtsが単調増加でない: {dts:?}"
    );
}

// --- 実際に cut を実行する e2e テスト（配線待ちのプレースホルダ） ---

/// フィクスチャ (`tests/fixtures/sample.mp4`, GOP=120 固定, 30000/1001fps, docs/lossless-cut.md
/// 前提と同じ) の先頭2GOP分を2区間に分けて keep する Trim リスト:
/// `Trim(0,119) ++ Trim(240,359)`（キーフレーム境界ちょうどなので snap による移動は無い）。
const TRIM_AVS_CONTENT: &str = "Trim(0,119) ++ Trim(240,359)";

/// 完了条件:
/// - フィクスチャで差分0件（出力の全音声パケットが入力に存在する）
/// - 出力の音声パケット数が `select_audio_segments` の計算結果（
///   `reference_cumulative_audio_packet_ends` で独立に再計算した期待値）と一致する
/// - 出力の音声パケットの dts が単調増加である
///
/// `main.rs` の `Commands::Cut { .. }` が `unimplemented!()` のままであり、かつこの
/// クレートに `[lib]` ターゲットが無く `tests/` から `src/` の関数を直接呼べないため、
/// 現時点ではこのテストを実行すると `tachikaze cut` の起動そのものが失敗する
/// （パニックで終了する）。CLI 配線が完了したら `#[ignore]` を外して有効化する。
#[test]
#[ignore = "cut サブコマンドがまだ CLI に配線されていない（main.rs の unimplemented!()）。配線後に有効化する"]
fn cut_audio_is_bitwise_copy_and_matches_expected_count() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe")
    {
        return;
    }

    let fixture = common::fixture_path();

    let tmp_dir = std::env::temp_dir().join(format!("tachikaze-audio-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    std::fs::write(&trim_path, TRIM_AVS_CONTENT).expect("trim.avs を書けること");

    let status = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .status()
        .expect("tachikaze cut の起動に失敗した");
    assert!(
        status.success(),
        "tachikaze cut が失敗した: status={status:?}"
    );

    // --- 完了条件1: 差分0件（集合比較） ---
    let src_set = ffprobe_audio_crc_set(&fixture);
    let out_set = ffprobe_audio_crc_set(&out_path);
    let diff = packets_only_in_output(&out_set, &src_set);
    assert!(diff.is_empty(), "{}", describe_diff(&diff));

    // --- 完了条件2: 出力の音声パケット数が期待値と一致する ---
    let video_timescale: u32 = ffprobe_scalar_stream_entry(&fixture, "v:0", "stream=time_base")
        .rsplit('/')
        .next()
        .expect("time_base の書式が想定と違う")
        .parse()
        .expect("video timescale が整数としてパースできない");
    let video_frame_duration: u64 = ffprobe_scalar_stream_entry(&fixture, "v:0", "packet=duration")
        .lines()
        .next()
        .expect("映像パケットが1つも無い")
        .parse()
        .expect("映像パケットの duration が整数としてパースできない");
    let audio_timescale: u32 = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=sample_rate")
        .parse()
        .expect("sample_rate が整数としてパースできない");
    let frame_size: u64 = ffprobe_scalar_stream_entry(&fixture, "a:0", "packet=duration")
        .lines()
        .next()
        .expect("音声パケットが1つも無い")
        .parse()
        .expect("音声パケットの duration が整数としてパースできない");
    let total_audio_samples = ffprobe_audio_dts(&fixture).len() as u64;

    // Trim(0,119) ++ Trim(240,359): 120フレームずつ2区間。
    let video_segment_durations = vec![120 * video_frame_duration, 120 * video_frame_duration];

    let expected_ends = reference_cumulative_audio_packet_ends(
        &video_segment_durations,
        video_timescale,
        frame_size,
        total_audio_samples,
        audio_timescale,
    );
    let expected_packet_count = *expected_ends.last().expect("区間が1つも無い");

    let got_packet_count = out_set.len() as u64;
    // 集合の要素数はハッシュの衝突が無い限りパケット数と一致するはずだが、より
    // 直接的にパケット数そのもの（dts の行数）でも確認する。
    let got_dts = ffprobe_audio_dts(&out_path);
    assert_eq!(
        got_dts.len() as u64,
        expected_packet_count,
        "出力の音声パケット数が期待値と一致しない（dtsの行数で確認）"
    );
    assert!(
        got_packet_count <= got_dts.len() as u64,
        "CRC32集合の要素数がパケット数を超えている（あり得ないはず）"
    );

    // --- 完了条件3: 出力の音声パケットの dts が単調増加である ---
    assert!(
        is_strictly_increasing(&got_dts),
        "出力の音声パケットのdtsが単調増加でない: {got_dts:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
