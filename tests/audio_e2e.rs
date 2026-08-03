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
//! # テストの構成
//!
//! `Cargo.toml` に `[lib]` ターゲットが無いため、`tests/`（別クレート扱い）から
//! `src/` の関数を直接呼ぶことはできない。そのため実際のカット処理は
//! `tachikaze` バイナリ（`CARGO_BIN_EXE_tachikaze`）を起動する統合テストとして
//! 検証する（[`cut_audio_is_bitwise_copy_and_matches_expected_count`] と
//! [`corrupted_audio_packet_is_detected_by_set_comparison`]）。これらは
//! フィクスチャと `ffmpeg`/`ffprobe` を要するため `#[ignore]` を付けている。
//!
//! 集合比較・パース・順序検証・期待パケット数の算出ロジックは本ファイル内で完結する
//! 独立関数として実装し、実ファイルが無くても（`ffprobe` の出力を模した文字列だけで）
//! unit test で検証できるようにしている。加えて `ffprobe` 呼び出しと CRC32 の
//! パース処理そのものは [`ffprobe_wrapper_round_trips_on_real_fixture`] で
//! 実フィクスチャ・実 `ffprobe` を使って検証する。
//!
//! # `cut` に `--dtvi` が必要な理由
//!
//! `cut` はオープン GOP かどうかを `.dtvi` のフレーム表からしか判定できないため、
//! `mp4io::support::check_supported` が `.dtvi` を必須にしている（#36 の決定）。
//! したがってバイナリを起動する際は `--dtvi` を渡す必要がある。ここでは
//! `tests/data/sample.dtvi`（このフィクスチャと同じ手順で作った mp4 に対する
//! 実 `dtvindex` 出力の抜粋）を使う。

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
/// 再実装（e2e テストの期待値を、実装本体を呼ばずに算出するために使う）。
///
/// 入力は `src` 側の `SampleInfo` ではなく **`ffprobe` から取った実パケット長の列**
/// なので、データの取得経路は実装本体と独立している。
///
/// # 固定の 1 パケット長で割ってはいけない
///
/// 素朴には `累積映像時間 × audio_timescale / (video_timescale × frame_size)` を
/// 丸めれば済みそうに見えるが、**これは実データで誤った期待値を出す。** エンコーダの
/// プライミング分だけ先頭パケットが他より大幅に長いことがあるためで、
/// `tests/fixtures/sample.mp4` でも先頭だけ `duration=3852`（他は `960`）になっている。
/// 固定長で割ると 400、実 duration を累積すると 397 で、後者が正しい。
///
/// そのため `src/audio.rs` と同様に、各パケットの実 duration を先頭から累積し、
/// 目標時刻に最も近い累積位置を探す方式にする。
///
/// # 【最重要】区間ごとの「ソース上の絶対開始時刻」から引き直す
///
/// 過去の実装は「出力タイムライン上の累積映像時間」を起点にしており、2区間目以降の
/// 音声が常にソースの先頭側へ先行するバグがあった
/// （docs/lossless-cut.md「実際に起きた誤り」節）。この再実装がそのバグを見逃さない
/// よう、`video_segment_source_starts`（各区間のソース上の絶対開始時刻）を別途受け取り、
/// 区間ごとに毎回そこから独立に目標時刻を計算する（出力の累積時間は一切使わない）。
///
/// 戻り値は各出力区間の音声パケット範囲 `[start, end)` の列（`AudioSegment` 相当）。
fn reference_position_based_audio_packet_ranges(
    video_segment_durations: &[u64],
    video_segment_source_starts: &[u64],
    video_timescale: u32,
    audio_packet_durations: &[u64],
    audio_timescale: u32,
) -> Vec<(u64, u64)> {
    // cumulative[i] は audio_packet_durations[0..i] の合計（cumulative[0] == 0）。
    let mut cumulative: Vec<u64> = Vec::with_capacity(audio_packet_durations.len() + 1);
    cumulative.push(0);
    for &d in audio_packet_durations {
        cumulative.push(cumulative.last().copied().unwrap_or(0) + d);
    }
    let total_audio_samples = audio_packet_durations.len() as u64;

    video_segment_durations
        .iter()
        .zip(video_segment_source_starts.iter())
        .map(|(&duration, &source_start)| {
            let source_end = source_start + duration;

            let start_target =
                (source_start as f64 * audio_timescale as f64) / video_timescale as f64;
            let end_target = (source_end as f64 * audio_timescale as f64) / video_timescale as f64;

            let start = reference_nearest_cumulative_index(&cumulative, start_target)
                .min(total_audio_samples);
            let end = reference_nearest_cumulative_index(&cumulative, end_target)
                .min(total_audio_samples)
                .max(start);
            (start, end)
        })
        .collect()
}

/// 区間が（CM を挟まず）ソース先頭から連続しているケースのための
/// `video_segment_source_starts` を組み立てる（`durations[k]` の直前までの累積時間が
/// `k` 番目の区間のソース開始時刻になる）。`src/audio.rs` の同名ヘルパと同じ考え方。
fn contiguous_source_starts(durations: &[u64]) -> Vec<u64> {
    let mut starts = Vec::with_capacity(durations.len());
    let mut acc = 0u64;
    for &d in durations {
        starts.push(acc);
        acc += d;
    }
    starts
}

