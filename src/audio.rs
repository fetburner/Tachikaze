//! 出力する各映像区間に対応する音声パケットの範囲を決める。
//!
//! 音声（Opus, 20ms グリッド）と映像（例: 30000/1001 fps, 33.37ms グリッド）は
//! フレーム長が一致しないため、区間ごとに端数が出る。区間の境界を個別に丸めると
//! 端数が継ぎ目ごとに蓄積しうるため、ここでは**出力タイムライン上の累積映像時間**
//! から毎回パケット数を計算し直す（[`select_audio_segments`] のドキュメント参照）。
//!
//! `plan.rs`（映像側のスナップ結果）には依存しない。プリミティブな数値だけを
//! 受け取ることで、音声側の実装・テストを映像側の型から独立させる。

use crate::mp4io::read::SampleInfo;
use crate::order::DecodeIdx;

/// 音声パケット選択の結果。ある一つの出力区間に割り当てる音声パケットの
/// デコード順（== 表示順）半開区間 `[start, end)`。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub struct AudioSegment {
    pub start: DecodeIdx,
    pub end: DecodeIdx,
}

/// ドリフトの統計（ログ・レポート用）。
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct DriftStats {
    /// 各境界での理想値（丸める前の浮動小数点パケット数）と実際に丸めた
    /// パケット数との誤差（パケット単位）の絶対値の最大。
    pub max_abs_error_packets: f64,
}

/// 出力に並べる映像区間ごとに、割り当てる音声パケットの範囲を決める。
///
/// # 引数
/// - `video_segment_durations`: 出力に並べる順の各映像区間の再生時間（映像トラックの
///   timescale 単位）。
/// - `video_timescale`: 映像トラックの timescale。
/// - `audio_samples`: 音声トラックの全サンプル（デコード順 == 表示順。音声には B
///   フレーム相当が無いため並べ替えは無い）。空スライスの場合は音声処理そのものを
///   行わず、空の結果を返す（`--video-only` 相当の呼び出し側での分岐を想定）。
/// - `audio_timescale`: 音声トラックの timescale（sample rate に相当）。
///
/// # 設計
/// 区間 `k` が終わった時点までの、出力タイムライン上の累積映像時間を `T_k`
/// （`T_0 = 0`）とする。`T_k` を音声の timescale に変換した目標時刻に対して、
/// **音声サンプルの実際の duration を先頭から累積した値**が最も近くなる
/// パケット数を毎回 `T_k` から計算し直す（二分探索）。区間 `k` に割り当てる
/// パケットは境界 `k-1` と境界 `k` の半開区間。
///
/// 区間の長さだけを見て毎回独立に丸める方式と異なり、境界のパケット数は常に
/// 「これまでの累積時間」に対して最も近い値に選ばれるため、丸め誤差が継ぎ目を
/// 越えて蓄積しない。
///
/// **固定の `frame_size`（1パケットあたりの長さ）を仮定しない。** 実データでは
/// 先頭パケットがエンコーダのプライミング分だけ他のパケットより大幅に長い
/// ことがある（実測: 20ms 相当のパケットが並ぶ中、先頭だけ 80ms 相当）。
/// `audio_samples[0].duration` を frame_size とみなして除算する実装だと、
/// この1個の外れ値のせいで全パケットの境界がズレてしまう（実際にこの不具合を
/// 実ファイルの E2E テストで検出した）。そのため、各サンプルの実際の
/// duration を毎回合計してから最近傍を探す方式にしている。
///
/// 区間の終端が音声サンプル総数を超える場合は音声サンプル総数で止める。
pub fn select_audio_segments(
    video_segment_durations: &[u64],
    video_timescale: u32,
    audio_samples: &[SampleInfo],
    audio_timescale: u32,
) -> anyhow::Result<(Vec<AudioSegment>, DriftStats)> {
    if audio_samples.is_empty() {
        return Ok((
            Vec::new(),
            DriftStats {
                max_abs_error_packets: 0.0,
            },
        ));
    }

    anyhow::ensure!(
        video_timescale > 0,
        "video_timescale はゼロより大きい必要があります"
    );
    anyhow::ensure!(
        audio_timescale > 0,
        "audio_timescale はゼロより大きい必要があります"
    );

    // 累積実測時間（音声 timescale 単位）。cumulative[i] は samples[0..i] の
    // duration 合計（cumulative[0] == 0、cumulative[len] == 音声トラック全体の長さ）。
    let mut cumulative: Vec<u64> = Vec::with_capacity(audio_samples.len() + 1);
    cumulative.push(0);
    for s in audio_samples {
        cumulative.push(cumulative.last().copied().unwrap_or(0) + s.duration as u64);
    }
    let total_audio_samples = audio_samples.len() as u64;

    // 誤差をパケット単位で報告するための正規化係数（典型的な1パケットの長さ）。
    // 実データは先頭パケットが外れ値になりうるので、全体の平均を使う
    // （選択アルゴリズム自体はこの値を使わない。ログ・テスト向けの目安）。
    let average_frame_size = *cumulative.last().unwrap() as f64 / total_audio_samples as f64;

    let mut segments = Vec::with_capacity(video_segment_durations.len());
    let mut max_abs_error_packets = 0.0f64;

    let mut cumulative_video_time: u64 = 0;
    let mut prev_packet_count: u64 = 0;

    for &duration in video_segment_durations {
        cumulative_video_time += duration;

        // T_k を音声 timescale 単位の目標時刻に変換する。
        let target =
            (cumulative_video_time as f64 * audio_timescale as f64) / video_timescale as f64;

        let packet_count = nearest_cumulative_index(&cumulative, target).min(total_audio_samples);

        let actual = cumulative[packet_count as usize] as f64;
        let error_packets = (actual - target).abs() / average_frame_size;
        if error_packets > max_abs_error_packets {
            max_abs_error_packets = error_packets;
        }

        let start = prev_packet_count.min(total_audio_samples);
        // 累積値は非減少なので理論上 packet_count >= start だが、丸め誤差に対して
        // 空区間（start == end）を返す形で安全側に倒す。
        let end = packet_count.max(start);

        segments.push(AudioSegment {
            start: DecodeIdx(start as u32),
            end: DecodeIdx(end as u32),
        });

        prev_packet_count = end;
    }

    Ok((
        segments,
        DriftStats {
            max_abs_error_packets,
        },
    ))
}

