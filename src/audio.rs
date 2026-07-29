// cut パイプラインから消費されるまで未使用。配線されたら外す。
#![allow(dead_code)]

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
/// （`T_0 = 0`）とする。`T_k` を音声の timescale に変換し、`round(T_k' / frame_size)`
/// で「区間 `k` の終わりまでに消費されているべき音声パケット数」を毎回 `T_k` から
/// 計算し直す。区間 `k` に割り当てるパケットは境界 `k-1` と境界 `k` の半開区間。
///
/// 区間の長さだけを見て毎回独立に丸める方式と異なり、境界のパケット数は常に
/// 「これまでの累積時間」に対して最も近い値に丸められるため、丸め誤差が継ぎ目を
/// 越えて蓄積しない（誤差は常に高々 0.5 パケット）。
///
/// `frame_size`（音声 1 パケットあたりの timescale 単位の長さ）は `audio_samples`
/// の先頭サンプルの `duration` から求める（対象素材では全サンプルで一定である前提）。
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

    let frame_size = audio_samples[0].duration;
    anyhow::ensure!(
        frame_size > 0,
        "音声サンプルの duration（frame_size）はゼロより大きい必要があります"
    );

    let total_audio_samples = audio_samples.len() as u64;

    let mut segments = Vec::with_capacity(video_segment_durations.len());
    let mut max_abs_error_packets = 0.0f64;

    let mut cumulative_video_time: u64 = 0;
    let mut prev_packet_count: u64 = 0;

    for &duration in video_segment_durations {
        cumulative_video_time += duration;

        // T_k を音声パケット数に変換する。一度の乗除算にまとめることで、
        // 「映像 timescale → 音声 timescale」「時間 → パケット数」の 2 段階に
        // 分けた場合よりも丸め誤差の混入を抑える。
        let ideal_packets = (cumulative_video_time as f64 * audio_timescale as f64)
            / (video_timescale as f64 * frame_size as f64);

        let rounded_packets = ideal_packets.round();
        let error = (ideal_packets - rounded_packets).abs();
        if error > max_abs_error_packets {
            max_abs_error_packets = error;
        }

        let packet_count = (rounded_packets as u64).min(total_audio_samples);

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
}