/// 昇順の `cumulative` の中から `target` に最も近い要素のインデックスを返す。
fn reference_nearest_cumulative_index(cumulative: &[u64], target: f64) -> u64 {
    let idx = cumulative.partition_point(|&c| (c as f64) < target);
    if idx == 0 {
        return 0;
    }
    if idx >= cumulative.len() {
        return (cumulative.len() - 1) as u64;
    }
    let before = cumulative[idx - 1] as f64;
    let after = cumulative[idx] as f64;
    if (target - before).abs() <= (after - target).abs() {
        (idx - 1) as u64
    } else {
        idx as u64
    }
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
        let audio_packet_durations = vec![960u64; 10];

        let video_segment_durations = vec![frame_duration * 10_000];
        let video_segment_source_starts = vec![0u64];

        let ranges = reference_position_based_audio_packet_ranges(
            &video_segment_durations,
            &video_segment_source_starts,
            video_timescale,
            &audio_packet_durations,
            audio_timescale,
        );

        // src/audio.rs の同名テストでは segments[0] == { start: 0, end: 10 } になる。
        assert_eq!(ranges, vec![(0, 10)]);
    }

    /// `src/audio.rs::select_audio_segments` の `works_with_44100_audio_timescale`
    /// テストと同じ入力（frame_size=882, audio_timescale=44100, 6区間×20フレーム、
    /// ソース先頭から連続）で、累積誤差が1パケット未満に収まる（区間数が一致し、
    /// 区間が連続して単調非減少である）ことを確認する。
    #[test]
    fn reference_matches_select_audio_segments_44100_case() {
        let video_timescale = 30_000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 44_100u32;
        let audio_packet_durations = vec![882u64; 500];

        let frames_per_segment = 20u64;
        let segment_duration = frames_per_segment * frame_duration;
        let video_segment_durations = vec![segment_duration; 6];
        let video_segment_source_starts = contiguous_source_starts(&video_segment_durations);

        let ranges = reference_position_based_audio_packet_ranges(
            &video_segment_durations,
            &video_segment_source_starts,
            video_timescale,
            &audio_packet_durations,
            audio_timescale,
        );

        assert_eq!(ranges.len(), 6);
        let mut prev = 0u64;
        for &(start, end) in &ranges {
            assert!(start >= prev, "非減少であること: ranges={ranges:?}");
            assert!(end >= start);
            prev = end;
        }
    }

    /// 先頭パケットがエンコーダのプライミング分だけ長い実データ（フィクスチャの
    /// 実測値: 先頭 `3852`、以降 `960`）で、固定の 1 パケット長で割る素朴な計算と
    /// 結果が食い違うことを固定する回帰テスト。
    ///
    /// `tests/fixtures/sample.mp4` に対して `Trim` を 120 フレーム × 2 区間（ソース
    /// 先頭から連続）にした場合、固定長では 400 になるが正しい期待値は 397 である。
    /// この差を見落とすと e2e テストが「実装が正しいのに落ちる」状態になる
    /// （実際にそうなっていた）。
    #[test]
    fn reference_accounts_for_longer_priming_packet() {
        let video_timescale = 30_000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48_000u32;

        // フィクスチャの実測パターン: 先頭だけ 3852、以降は 960。
        let mut audio_packet_durations = vec![3852u64];
        audio_packet_durations.extend(std::iter::repeat_n(960u64, 999));

        let video_segment_durations = vec![120 * frame_duration, 120 * frame_duration];
        let video_segment_source_starts = contiguous_source_starts(&video_segment_durations);

        let ranges = reference_position_based_audio_packet_ranges(
            &video_segment_durations,
            &video_segment_source_starts,
            video_timescale,
            &audio_packet_durations,
            audio_timescale,
        );

        assert_eq!(
            ranges,
            vec![(0, 197), (197, 397)],
            "プライミング分の長い先頭パケットを考慮した期待値になること"
        );

        // 固定長 960 で割る素朴な計算は 200 / 400 になり、上と食い違う。
        let naive_last = ((240 * frame_duration) as f64 * audio_timescale as f64
            / (video_timescale as f64 * 960.0))
            .round() as u64;
        assert_eq!(
            naive_last, 400,
            "素朴な計算は 400 になる（＝使ってはいけない）"
        );
        assert_ne!(
            ranges.last().unwrap().1,
            naive_last,
            "実 duration の累積と固定長除算が食い違うことをこのテストで固定する"
        );
    }

    /// 【最重要】区間ごとの音声は「出力の累積時間」ではなく「その区間のソース上の
    /// 絶対開始時刻」から選ばれること（docs/lossless-cut.md「実際に起きた誤り」の
    /// 回帰テスト）。`src/audio.rs` の同名テストと同じ考え方を、本ファイルの独立した
    /// 再実装で確認する。
    ///
    /// 区間1はソース [0, 120フレーム)、区間2はソース [360, 480フレーム) を保持し、
    /// その間 [120, 360) はCMとして除去された想定（実際の `cut_audio_is_bitwise_copy_
    /// and_matches_expected_count` / `cut_audio_segments_start_from_correct_source_
    /// position` と同じ形）。
    #[test]
    fn reference_uses_source_position_not_output_cumulative_time() {
        let video_timescale = 30_000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48_000u32;
        let audio_packet_durations = vec![960u64; 2000];

        let segment_duration = 120 * frame_duration;
        let video_segment_durations = vec![segment_duration, segment_duration];
        // 区間2はソースの360フレーム目から始まる(区間1の直後ではない)。
        let video_segment_source_starts = vec![0u64, 360 * frame_duration];

        let ranges = reference_position_based_audio_packet_ranges(
            &video_segment_durations,
            &video_segment_source_starts,
            video_timescale,
            &audio_packet_durations,
            audio_timescale,
        );

        assert_eq!(ranges.len(), 2);
        let (seg1_start, seg1_end) = ranges[0];
        let (seg2_start, _seg2_end) = ranges[1];
        assert_eq!(seg1_start, 0);

        // 「出力の累積時間」を起点にする修正前のロジックなら区間2はseg1_endの直後
        // (=区間1と連結)になってしまうはず。
        assert_ne!(
            seg2_start, seg1_end,
            "区間2の音声開始が区間1の終端(=出力の累積時間)に連結されている(修正前のバグの再現)"
        );
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
    tachikaze::ffprobe::csv_rows(Path::new("ffprobe"), path, "a:0", "packet=data_hash", true)
        .expect("ffprobe の起動に失敗した（PATH を確認）")
        .into_iter()
        .collect()
}