/// 昇順の `cumulative` の中から `target` に最も近い要素のインデックスを返す。
fn nearest_cumulative_index(cumulative: &[u64], target: f64) -> u64 {
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

/// 継ぎ目（出力区間）ごとの A/V ずれ 1 件分。
#[derive(Copy, Clone, PartialEq, Debug)]
pub struct SegmentAvDiff {
    /// その区間の映像の再生時間（秒）。
    pub video_seconds: f64,
    /// その区間に割り当てられた音声サンプルの duration を合計した再生時間（秒）。
    pub audio_seconds: f64,
    /// `audio_seconds - video_seconds` を ms 換算したもの（符号あり）。
    /// 正なら音声が映像より長い（音声が遅れて終わる）。
    pub diff_ms: f64,
}

/// cut 全体の A/V 同期レポート。
#[derive(Clone, PartialEq, Debug)]
pub struct AvSyncReport {
    pub per_segment: Vec<SegmentAvDiff>,
    /// `per_segment` の `diff_ms` の絶対値の最大。
    pub max_abs_diff_ms: f64,
    /// `max_abs_diff_ms` を記録した区間のインデックス（0 始まり）。
    pub max_abs_diff_segment_index: usize,
    /// `per_segment` の `diff_ms` の合計（符号あり）。
    pub total_diff_ms: f64,
    /// `max_abs_diff_ms` が [`AV_DIFF_WARNING_THRESHOLD_MS`] を超えるかどうか。
    pub exceeds_threshold: bool,
}

/// A/V ずれの警告閾値（ms）。映像 1 フレーム（30000/1001 fps で約 33.4ms）より
/// 少し大きい値を採用し、フレーム単位の丸め誤差では鳴らないようにする。
pub const AV_DIFF_WARNING_THRESHOLD_MS: f64 = 40.0;

/// 出力区間ごとの A/V ずれを集計する。
///
/// `video_segment_durations` と `audio_segments` は同じ長さ・同じ順序であることを
/// 前提とする（[`select_audio_segments`] の戻り値の `audio_segments` をそのまま渡す
/// 想定）。各区間の音声側の実際の長さは、`frame_size` を仮定して掛け算するのでは
/// なく、その区間に割り当てられた実際の音声サンプルの `duration` を合計して求める
/// （`SampleInfo::duration` は必ずしも全サンプルで一定とは限らないため、こちらの
/// 方が正確）。
///
/// `video_segment_durations` が空の場合は `per_segment` が空のレポートを返す。
pub fn av_sync_report(
    video_segment_durations: &[u64],
    video_timescale: u32,
    audio_segments: &[AudioSegment],
    audio_samples: &[SampleInfo],
    audio_timescale: u32,
) -> anyhow::Result<AvSyncReport> {
    anyhow::ensure!(
        video_segment_durations.len() == audio_segments.len(),
        "video_segment_durations と audio_segments の長さが一致しません（{} vs {}）",
        video_segment_durations.len(),
        audio_segments.len()
    );
    anyhow::ensure!(
        video_timescale > 0,
        "video_timescale はゼロより大きい必要があります"
    );
    anyhow::ensure!(
        audio_timescale > 0,
        "audio_timescale はゼロより大きい必要があります"
    );

    let mut per_segment = Vec::with_capacity(video_segment_durations.len());
    let mut max_abs_diff_ms = 0.0f64;
    let mut max_abs_diff_segment_index = 0usize;
    let mut total_diff_ms = 0.0f64;

    for (i, (&video_duration, audio_segment)) in video_segment_durations
        .iter()
        .zip(audio_segments.iter())
        .enumerate()
    {
        let video_seconds = video_duration as f64 / video_timescale as f64;

        let start = audio_segment.start.0 as usize;
        let end = audio_segment.end.0 as usize;
        anyhow::ensure!(
            start <= end,
            "audio_segments[{i}] の start が end を超えています（start={start}, end={end}）"
        );
        anyhow::ensure!(
            end <= audio_samples.len(),
            "audio_segments[{i}] の end が audio_samples の範囲外です（end={end}, len={}）",
            audio_samples.len()
        );

        let audio_duration_units: u64 = audio_samples[start..end]
            .iter()
            .map(|s| s.duration as u64)
            .sum();
        let audio_seconds = audio_duration_units as f64 / audio_timescale as f64;

        let diff_ms = (audio_seconds - video_seconds) * 1000.0;

        total_diff_ms += diff_ms;
        if diff_ms.abs() > max_abs_diff_ms {
            max_abs_diff_ms = diff_ms.abs();
            max_abs_diff_segment_index = i;
        }

        per_segment.push(SegmentAvDiff {
            video_seconds,
            audio_seconds,
            diff_ms,
        });
    }

    let exceeds_threshold = max_abs_diff_ms > AV_DIFF_WARNING_THRESHOLD_MS;

    Ok(AvSyncReport {
        per_segment,
        max_abs_diff_ms,
        max_abs_diff_segment_index,
        total_diff_ms,
        exceeds_threshold,
    })
}

/// [`av_sync_report`] の結果を、issue 記載の形式のテキストに整形する。
///
/// `println!` はしない。表示するかどうか・タイミングは呼び出し側（cut パイプライン
/// の配線）が決める。
pub fn format_av_sync_report(report: &AvSyncReport) -> String {
    use std::fmt::Write as _;

    let mut out = String::new();

    if report.per_segment.is_empty() {
        out.push_str("音声同期: 区間 0（対象区間なし）\n");
        return out;
    }

    let warning = if report.exceeds_threshold {
        " [警告: 閾値超過]"
    } else {
        ""
    };

    // 最大ずれは符号付きで表示する（区間番号は 1 始まり、ユーザ向けの表示のため）。
    let max_diff_signed_ms = report.per_segment[report.max_abs_diff_segment_index].diff_ms;
    let _ = writeln!(
        out,
        "音声同期: 区間 {} / 最大ずれ {:+.0} ms (区間 {}) / 合計 {:+.0} ms{}",
        report.per_segment.len(),
        max_diff_signed_ms,
        report.max_abs_diff_segment_index + 1,
        report.total_diff_ms,
        warning
    );

    for (i, seg) in report.per_segment.iter().enumerate() {
        let _ = writeln!(
            out,
            "  区間 {}: 映像 {:.3}s 音声 {:.3}s ({:+.0} ms)",
            i + 1,
            seg.video_seconds,
            seg.audio_seconds,
            seg.diff_ms
        );
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の音声サンプルを `count` 個、全て `duration` で生成する。
    fn make_audio_samples(count: usize, duration: u32) -> Vec<SampleInfo> {
        (0..count)
            .map(|i| SampleInfo {
                file_offset: i as u64 * 100,
                size: 100,
                duration,
                cts_offset: 0,
                is_sync: true,
            })
            .collect()
    }

    /// 完了条件: 映像 30000/1001 fps、音声 48000Hz / 960 サンプルで 10 区間つないでも
    /// 累積誤差が 1 パケット以内に収まる。
    #[test]
    fn ten_segments_drift_stays_within_one_packet() {
        let video_timescale = 30000u32;
        let frame_duration = 1001u64; // 1 映像フレームぶんの timescale 単位
        let audio_timescale = 48000u32;
        let frame_size = 960u32;

        // 各区間 16 映像フレーム分。48000Hz/960 の音声パケットに対して
        // 端数が出るように選んだ値。
        let frames_per_segment = 16u64;
        let segment_duration = frames_per_segment * frame_duration;
        let video_segment_durations = vec![segment_duration; 10];

        // 十分な数の音声サンプルを用意する。
        let audio_samples = make_audio_samples(1000, frame_size);

        let (segments, stats) = select_audio_segments(
            &video_segment_durations,
            video_timescale,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        assert_eq!(segments.len(), 10);
        assert!(
            stats.max_abs_error_packets < 1.0,
            "max_abs_error_packets = {}",
            stats.max_abs_error_packets
        );

        // 各境界で、丸める前の理想値との誤差が 0.5 パケット未満（丸めの定義上
        // 常に成り立つはずだが、明示的に検証しておく）。
        let mut cumulative_video_time = 0u64;
        let mut prev_end = 0u32;
        for seg in &segments {
            cumulative_video_time += segment_duration;
            let ideal = (cumulative_video_time as f64 * audio_timescale as f64)
                / (video_timescale as f64 * frame_size as f64);
            assert!(
                (seg.end.0 as f64 - ideal).abs() < 0.5 + 1e-9,
                "end={}, ideal={}",
                seg.end.0,
                ideal
            );
            assert_eq!(seg.start.0, prev_end);
            prev_end = seg.end.0;
        }
    }

    /// 回帰防止: 区間ごとに独立に丸める（誤差を持ち越さない）実装だと、
    /// 端数が出やすい区間長を並べたときに累積誤差が 0.5 パケットを超えて
    /// 蓄積しうる。累積時間から毎回計算する本実装ではそれが起きないことを示す。
    #[test]
    fn cumulative_rounding_beats_independent_rounding() {
        let video_timescale = 30000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48000u32;
        let frame_size = 960u32;

        let frames_per_segment = 16u64;
        let segment_duration = frames_per_segment * frame_duration;
        let num_segments = 10usize;
        let video_segment_durations = vec![segment_duration; num_segments];

        let audio_samples = make_audio_samples(1000, frame_size);

        let (segments, _stats) = select_audio_segments(
            &video_segment_durations,
            video_timescale,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        // 「累積時間から毎回計算する」実装（本実装）の、最終境界での理想値との誤差。
        let total_video_time = segment_duration * num_segments as u64;
        let true_ideal_total = (total_video_time as f64 * audio_timescale as f64)
            / (video_timescale as f64 * frame_size as f64);
        let cumulative_final_error =
            (segments.last().unwrap().end.0 as f64 - true_ideal_total).abs();
        assert!(
            cumulative_final_error < 0.5 + 1e-9,
            "cumulative_final_error = {}",
            cumulative_final_error
        );

        // 比較対象: 区間の長さだけを見て毎回独立に丸め、それを積算する方式。
        // （誤差を持ち越さないナイーブな実装のシミュレーション。src/audio.rs の
        // 実装はこちらのロジックを持たない。）
        let per_segment_ideal = (segment_duration as f64 * audio_timescale as f64)
            / (video_timescale as f64 * frame_size as f64);
        let independent_sum: f64 = per_segment_ideal.round() * num_segments as f64;
        let independent_error = (independent_sum - true_ideal_total).abs();

        // 独立丸め方式では誤差が 0.5 を超えて蓄積し、累積方式より明確に悪化する
        // ことを確認する（このテストが壊れたらドリフト補正の設計が壊れている）。
        assert!(
            independent_error > 0.5,
            "independent_error = {} (regression test の前提が崩れている)",
            independent_error
        );
        assert!(independent_error > cumulative_final_error);
    }

    /// 完了条件: 音声トラックの timescale が 48000 以外でも動く（44100Hz 相当）。
    #[test]
    fn works_with_44100_audio_timescale() {
        let video_timescale = 30000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 44100u32;
        // 44.1kHz での 20ms 相当。
        let frame_size = 882u32;

        let frames_per_segment = 20u64;
        let segment_duration = frames_per_segment * frame_duration;
        let video_segment_durations = vec![segment_duration; 6];

        let audio_samples = make_audio_samples(500, frame_size);

        let (segments, stats) = select_audio_segments(
            &video_segment_durations,
            video_timescale,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        assert_eq!(segments.len(), 6);
        assert!(stats.max_abs_error_packets < 1.0);

        // 区間は連続しており、重複や欠落がない。
        let mut prev_end = 0u32;
        for seg in &segments {
            assert_eq!(seg.start.0, prev_end);
            assert!(seg.end.0 >= seg.start.0);
            prev_end = seg.end.0;
        }
    }

    /// 回帰テスト: 実ファイルの E2E テストで実際に踏んだ不具合の再現。
    ///
    /// libopus でエンコードした音声の先頭パケットは、エンコーダのプライミング分
    /// だけ他の20msパケットより大幅に長くなることがある（実測: 80.25ms）。
    /// `frame_size = audio_samples[0].duration` と仮定する実装だと、この1個の
    /// 外れ値のせいで全パケットの境界が大きくズレる（実測で音声時間が映像時間の
    /// 1/4程度になった）。累積の実測 duration から最近傍を探す本実装ではこれが
    /// 起きないことを確認する。
    #[test]
    fn handles_outlier_first_packet_duration_without_derailing() {
        let video_timescale = 30000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48000u32;
        let normal_frame_size = 960u32; // 20ms @ 48kHz
        let outlier_first_frame_size = 3852u32; // 実測(80.25ms相当)に近い外れ値

        // 20秒ぶんの映像を2区間に分ける(実際のE2Eテストと同じ形)。
        let video_segment_durations = vec![8 * frame_duration * 30, 8 * frame_duration * 30];

        let mut audio_samples = make_audio_samples(1000, normal_frame_size);
        audio_samples[0].duration = outlier_first_frame_size;

        let (segments, _stats) = select_audio_segments(
            &video_segment_durations,
            video_timescale,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        for (i, seg) in segments.iter().enumerate() {
            let audio_duration: u64 = audio_samples[seg.start.0 as usize..seg.end.0 as usize]
                .iter()
                .map(|s| s.duration as u64)
                .sum();
            let video_seconds = video_segment_durations[i] as f64 / video_timescale as f64;
            let audio_seconds = audio_duration as f64 / audio_timescale as f64;
            assert!(
                (audio_seconds - video_seconds).abs() < 0.1,
                "区間{i}: 映像{video_seconds:.3}s に対して音声{audio_seconds:.3}s\
                 (外れ値パケットに引きずられて大きくズレてはいけない)"
            );
        }
    }

    /// 完了条件: `--video-only` 相当（音声処理を呼ばない/音声サンプルが空）では
    /// 空の結果を返す。
    #[test]
    fn empty_audio_samples_returns_empty_result() {
        let video_segment_durations = vec![1001u64 * 30; 5];
        let audio_samples: Vec<SampleInfo> = Vec::new();

        let (segments, stats) =
            select_audio_segments(&video_segment_durations, 30000, &audio_samples, 48000).unwrap();

        assert!(segments.is_empty());
        assert_eq!(stats.max_abs_error_packets, 0.0);
    }

    /// 区間の終端が音声サンプル総数を超える場合は音声サンプル総数で止める。
    #[test]
    fn clamps_to_total_audio_samples() {
        let video_timescale = 30000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48000u32;
        let frame_size = 960u32;

        // 音声サンプルはごく少数しか用意しない一方、映像区間は長く要求する。
        let video_segment_durations = vec![frame_duration * 10_000];
        let audio_samples = make_audio_samples(10, frame_size);

        let (segments, _stats) = select_audio_segments(
            &video_segment_durations,
            video_timescale,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        assert_eq!(segments.len(), 1);
        assert_eq!(segments[0].start.0, 0);
        assert_eq!(segments[0].end.0, 10);
    }

    #[test]
    fn rejects_zero_timescale() {
        let audio_samples = make_audio_samples(10, 960);
        assert!(select_audio_segments(&[1001], 0, &audio_samples, 48000).is_err());
        assert!(select_audio_segments(&[1001], 30000, &audio_samples, 0).is_err());
    }

    /// 完了条件: #33 の `select_audio_segments` と組み合わせた合成データで、
    /// 継ぎ目ごとの A/V ずれが数 ms 台に収まる（docs/measurements.md の実測値
    /// 「5 区間、継ぎ目あたり約 6ms」と桁が合っていることを確認する）。
    #[test]
    fn av_sync_report_stays_in_single_digit_ms_for_realistic_fixture() {
        let video_timescale = 30000u32;
        let frame_duration = 1001u64;
        let audio_timescale = 48000u32;
        let frame_size = 960u32;

        // ファイルCの実測（5区間）を模した合成データ: 区間ごとに長さを変える。
        let frames_per_segment = [500u64, 650, 300, 800, 450];
        let video_segment_durations: Vec<u64> = frames_per_segment
            .iter()
            .map(|&f| f * frame_duration)
            .collect();

        let total_frames: u64 = frames_per_segment.iter().sum();
        // 十分な数の音声サンプルを用意する（映像の総尺を確実に上回る）。
        let total_audio_needed = (total_frames * frame_duration * audio_timescale as u64)
            / (video_timescale as u64 * frame_size as u64);
        let audio_samples = make_audio_samples((total_audio_needed as usize) + 10, frame_size);

        let (audio_segments, _drift_stats) = select_audio_segments(
            &video_segment_durations,
            video_timescale,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        let report = av_sync_report(
            &video_segment_durations,
            video_timescale,
            &audio_segments,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        assert_eq!(report.per_segment.len(), 5);

        // 1 音声パケット（20ms）を大きく超えるようなら #33 側の丸めロジックが
        // 壊れている（このテストでは #33 は変更せず、異常な値でないことだけ確認する）。
        assert!(
            report.max_abs_diff_ms < 20.0,
            "max_abs_diff_ms = {} (1 パケット分の20msを超えている)",
            report.max_abs_diff_ms
        );
        assert!(
            report.total_diff_ms.abs() < 20.0,
            "total_diff_ms = {}",
            report.total_diff_ms
        );
        assert!(!report.exceeds_threshold);

        let text = format_av_sync_report(&report);
        assert!(text.contains("音声同期: 区間 5"));
        assert!(text.contains("区間 1:"));
        assert!(text.contains("区間 5:"));
        assert!(!text.contains("警告"));
    }

    /// 完了条件: 閾値（[`AV_DIFF_WARNING_THRESHOLD_MS`]）を超えるずれがあれば
    /// `exceeds_threshold` が true になり、フォーマット済みテキストにも警告が出る。
    /// ただしエラーにはしない（`av_sync_report` は `Ok` を返す）。
    #[test]
    fn av_sync_report_flags_warning_when_threshold_exceeded() {
        // 意図的に不自然なデータを手で作る: 映像区間は短いのに、その区間に
        // 割り当てられた音声パケット数が過大（≈480ms 分）になるようにする。
        let video_timescale = 30000u32;
        let video_segment_durations = vec![1001u64 * 10]; // 約 333.7 ms 分の映像
        let audio_timescale = 48000u32;

        // 音声パケットは 960 サンプル(20ms)刻みだが、24 パケット全てを
        // この 1 区間に割り当てる（select_audio_segments を経由せず、
        // AudioSegment を手作りして不自然な対応を作る）。
        let audio_samples = make_audio_samples(24, 960); // 24 * 20ms = 480ms 分
        let audio_segments = vec![AudioSegment {
            start: DecodeIdx(0),
            end: DecodeIdx(24),
        }];

        let report = av_sync_report(
            &video_segment_durations,
            video_timescale,
            &audio_segments,
            &audio_samples,
            audio_timescale,
        )
        .unwrap();

        assert_eq!(report.per_segment.len(), 1);
        // 映像 ≈333.7ms に対して音声 480ms なので、差は 100ms を超える。
        assert!(report.max_abs_diff_ms > AV_DIFF_WARNING_THRESHOLD_MS);
        assert!(report.exceeds_threshold);

        let text = format_av_sync_report(&report);
        assert!(text.contains("警告"));
    }

    #[test]
    fn av_sync_report_rejects_length_mismatch() {
        let audio_samples = make_audio_samples(10, 960);
        let audio_segments = vec![AudioSegment {
            start: DecodeIdx(0),
            end: DecodeIdx(5),
        }];
        // video_segment_durations が 2 要素、audio_segments が 1 要素で不一致。
        let result = av_sync_report(&[1001, 1001], 30000, &audio_segments, &audio_samples, 48000);
        assert!(result.is_err());
    }

    #[test]
    fn format_av_sync_report_handles_empty_report() {
        let report = AvSyncReport {
            per_segment: Vec::new(),
            max_abs_diff_ms: 0.0,
            max_abs_diff_segment_index: 0,
            total_diff_ms: 0.0,
            exceeds_threshold: false,
        };
        let text = format_av_sync_report(&report);
        assert!(text.contains("区間 0"));
    }
}