/// 音声ストリームの全パケットの dts を格納順に取得する。
fn ffprobe_audio_dts(path: &Path) -> Vec<i64> {
    tachikaze::ffprobe::csv_rows(Path::new("ffprobe"), path, "a:0", "packet=dts", false)
        .expect("ffprobe の起動に失敗した")
        .into_iter()
        .map(|line| {
            line.parse::<i64>()
                .unwrap_or_else(|_| panic!("dts が整数としてパースできない: {line:?}"))
        })
        .collect()
}

/// 音声ストリームの全パケットの duration を格納順に取得する。
///
/// 固定の 1 パケット長を仮定せず実測値を使うため
/// （[`reference_position_based_audio_packet_ranges`] のドキュメント参照）。
fn ffprobe_audio_packet_durations(path: &Path) -> Vec<u64> {
    ffprobe_csv_column(path, "a:0", "packet=duration")
        .into_iter()
        .map(|line| {
            line.parse::<u64>()
                .unwrap_or_else(|_| panic!("duration が整数としてパースできない: {line:?}"))
        })
        .collect()
}

/// 音声ストリームの全パケットの CRC32 を**格納順（重複・順序を保ったまま）**取得する。
///
/// [`ffprobe_audio_crc_set`] は `HashSet` に畳み込むため順序と重複が失われる。
/// 「区間の先頭パケットが元ファイルの正しい位置のパケットと一致するか」という
/// **位置**の検査には、集合ではなく列としての値が要る。
fn ffprobe_audio_crc_ordered(path: &Path) -> Vec<String> {
    tachikaze::ffprobe::csv_rows(Path::new("ffprobe"), path, "a:0", "packet=data_hash", true)
        .expect("ffprobe の起動に失敗した（PATH を確認）")
}

/// 映像ストリームの全パケットの pts（timescale 単位、格納順 = デコード順）を取得する。
///
/// 対象フィクスチャは閉じた GOP なので、同期サンプル（GOP境界）のデコード順
/// インデックスは表示順インデックスと一致する
/// （`src/mp4io/order_map.rs` の `closed_gop_sync_samples_have_matching_display_and_
/// decode_index` が実フィクスチャで確認済み）。そのため `pts[display_frame]`
/// （`display_frame` がGOP境界の場合）がその表示フレームの**ソース上の絶対
/// プレゼンテーション時刻**（映像timescale単位）になる。CFR前提の
/// `display_frame * frame_duration` のような決め打ちをせず、実測の pts を直接使う。
fn ffprobe_video_pts(path: &Path) -> Vec<i64> {
    ffprobe_csv_column(path, "v:0", "packet=pts")
        .into_iter()
        .map(|line| {
            line.parse::<i64>()
                .unwrap_or_else(|_| panic!("pts が整数としてパースできない: {line:?}"))
        })
        .collect()
}

/// 映像ストリームの全パケットの dts（timescale 単位、格納順 = デコード順）を取得する。
///
/// # なぜ pts ではなく dts が要るか
///
/// 区間のソース上の絶対開始時刻として使うべきなのは合成時刻（pts 相当）ではなく
/// **DTS** である（`src/commands.rs::segment_video_source_starts` の doc comment、
/// CLAUDE.md「静かに壊れる3つの罠」参照）。過去に `expected_video_segments`
/// （このテストファイルのオラクル）が `ffprobe_video_pts`（合成時刻）をそのまま
/// 区間開始時刻として使っており、実装本体（`src/commands.rs`）が合成時刻を使う
/// バグと**同じ間違いを踏んでいた**ため、テストとバグが「合意」してしまい検出でき
/// なかった。オラクル側は `ffprobe` が返す dts（実装の内部計算を経由しない、真に
/// 独立な値）を使うことで、この種の取り違えを検出できるようにする。
fn ffprobe_video_dts(path: &Path) -> Vec<i64> {
    ffprobe_csv_column(path, "v:0", "packet=dts")
        .into_iter()
        .map(|line| {
            line.parse::<i64>()
                .unwrap_or_else(|_| panic!("dts が整数としてパースできない: {line:?}"))
        })
        .collect()
}

/// 音声ストリームの全パケットの pts を格納順に取得する。
///
/// 音声には B フレーム相当の並べ替えが無いため、通常 `pts == dts` になる
/// （[`ffprobe_audio_dts`] と同じ値のはず）。出力の映像 pts と音声 pts の見た目上の
/// 対応（実際に再生した場合の A/V ずれ）を確認する
/// [`output_video_and_audio_first_packet_stay_in_av_sync_with_source`] で、
/// 「音声側もptsで揃える」ことを明示するために dts とは別名で用意する。
fn ffprobe_audio_pts(path: &Path) -> Vec<i64> {
    ffprobe_csv_column(path, "a:0", "packet=pts")
        .into_iter()
        .map(|line| {
            line.parse::<i64>()
                .unwrap_or_else(|_| panic!("pts が整数としてパースできない: {line:?}"))
        })
        .collect()
}

/// 音声ストリームの各パケットの `(ファイル内オフセット, サイズ)` を格納順に取得する。
///
/// 出力ファイルの音声パケットを1つだけ意図的に壊すために使う。
///
/// # `packet=pos,size` を1回で取らない理由
///
/// `ffprobe` は `-show_entries` に並べた順ではなく**内部の定義順**でフィールドを出す。
/// 実際に `packet=pos,size` と指定しても CSV は `size,pos` の順で出てくるため、
/// 1回のクエリを位置で解釈すると値が入れ替わる（このテストで実際に踏んだ）。
/// フィールド順に依存しないよう、`pos` と `size` を別々に取得して zip する。
fn ffprobe_audio_packet_positions(path: &Path) -> Vec<(u64, u64)> {
    let parse_all = |entry: &str| -> Vec<u64> {
        ffprobe_csv_column(path, "a:0", entry)
            .into_iter()
            .map(|line| {
                line.parse::<u64>()
                    .unwrap_or_else(|_| panic!("{entry} が整数としてパースできない: {line:?}"))
            })
            .collect()
    };

    let positions = parse_all("packet=pos");
    let sizes = parse_all("packet=size");
    assert_eq!(
        positions.len(),
        sizes.len(),
        "pos と size のパケット数が一致しない"
    );
    positions.into_iter().zip(sizes).collect()
}

/// `ffprobe ... -of csv=p=0` の出力を空行を除いた行の列として返す。
fn ffprobe_csv_column(path: &Path, stream_selector: &str, entry: &str) -> Vec<String> {
    tachikaze::ffprobe::csv_rows(Path::new("ffprobe"), path, stream_selector, entry, false)
        .expect("ffprobe の起動に失敗した")
}

fn ffprobe_scalar_stream_entry(path: &Path, stream_selector: &str, entry: &str) -> String {
    tachikaze::ffprobe::scalar_entry(Path::new("ffprobe"), path, stream_selector, entry)
        .expect("ffprobe の起動に失敗した")
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

// --- 実際に cut を実行する e2e テスト ---

/// フィクスチャ (`tests/fixtures/sample.mp4`, GOP=120 固定, 30000/1001fps, docs/lossless-cut.md
/// 前提と同じ) に対する Trim リスト。
///
/// **キーフレーム境界からわざとずらした値**を使う（#15 の補足）。フィクスチャの
/// キーフレームは表示順 0 / 120 / 240 / 360 / 480 なので、`Snap::Outward`（既定）で
/// `[10,110)` は `[0,120)` へ、`[370,470)` は `[360,480)` へ広がる。これにより
/// スナップ処理も経路に入り、`video_e2e.rs` / `src/verify.rs` のテストと同じ区間に揃う。
const TRIM_AVS_CONTENT: &str = "Trim(10,109) ++ Trim(370,469)";

/// スナップ後の各区間の映像フレーム数（`[0,120)` と `[360,480)`）。
const SNAPPED_FRAMES_PER_SEGMENT: [u64; 2] = [120, 120];

/// スナップ後の各区間の開始表示フレーム番号（`[0,120)` と `[360,480)` の開始側）。
/// どちらも GOP 境界（同期サンプル）なので、デコード順インデックス = 表示順
/// インデックスであり、`ffprobe_video_pts` の同じ添字でそのままソース時刻が引ける。
const SNAPPED_START_DISPLAY_FRAMES: [usize; 2] = [0, 360];

/// `cut` に渡す `.dtvi`。実 `dtvindex` 出力の抜粋（`src/mp4io/order_map.rs` の
/// テストが同じものをフィクスチャとの全行一致検証に使っている）。
fn dtvi_path() -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.dtvi")
}

/// 指定したフィクスチャに対して `tachikaze cut` を実行し、
/// `(一時ディレクトリ, 出力パス)` を返す。
fn run_cut_with_fixture(fixture: &Path, label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let tmp_dir = std::env::temp_dir().join(format!(
        "tachikaze-audio-e2e-{label}-{}",
        std::process::id()
    ));
    std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    std::fs::write(&trim_path, TRIM_AVS_CONTENT).expect("trim.avs を書けること");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("cut")
        .arg(fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--dtvi")
        .arg(dtvi_path())
        .arg("--verify")
        .output()
        .expect("tachikaze cut の起動に失敗した");
    assert!(
        output.status.success(),
        "tachikaze cut が失敗した: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    (tmp_dir, out_path)
}

/// 既存の Opus フィクスチャに対して `tachikaze cut` を実行する。
fn run_cut(label: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    run_cut_with_fixture(&common::fixture_path(), label)
}

/// フィクスチャ・区間定義から `reference_position_based_audio_packet_ranges` に渡す
/// `video_segment_durations` / `video_segment_source_starts` を実測値から組み立てる。
///
/// # なぜ pts ではなく dts を使うか(【最重要】過去にここが原因で67msのA/Vずれを見逃した)
///
/// `video_segment_source_starts` は `SNAPPED_START_DISPLAY_FRAMES` の各表示フレームの
/// 実測 **dts**（`ffprobe_video_dts`）を使う。合成時刻（pts）ではない
/// （`src/commands.rs::segment_video_source_starts` の doc comment、CLAUDE.md
/// 「静かに壊れる3つの罠」参照）。
///
/// 過去はここで `ffprobe_video_pts`（合成時刻）を使っており、実装本体
/// （`src/commands.rs`）が合成時刻を区間開始時刻として使うバグと**まったく同じ
/// 間違い**を踏んでいた。その結果、このテストのオラクル（期待値の再計算）と
/// 実装が「同じ間違った基準」で一致してしまい、67ms（=Bフレーム並べ替え深度2フレーム
/// 分の`cts_offset`）のA/Vずれを検出できなかった（`cut_audio_is_bitwise_copy_and_
/// matches_expected_count` / `cut_audio_segments_start_from_correct_source_position`
/// はどちらも通っていたが、実際には出力の音声が映像より67ms先行していた）。
/// dts はソースの実データを ffprobe が直接報告する値であり、`src/commands.rs` の
/// 内部計算を一切経由しない独立な値なので、この種の取り違えを検出できる。
///
/// CFR 前提の `frame * frame_duration` という決め打ちもしない（先頭 GOP でも
/// `dts != frame * frame_duration` になりうるため）。
fn expected_video_segments(fixture: &Path) -> (Vec<u64>, Vec<u64>, u32) {
    let video_timescale: u32 = ffprobe_scalar_stream_entry(fixture, "v:0", "stream=time_base")
        .rsplit('/')
        .next()
        .expect("time_base の書式が想定と違う")
        .parse()
        .expect("video timescale が整数としてパースできない");
    let video_frame_duration: u64 = ffprobe_scalar_stream_entry(fixture, "v:0", "packet=duration")
        .lines()
        .next()
        .expect("映像パケットが1つも無い")
        .parse()
        .expect("映像パケットの duration が整数としてパースできない");

    // 対象フィクスチャは閉じた GOP なので、同期サンプル（GOP境界）のデコード順
    // インデックスは表示順インデックスと一致する（`src/mp4io/order_map.rs` の
    // `closed_gop_sync_samples_have_matching_display_and_decode_index` で確認済み）。
    // `SNAPPED_START_DISPLAY_FRAMES` はどちらも GOP 境界なので、`dts`（格納順=
    // デコード順の配列）を表示フレーム番号でそのまま添字アクセスしてよい。
    let video_dts = ffprobe_video_dts(fixture);
    let video_segment_source_starts: Vec<u64> = SNAPPED_START_DISPLAY_FRAMES
        .iter()
        .map(|&frame| {
            let dts = video_dts[frame];
            assert!(
                dts >= 0,
                "映像フレーム{frame}のdtsが負になった: {dts}(想定外)"
            );
            dts as u64
        })
        .collect();

    let video_segment_durations: Vec<u64> = SNAPPED_FRAMES_PER_SEGMENT
        .iter()
        .map(|frames| frames * video_frame_duration)
        .collect();

    (
        video_segment_durations,
        video_segment_source_starts,
        video_timescale,
    )
}

/// 完了条件:
/// - フィクスチャで差分0件（出力の全音声パケットが入力に存在する）
/// - 出力の音声パケット数が `select_audio_segments` の計算結果（
///   `reference_position_based_audio_packet_ranges` で独立に再計算した期待値）と
///   一致する
/// - 出力の音声パケットの dts が単調増加である
///
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn cut_audio_is_bitwise_copy_and_matches_expected_count() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe")
    {
        return;
    }

    let fixture = common::fixture_path();
    let (tmp_dir, out_path) = run_cut("bitcopy");

    // --- 完了条件1: 差分0件（集合比較） ---
    let src_set = ffprobe_audio_crc_set(&fixture);
    let out_set = ffprobe_audio_crc_set(&out_path);
    let diff = packets_only_in_output(&out_set, &src_set);
    assert!(diff.is_empty(), "{}", describe_diff(&diff));

    // --- 完了条件2: 出力の音声パケット数が期待値と一致する ---
    let (video_segment_durations, video_segment_source_starts, video_timescale) =
        expected_video_segments(&fixture);
    let audio_timescale: u32 = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=sample_rate")
        .parse()
        .expect("sample_rate が整数としてパースできない");
    // 固定の 1 パケット長ではなく実測の duration 列を使う
    // （reference_position_based_audio_packet_ranges のドキュメント参照）。
    let audio_packet_durations = ffprobe_audio_packet_durations(&fixture);

    let expected_ranges = reference_position_based_audio_packet_ranges(
        &video_segment_durations,
        &video_segment_source_starts,
        video_timescale,
        &audio_packet_durations,
        audio_timescale,
    );
    let expected_packet_count: u64 = expected_ranges.iter().map(|&(s, e)| e - s).sum();

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

    // --- 注意書きの補完: 出力の音声パケットの dts が単調増加である ---
    assert!(
        is_strictly_increasing(&got_dts),
        "出力の音声パケットのdtsが単調増加でない: {got_dts:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 【最重要】自己検証の検査5（`docs/architecture.md`「自己検証（手順10）」の
/// 検査5）に対応する e2e テスト: 出力の**各区間の先頭音声パケット**の CRC32 が、
/// 元ファイルの**その区間のソース上の絶対開始時刻**付近のパケットの CRC32 と
/// 一致すること。
///
/// フィクスチャの音声は周波数スイープ（`tests/fixtures/gen.sh` 参照）にしてある。
/// 一定周波数のサイン波だとコーデックによっては音声パケットの中身がほぼ同一バイト列になり、
/// 「音声パケットをソースのどの位置から取ったか」という位置ずれを CRC32 比較で
/// 検出できない（同じ値ばかりで一致してしまう）ため、パケットごとに中身が変わる
/// 信号が必須。
///
/// このテストが無いと、`cut_audio_is_bitwise_copy_and_matches_expected_count`
/// （集合比較 + パケット数一致 + dts単調増加）が通っても、実際には
/// 「出力の音声が常にソースの先頭から詰められている」バグ
/// （docs/lossless-cut.md「実際に起きた誤り」参照）を見逃す。実際にそれが起きていた。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn cut_audio_segments_start_from_correct_source_position() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe")
    {
        return;
    }

    let fixture = common::fixture_path();
    let (tmp_dir, out_path) = run_cut("position");

    let (video_segment_durations, video_segment_source_starts, video_timescale) =
        expected_video_segments(&fixture);
    let audio_timescale: u32 = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=sample_rate")
        .parse()
        .expect("sample_rate が整数としてパースできない");
    let audio_packet_durations = ffprobe_audio_packet_durations(&fixture);

    // 元ファイルの各区間について、「ソース上の絶対開始時刻」に対応する音声パケット
    // 範囲を独立に求める（select_audio_segments と同じアルゴリズムのテスト側再実装。
    // 出力の累積時間は一切使わない）。
    let expected_ranges = reference_position_based_audio_packet_ranges(
        &video_segment_durations,
        &video_segment_source_starts,
        video_timescale,
        &audio_packet_durations,
        audio_timescale,
    );
    assert_eq!(expected_ranges.len(), 2, "区間は2つのはず");

    let src_crc = ffprobe_audio_crc_ordered(&fixture);
    let out_crc = ffprobe_audio_crc_ordered(&out_path);

    // 出力は区間を順番に連結したものなので、出力側のオフセットは各区間の長さ
    // (end - start) を先頭から積算するだけで求まる。
    let mut out_offset = 0usize;
    for (seg_idx, &(src_start, src_end)) in expected_ranges.iter().enumerate() {
        let seg_len = (src_end - src_start) as usize;
        assert!(seg_len > 0, "区間{}の音声パケット数が0", seg_idx + 1);

        // 先頭から数パケット（区間が短い場合はその分だけ）を比較する。
        // 手動の再現手順（issue本文）で最初の3パケットを確認しているのに合わせる。
        let compare_len = seg_len.min(3).min(out_crc.len() - out_offset);
        for k in 0..compare_len {
            let got = &out_crc[out_offset + k];
            let want = &src_crc[src_start as usize + k];
            assert_eq!(
                got,
                want,
                "区間{}のパケット{k}: 出力={got:?}, 期待(元ファイルのソース位置{})={want:?}\n\
                 (出力の音声が出力タイムライン上の累積時間から詰められていないか確認すること)",
                seg_idx + 1,
                src_start as usize + k,
            );
        }

        out_offset += seg_len;
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 【最重要】出力の**映像先頭フレームの pts** と**音声先頭パケットの pts** の対応
/// （実際に再生した場合の見た目上の A/V ずれ）が、元ファイルにおける同じフレーム・
/// 同じ音声パケットの対応から大きくずれていないことを確認する e2e。
///
/// 具体的には、区間ごとに
/// `(元の映像pts − 元の音声pts) ≈ (出力の映像pts − 出力の音声pts)`
/// が1音声パケット長以内で成り立つことを assert する。
///
/// # 既存の検査と何が違うか（なぜこれが要るか）
///
/// [`cut_audio_segments_start_from_correct_source_position`] は「出力の先頭音声
/// パケットの CRC32 が、テスト側オラクル（[`expected_video_segments`] /
/// [`reference_position_based_audio_packet_ranges`]）が計算した位置のパケットと
/// 一致するか」を見る。しかし**テスト側オラクルと実装本体が同じ間違った基準
/// （合成時刻/PTS）を区間開始時刻に使っていれば、両者は一致してしまい67msのずれを
/// 見逃す**（実際にこれが起きていた。[`expected_video_segments`] の doc comment
/// 参照。修正前は `src/commands.rs` も `expected_video_segments` も合成時刻を使って
/// いたため、比較先が「同じ間違い」を共有していて検出できなかった）。
///
/// このテストは `ffprobe` が報告する**再生時の pts 同士の見た目上の対応**だけを
/// 比較する。実装側・テスト側どちらの「区間開始時刻」計算ロジックにも依存しない
/// （`select_audio_segments` も `reference_position_based_audio_packet_ranges` も
/// 呼ばない）ため、実装とテストオラクルが同じ勘違いを共有して見逃す、という
/// 上記の失敗モードが原理的に起こらない。修正前のコードではここが約67msずれるため、
/// この検査は実際に有効。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn output_video_and_audio_first_packet_stay_in_av_sync_with_source() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe")
    {
        return;
    }

    let fixture = common::fixture_path();
    let (tmp_dir, out_path) = run_cut("avsync");

    // 区間ごとの「元ファイルでの音声パケット範囲」はテスト側オラクルで求める
    // （出力側で各区間が何パケットになるかを、区間の長さを先頭から積算して求める
    // ためだけに使う。区間開始時刻の比較そのものには使わない点が
    // `cut_audio_segments_start_from_correct_source_position` と違う）。
    let (video_segment_durations, video_segment_source_starts, video_timescale) =
        expected_video_segments(&fixture);
    let audio_timescale: u32 = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=sample_rate")
        .parse()
        .expect("sample_rate が整数としてパースできない");
    let audio_packet_durations = ffprobe_audio_packet_durations(&fixture);
    let expected_ranges = reference_position_based_audio_packet_ranges(
        &video_segment_durations,
        &video_segment_source_starts,
        video_timescale,
        &audio_packet_durations,
        audio_timescale,
    );
    assert_eq!(expected_ranges.len(), 2, "区間は2つのはず");

    let src_video_pts = ffprobe_video_pts(&fixture);
    let out_video_pts = ffprobe_video_pts(&out_path);
    let src_audio_pts = ffprobe_audio_pts(&fixture);
    let out_audio_pts = ffprobe_audio_pts(&out_path);

    // 出力側で各区間が始まる映像フレーム番号・音声パケット番号。出力は区間を
    // 順番に連結したものなので、区間の長さ(映像はSNAPPED_FRAMES_PER_SEGMENT、
    // 音声はexpected_rangesの幅)を先頭から積算するだけで求まる
    // （cut_audio_segments_start_from_correct_source_position と同じ考え方）。
    let mut out_video_frame = 0usize;
    let mut out_audio_packet = 0usize;

    for (seg_idx, (&src_frame, &(src_audio_start, src_audio_end))) in SNAPPED_START_DISPLAY_FRAMES
        .iter()
        .zip(expected_ranges.iter())
        .enumerate()
    {
        let src_video_pts_sec = src_video_pts[src_frame] as f64 / video_timescale as f64;
        let src_audio_pts_sec =
            src_audio_pts[src_audio_start as usize] as f64 / audio_timescale as f64;
        let src_av_offset_sec = src_video_pts_sec - src_audio_pts_sec;

        let out_video_pts_sec = out_video_pts[out_video_frame] as f64 / video_timescale as f64;
        let out_audio_pts_sec = out_audio_pts[out_audio_packet] as f64 / audio_timescale as f64;
        let out_av_offset_sec = out_video_pts_sec - out_audio_pts_sec;

        // 1音声パケット長ぶんの許容誤差。パケット長は一定でない(先頭はプライミング
        // で他より長い)ため、比較対象の区間の先頭パケットの実測durationを使う。
        let tolerance_sec =
            audio_packet_durations[src_audio_start as usize] as f64 / audio_timescale as f64;

        let drift_sec = (src_av_offset_sec - out_av_offset_sec).abs();
        assert!(
            drift_sec <= tolerance_sec,
            "区間{}: 元ファイルの(映像pts-音声pts)={src_av_offset_sec:.6}s に対し \
             出力の(映像pts-音声pts)={out_av_offset_sec:.6}s で、{drift_sec:.6}s \
             (許容{tolerance_sec:.6}s)ずれている。音声が映像より先行/遅延していないか \
             確認すること(docs/lossless-cut.md「実際に起きた誤り」参照)",
            seg_idx + 1
        );

        out_video_frame += SNAPPED_FRAMES_PER_SEGMENT[seg_idx] as usize;
        out_audio_packet += (src_audio_end - src_audio_start) as usize;
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// #42 / #45: AAC(`Mp4a`)でも複数区間 cut・パケット CRC32・音声位置・A/V pts 関係が
/// Opus と同じ規則で保たれることをまとめて確認する。
///
/// `run_cut_with_fixture` は `--verify` を付けるため、本体の自己検証（検査1〜6）と
/// ffprobe CRC32 検証も同じ実行内で通る。ここではさらに実装と独立したテスト側
/// オラクルで、区間先頭の音声位置と `(映像pts - 音声pts)` を検査する。
#[test]
#[ignore = "tests/fixtures/sample_aac.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn aac_cut_is_bitwise_copy_and_preserves_segment_positions_and_av_sync() {
    let fixture = common::aac_fixture_path();
    if common::skip_if_fixture_missing_at(&fixture)
        || skip_if_missing("ffmpeg")
        || skip_if_missing("ffprobe")
    {
        return;
    }

    let (tmp_dir, out_path) = run_cut_with_fixture(&fixture, "aac");

    // stsd は入力から clone するため、出力の音声 Codec は入力と同じ AAC のまま。
    let src_codec = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=codec_name");
    let out_codec = ffprobe_scalar_stream_entry(&out_path, "a:0", "stream=codec_name");
    assert_eq!(src_codec, "aac");
    assert_eq!(out_codec, src_codec);

    // 出力の音声パケットはすべて入力に存在する（再エンコードなしのビットコピー）。
    let src_set = ffprobe_audio_crc_set(&fixture);
    let out_set = ffprobe_audio_crc_set(&out_path);
    let diff = packets_only_in_output(&out_set, &src_set);
    assert!(diff.is_empty(), "{}", describe_diff(&diff));

    let (video_segment_durations, video_segment_source_starts, video_timescale) =
        expected_video_segments(&fixture);
    let audio_timescale: u32 = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=sample_rate")
        .parse()
        .expect("sample_rate が整数としてパースできない");
    let audio_packet_durations = ffprobe_audio_packet_durations(&fixture);
    let expected_ranges = reference_position_based_audio_packet_ranges(
        &video_segment_durations,
        &video_segment_source_starts,
        video_timescale,
        &audio_packet_durations,
        audio_timescale,
    );
    assert_eq!(expected_ranges.len(), 2, "区間は2つのはず");

    let expected_packet_count: usize = expected_ranges
        .iter()
        .map(|&(start, end)| (end - start) as usize)
        .sum();
    let out_dts = ffprobe_audio_dts(&out_path);
    assert_eq!(out_dts.len(), expected_packet_count);
    assert!(is_strictly_increasing(&out_dts));

    // 各区間の先頭3パケットをソース上の正しい位置と比較する。
    let src_crc = ffprobe_audio_crc_ordered(&fixture);
    let out_crc = ffprobe_audio_crc_ordered(&out_path);
    let mut out_audio_packet = 0usize;
    for (seg_idx, &(src_start, src_end)) in expected_ranges.iter().enumerate() {
        let seg_len = (src_end - src_start) as usize;
        for k in 0..seg_len.min(3) {
            assert_eq!(
                out_crc[out_audio_packet + k],
                src_crc[src_start as usize + k],
                "AAC 区間{}のパケット{k}がソース位置と一致しない",
                seg_idx + 1
            );
        }
        out_audio_packet += seg_len;
    }

    // 区間ごとに元と出力の「映像 pts - 音声 pts」を比較する（罠4の独立検査）。
    let src_video_pts = ffprobe_video_pts(&fixture);
    let out_video_pts = ffprobe_video_pts(&out_path);
    let src_audio_pts = ffprobe_audio_pts(&fixture);
    let out_audio_pts = ffprobe_audio_pts(&out_path);
    let mut out_video_frame = 0usize;
    let mut out_audio_packet = 0usize;
    for (seg_idx, (&src_frame, &(src_audio_start, src_audio_end))) in SNAPPED_START_DISPLAY_FRAMES
        .iter()
        .zip(expected_ranges.iter())
        .enumerate()
    {
        let src_av_offset = src_video_pts[src_frame] as f64 / video_timescale as f64
            - src_audio_pts[src_audio_start as usize] as f64 / audio_timescale as f64;
        let out_av_offset = out_video_pts[out_video_frame] as f64 / video_timescale as f64
            - out_audio_pts[out_audio_packet] as f64 / audio_timescale as f64;
        let tolerance =
            audio_packet_durations[src_audio_start as usize] as f64 / audio_timescale as f64;
        assert!(
            (src_av_offset - out_av_offset).abs() <= tolerance,
            "AAC 区間{}のA/V pts関係が保持されていない: src={src_av_offset:.6}s, \
             out={out_av_offset:.6}s, tolerance={tolerance:.6}s",
            seg_idx + 1
        );

        out_video_frame += SNAPPED_FRAMES_PER_SEGMENT[seg_idx] as usize;
        out_audio_packet += (src_audio_end - src_audio_start) as usize;
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// #47: Opus / AAC 以外の実コーデックとして FLAC を smoke E2E する。
///
/// `--verify` 付きで複数区間を cut し、認識・自己検証・音声 CRC32 比較までを通す。
/// FLAC 固有の復号や再構成は行わず、入力の `fLaC` サンプルエントリを clone して
/// パケット列だけをビットコピーできることを確認する。
#[test]
#[ignore = "tests/fixtures/sample_flac.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn flac_cut_smoke_is_bitwise_copy() {
    let fixture = common::fixture_path_named("sample_flac.mp4");
    if common::skip_if_fixture_missing_at(&fixture)
        || skip_if_missing("ffmpeg")
        || skip_if_missing("ffprobe")
    {
        return;
    }

    let (tmp_dir, out_path) = run_cut_with_fixture(&fixture, "flac");

    let src_codec = ffprobe_scalar_stream_entry(&fixture, "a:0", "stream=codec_name");
    let out_codec = ffprobe_scalar_stream_entry(&out_path, "a:0", "stream=codec_name");
    assert_eq!(src_codec, "flac");
    assert_eq!(out_codec, src_codec);

    let src_set = ffprobe_audio_crc_set(&fixture);
    let out_set = ffprobe_audio_crc_set(&out_path);
    let diff = packets_only_in_output(&out_set, &src_set);
    assert!(diff.is_empty(), "{}", describe_diff(&diff));

    let out_dts = ffprobe_audio_dts(&out_path);
    assert!(!out_dts.is_empty(), "FLAC 音声パケットが出力されていない");
    assert!(
        is_strictly_increasing(&out_dts),
        "FLAC 音声パケットの dts が単調増加でない: {out_dts:?}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件3: 意図的に壊した出力で差分が検出されることを確認する。
///
/// `cut` の正常な出力をコピーし、**音声パケット1個のバイトを1つだけ反転**させて
/// （`ffprobe` から得たそのパケットのファイル内オフセットを使う）、集合比較が
/// その1件を検出することを確認する。純ロジックの
/// [`pure_logic_tests::packets_only_in_output_detects_intentionally_corrupted_packet`]
/// と違い、実ファイル・実 `ffprobe` を通した経路で検出できることを示す。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn corrupted_audio_packet_is_detected_by_set_comparison() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe")
    {
        return;
    }

    let fixture = common::fixture_path();
    let (tmp_dir, out_path) = run_cut("corrupt");

    // 壊す前提のセルフチェック: 壊す前は差分0件のはず。
    let src_set = ffprobe_audio_crc_set(&fixture);
    let clean_set = ffprobe_audio_crc_set(&out_path);
    let diff = packets_only_in_output(&clean_set, &src_set);
    assert!(
        diff.is_empty(),
        "壊す前に差分が出た: {}",
        describe_diff(&diff)
    );

    // 音声パケットを1つ選んでバイトを1つ反転させる。先頭はプライミングで特殊なので
    // 中ほどのパケットを選ぶ。
    let positions = ffprobe_audio_packet_positions(&out_path);
    assert!(
        positions.len() >= 3,
        "音声パケットが少なすぎて壊す対象を選べない: {}",
        positions.len()
    );
    let (pos, size) = positions[positions.len() / 2];
    assert!(size > 0, "パケットサイズが0");

    let corrupted_path = tmp_dir.join("corrupted.mp4");
    let mut bytes = std::fs::read(&out_path).expect("出力を読めること");
    let target = pos as usize;
    assert!(
        target < bytes.len(),
        "パケットのオフセットがファイル範囲外: pos={pos}, len={}",
        bytes.len()
    );
    bytes[target] ^= 0xFF;
    std::fs::write(&corrupted_path, &bytes).expect("壊した出力を書けること");

    let corrupted_set = ffprobe_audio_crc_set(&corrupted_path);
    let diff = packets_only_in_output(&corrupted_set, &src_set);
    assert!(
        !diff.is_empty(),
        "音声パケットを1つ壊したのに集合比較で検出されなかった"
    );

    // 報告メッセージに件数と先頭のCRCが出ること。
    let message = describe_diff(&diff);
    assert!(
        message.contains(diff[0]),
        "報告メッセージに先頭のCRCが含まれること: {message}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
