//! analyze --report: カット境界とキーフレームの距離を報告する。
//!
//! **mp4 は一切読まない。** 入力は `.dtvi`（[`crate::dtvi::Dtvi`]）と `trim.avs` の
//! パース結果（[`crate::trim::TrimList`]）だけ。mp4 のサンプル表を読む処理（`mp4io`）に
//! 依存させないことで、解析レーン（`analyze`）とカットのレーン（`cut` / mp4 読み込み）を
//! 独立に進められる。
//!
//! 目的は「キーフレーム境界に丸めると何秒 CM が残るか」を、実際に切る前に把握すること。
//! `--snap`（既定 `outward`）は本編を削らずに CM を残す方向にカット境界を動かす。
//! - 保持区間の**開始**境界: 手前のキーフレームへ（境界より前が余分に残る）
//! - 保持区間の**終了**境界: 次のキーフレームへ（境界より後ろが余分に残る）
//!
//! キーフレーム列は GOP が固定長でも `floor(boundary / KFI) * KFI` のような等間隔前提の
//! 式では選ばない。シーンチェンジ由来の IDR やキーフレームの欠落があっても正しく
//! 直前／直後を選べるよう、キーフレーム列に対する二分探索（[`nearest_keyframes`]）で選ぶ。
//!
//! 秒への換算はフレームレート一定を仮定せず、境界とキーフレームの間にあるフレームの
//! `duration`（time_base 単位）を合計し、`.dtvi` ヘッダの `time_base_num` / `time_base_den`
//! で秒に変換する（[`seconds_between`]）。
//!
//! このモジュールは文字列を組み立てて返すだけで、自分では標準出力に何も書かない。
//! `--report` が指定されたときにだけ、呼び出し側（analyze 側の配線）が
//! [`format_report`] の戻り値を表示する。これにより「`--report` なしのときは
//! 余計な標準出力を出さない」という条件を自然に満たす。

pub mod missed;

use crate::cli::Snap;
use crate::dtvi::Dtvi;
use crate::jls::JlsEntry;
use crate::order::DisplayIdx;
use crate::trim::TrimList;

/// カット境界が保持区間の開始側か終了側か。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundaryKind {
    /// 保持区間の開始（含む）。
    Start,
    /// 保持区間の終端（含まない、半開区間）。
    End,
}

/// 1つのカット境界と、直前／直後のキーフレームとの距離。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BoundaryDistance {
    /// 境界そのもの（表示順のフレーム番号）。
    pub boundary: DisplayIdx,
    /// この境界が保持区間の開始か終了か。
    pub kind: BoundaryKind,
    /// 境界以下で最大のキーフレーム。存在しなければ `None`。
    pub prev_keyframe: Option<DisplayIdx>,
    /// `prev_keyframe` と境界の距離（秒）。キーフレームは境界以下なので通常は
    /// 0 以下になるが、負値もありうるという前提で `Option<f64>` にしている。
    pub prev_keyframe_delta_sec: Option<f64>,
    /// 境界以上で最小のキーフレーム。存在しなければ `None`。
    pub next_keyframe: Option<DisplayIdx>,
    /// `next_keyframe` と境界の距離（秒）。通常は 0 以上。
    pub next_keyframe_delta_sec: Option<f64>,
}

/// トリム全体のサマリ。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ReportSummary {
    /// 保持区間の数。
    pub kept_ranges: usize,
    /// カット境界の数（保持区間数のちょうど2倍）。
    pub cut_boundaries: usize,
    /// スナップによって余分に残る（または余分に削れる）フレーム数の合計。
    /// `outward` では CM が余分に残る量、`inward` では本編が余分に削れる量を表す。
    pub extra_retained_frames_total: u32,
    /// `extra_retained_frames_total` を秒に換算した値。
    pub extra_retained_seconds_total: f64,
    /// カット境界1つあたりの平均（`extra_retained_seconds_total / cut_boundaries`）。
    pub extra_retained_seconds_average: f64,
    /// スナップ後に保持される区間の合計時間（秒）。
    pub output_duration_seconds: f64,
}

/// `.dtvi` ヘッダから `(time_base_num, time_base_den)` を取得する。
/// キーが無い、または数値としてパースできない場合は `1/1`（フレーム数=秒）にする。
fn time_base(dtvi: &Dtvi) -> (f64, f64) {
    let parse = |key: &str| {
        dtvi.header_value(key)
            .and_then(|v| v.trim().parse::<f64>().ok())
    };
    (
        parse("time_base_num").unwrap_or(1.0),
        parse("time_base_den").unwrap_or(1.0),
    )
}

/// 半開区間 `[from, to)`（表示順のフレーム番号、`from <= to` を仮定）に含まれる
/// フレームの `duration`（time_base 単位）を合計する。範囲は `frames.len()` に
/// クランプするので、境界が壊れていてもパニックはしない。
fn duration_units_between(dtvi: &Dtvi, from: u32, to: u32) -> i64 {
    let len = dtvi.frames.len() as u32;
    let from = from.min(len) as usize;
    let to = to.min(len) as usize;
    if from >= to {
        return 0;
    }
    dtvi.frames[from..to].iter().map(|f| f.duration).sum()
}

/// `from` から `to` までの時間差を秒で返す（`from <= to` を仮定、符号は常に非負）。
fn seconds_between(dtvi: &Dtvi, from: u32, to: u32, tb: (f64, f64)) -> f64 {
    let (num, den) = tb;
    duration_units_between(dtvi, from, to) as f64 * num / den
}

/// キーフレームの `frame_number` 一覧（昇順）から、`boundary` 以下の最大値と
/// `boundary` 以上の最小値を二分探索で求める。
///
/// 等間隔（GOP 固定長）を前提にした `floor(boundary / KFI) * KFI` という式は使わない。
/// シーンチェンジ由来の IDR でキーフレーム間隔が不揃いになっても正しく動く必要があるため。
fn nearest_keyframes(keyframes: &[u32], boundary: u32) -> (Option<u32>, Option<u32>) {
    let prev_count = keyframes.partition_point(|&k| k <= boundary);
    let prev = if prev_count > 0 {
        Some(keyframes[prev_count - 1])
    } else {
        None
    };

    let next_from = keyframes.partition_point(|&k| k < boundary);
    let next = keyframes.get(next_from).copied();

    (prev, next)
}

/// `.dtvi` からキーパケットの `frame_number` 一覧を昇順で取り出す。
fn keyframe_numbers(dtvi: &Dtvi) -> Vec<u32> {
    dtvi.frames
        .iter()
        .filter(|f| f.is_key_packet())
        .map(|f| f.frame_number.0)
        .collect()
}

fn distance_for_boundary(
    dtvi: &Dtvi,
    keyframes: &[u32],
    boundary: DisplayIdx,
    kind: BoundaryKind,
    tb: (f64, f64),
) -> BoundaryDistance {
    let (prev, next) = nearest_keyframes(keyframes, boundary.0);

    let prev_keyframe_delta_sec = prev.map(|pk| -seconds_between(dtvi, pk, boundary.0, tb));
    let next_keyframe_delta_sec = next.map(|nk| seconds_between(dtvi, boundary.0, nk, tb));

    BoundaryDistance {
        boundary,
        kind,
        prev_keyframe: prev.map(DisplayIdx),
        prev_keyframe_delta_sec,
        next_keyframe: next.map(DisplayIdx),
        next_keyframe_delta_sec,
    }
}

/// トリムの各保持区間の `start()` / `end()` ごとに、直前／直後のキーフレームとの
/// 距離を求める。保持区間数が N ならちょうど 2N 個返る（開始, 終了, 開始, 終了, ...の順）。
pub fn boundary_distances(trim: &TrimList, dtvi: &Dtvi) -> Vec<BoundaryDistance> {
    let keyframes = keyframe_numbers(dtvi);
    let tb = time_base(dtvi);

    trim.ranges()
        .iter()
        .flat_map(|range| {
            [
                distance_for_boundary(dtvi, &keyframes, range.start(), BoundaryKind::Start, tb),
                distance_for_boundary(dtvi, &keyframes, range.end(), BoundaryKind::End, tb),
            ]
        })
        .collect()
}

/// `snap` の方向でカット境界をキーフレームに丸めたときのサマリを求める。
///
/// `outward`（既定）: 開始境界は手前のキーフレームへ、終了境界は次のキーフレームへ動かす
/// （本編を削らず CM を残す方向）。`inward` はその逆で、開始境界は次のキーフレームへ、
/// 終了境界は手前のキーフレームへ動かす（CM を残さない代わりに本編を削るリスクがある方向）。
pub fn summarize(trim: &TrimList, dtvi: &Dtvi, snap: Snap) -> ReportSummary {
    let keyframes = keyframe_numbers(dtvi);
    let tb = time_base(dtvi);

    let mut extra_retained_frames_total: u32 = 0;
    let mut extra_retained_duration_units: i64 = 0;
    let mut kept_duration_units: i64 = 0;

    for range in trim.ranges() {
        let start = range.start().0;
        let end = range.end().0;

        let (start_prev, start_next) = nearest_keyframes(&keyframes, start);
        let (end_prev, end_next) = nearest_keyframes(&keyframes, end);

        let (snapped_start, snapped_end) = match snap {
            Snap::Outward => (start_prev.unwrap_or(start), end_next.unwrap_or(end)),
            Snap::Inward => (start_next.unwrap_or(start), end_prev.unwrap_or(end)),
        };

        extra_retained_frames_total += start.abs_diff(snapped_start);
        extra_retained_frames_total += end.abs_diff(snapped_end);

        // フレーム数をそのまま time_base 換算してはいけない（1フレーム = 1 time_base
        // 単位とは限らない）。実際のフレームの duration を合計してから秒に変換する。
        extra_retained_duration_units +=
            duration_units_between(dtvi, start.min(snapped_start), start.max(snapped_start));
        extra_retained_duration_units +=
            duration_units_between(dtvi, end.min(snapped_end), end.max(snapped_end));

        let (lo, hi) = if snapped_start <= snapped_end {
            (snapped_start, snapped_end)
        } else {
            (snapped_end, snapped_start)
        };
        kept_duration_units += duration_units_between(dtvi, lo, hi);
    }

    let (tb_num, tb_den) = tb;
    let cut_boundaries = trim.ranges().len() * 2;
    let extra_retained_seconds_total = extra_retained_duration_units as f64 * tb_num / tb_den;
    let extra_retained_seconds_average = if cut_boundaries > 0 {
        extra_retained_seconds_total / cut_boundaries as f64
    } else {
        0.0
    };

    ReportSummary {
        kept_ranges: trim.ranges().len(),
        cut_boundaries,
        extra_retained_frames_total,
        extra_retained_seconds_total,
        extra_retained_seconds_average,
        output_duration_seconds: kept_duration_units as f64 * tb_num / tb_den,
    }
}

/// 秒数を `分:秒`（秒は2桁）形式にする。`docs/measurements.md` の表記（`24:29` 等）に合わせる。
fn format_mm_ss(total_seconds: f64) -> String {
    let total = total_seconds.round().max(0.0) as u64;
    format!("{}:{:02}", total / 60, total % 60)
}

/// `--report` 用のプレーンテキストレポートを組み立てる。
///
/// 標準出力への書き込みは一切行わない（`println!` 等を含まない）。呼び出し側が
/// `--report` 指定時にだけこの戻り値を表示する設計にすることで、指定なしのときに
/// 余計な出力が出ないことを保証する。
pub fn format_report(
    trim: &TrimList,
    dtvi: &Dtvi,
    jls_entries: &[JlsEntry],
    snap_outward: bool,
) -> String {
    let snap = if snap_outward {
        Snap::Outward
    } else {
        Snap::Inward
    };
    let summary = summarize(trim, dtvi, snap);
    let distances = boundary_distances(trim, dtvi);

    let mut out = String::new();

    out.push_str(&format!(
        "保持区間数 {} / カット境界数 {}\n",
        summary.kept_ranges, summary.cut_boundaries
    ));
    out.push_str(&format!(
        "余分に残る量 {:.1}s（境界あたり平均 {:.2}s）\n",
        summary.extra_retained_seconds_total, summary.extra_retained_seconds_average
    ));
    out.push_str(&format!(
        "出力の長さ {}\n",
        format_mm_ss(summary.output_duration_seconds)
    ));
    out.push('\n');

    for d in &distances {
        let kind = match d.kind {
            BoundaryKind::Start => "開始",
            BoundaryKind::End => "終了",
        };
        let prev = match (d.prev_keyframe, d.prev_keyframe_delta_sec) {
            (Some(k), Some(sec)) => format!("{:>6} ({sec:+.2}s)", k.0),
            _ => "     -".to_string(),
        };
        let next = match (d.next_keyframe, d.next_keyframe_delta_sec) {
            (Some(k), Some(sec)) => format!("{:>6} ({sec:+.2}s)", k.0),
            _ => "     -".to_string(),
        };
        out.push_str(&format!(
            "境界({kind}) {:>6} : 直前KF={prev}  直後KF={next}\n",
            d.boundary.0
        ));
    }

    let cm_entries: Vec<&JlsEntry> = jls_entries.iter().filter(|e| e.is_cm()).collect();
    if !cm_entries.is_empty() {
        out.push('\n');
        out.push_str("CM ブロックの15秒格子誤差:\n");
        for e in cm_entries {
            out.push_str(&format!(
                "  {:>6}-{:<6} 誤差 {:+} フレーム\n",
                e.start, e.end, e.error_frames
            ));
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtvi::{DtviFrame, FLAG_KEY_PACKET, FLAG_VALID_DTS, FLAG_VALID_PTS};
    use crate::order::DecodeIdx;
    use std::collections::HashMap;

    fn approx_eq(a: f64, b: f64, eps: f64) -> bool {
        (a - b).abs() <= eps
    }

    /// GOP=120固定、duration=1001（time_base 1/30000、つまり 29.97fps 相当）の
    /// 合成 `.dtvi` を作る。`docs/measurements.md` の実測値の再現に使う。
    fn make_gop120_dtvi(frame_count: u32) -> Dtvi {
        let mut header = HashMap::new();
        header.insert("time_base_num".to_string(), "1".to_string());
        header.insert("time_base_den".to_string(), "30000".to_string());

        let frames = (0..frame_count)
            .map(|n| {
                let flags = if n % 120 == 0 {
                    FLAG_KEY_PACKET | FLAG_VALID_PTS | FLAG_VALID_DTS
                } else {
                    FLAG_VALID_PTS | FLAG_VALID_DTS
                };
                DtviFrame {
                    frame_number: DisplayIdx(n),
                    sample_number: DecodeIdx(n),
                    random_access_sample: DecodeIdx((n / 120) * 120),
                    file_offset: 0,
                    pts: n as i64 * 1001,
                    dts: n as i64 * 1001,
                    duration: 1001,
                    flags,
                }
            })
            .collect();

        Dtvi {
            format_version: 1,
            header,
            frames,
        }
    }

    #[test]
    fn boundary_distances_match_measured_example() {
        // docs/measurements.md の「カット点とキーフレームの距離（ファイル A）」を再現する。
        // カット点 63 : 直前KF=0 (-2.10s) 直後KF=120 (+1.90s)
        // カット点 5008: 直前KF=4920 (-2.94s) 直後KF=5040 (+1.07s)
        let dtvi = make_gop120_dtvi(5041);
        let trim = TrimList::parse("Trim(63,5007)").expect("should parse");

        let distances = boundary_distances(&trim, &dtvi);
        assert_eq!(distances.len(), 2);

        let start = &distances[0];
        assert_eq!(start.kind, BoundaryKind::Start);
        assert_eq!(start.boundary, DisplayIdx(63));
        assert_eq!(start.prev_keyframe, Some(DisplayIdx(0)));
        assert_eq!(start.next_keyframe, Some(DisplayIdx(120)));
        assert!(approx_eq(
            start.prev_keyframe_delta_sec.unwrap(),
            -2.10,
            0.01
        ));
        assert!(approx_eq(
            start.next_keyframe_delta_sec.unwrap(),
            1.90,
            0.01
        ));

        let end = &distances[1];
        assert_eq!(end.kind, BoundaryKind::End);
        assert_eq!(end.boundary, DisplayIdx(5008));
        assert_eq!(end.prev_keyframe, Some(DisplayIdx(4920)));
        assert_eq!(end.next_keyframe, Some(DisplayIdx(5040)));
        assert!(approx_eq(end.prev_keyframe_delta_sec.unwrap(), -2.94, 0.01));
        assert!(approx_eq(end.next_keyframe_delta_sec.unwrap(), 1.07, 0.01));
    }

    #[test]
    fn summarize_computes_extra_retained_outward() {
        let dtvi = make_gop120_dtvi(5041);
        let trim = TrimList::parse("Trim(63,5007)").expect("should parse");

        let summary = summarize(&trim, &dtvi, Snap::Outward);

        assert_eq!(summary.kept_ranges, 1);
        assert_eq!(summary.cut_boundaries, 2);
        // 開始: 63 - 0 = 63 フレーム、終了: 5040 - 5008 = 32 フレーム
        assert_eq!(summary.extra_retained_frames_total, 63 + 32);

        let expected_seconds = (63 + 32) as f64 * 1001.0 / 30000.0;
        assert!(approx_eq(
            summary.extra_retained_seconds_total,
            expected_seconds,
            1e-9
        ));
        assert!(approx_eq(
            summary.extra_retained_seconds_average,
            expected_seconds / 2.0,
            1e-9
        ));

        // 出力長: スナップ後の [0, 5040) の duration 合計。
        let expected_output = 5040.0 * 1001.0 / 30000.0;
        assert!(approx_eq(
            summary.output_duration_seconds,
            expected_output,
            1e-6
        ));
    }

    #[test]
    fn summarize_inward_shrinks_kept_range_instead() {
        let dtvi = make_gop120_dtvi(5041);
        let trim = TrimList::parse("Trim(63,5007)").expect("should parse");

        let summary = summarize(&trim, &dtvi, Snap::Inward);

        // 開始境界は次のKF(120)へ、終了境界は手前のKF(4920)へ動く。
        // 開始: 120 - 63 = 57 フレーム、終了: 5008 - 4920 = 88 フレーム
        assert_eq!(summary.extra_retained_frames_total, 57 + 88);

        let expected_output = (4920.0 - 120.0) * 1001.0 / 30000.0;
        assert!(approx_eq(
            summary.output_duration_seconds,
            expected_output,
            1e-6
        ));
    }

    #[test]
    fn nearest_keyframes_handles_uneven_spacing() {
        // 不等間隔なキーフレーム列 (0, 100, 250, 400) でも正しく直前／直後を選べること。
        let keyframes = [0u32, 100, 250, 400];

        assert_eq!(nearest_keyframes(&keyframes, 0), (Some(0), Some(0)));
        assert_eq!(nearest_keyframes(&keyframes, 63), (Some(0), Some(100)));
        assert_eq!(nearest_keyframes(&keyframes, 100), (Some(100), Some(100)));
        assert_eq!(nearest_keyframes(&keyframes, 230), (Some(100), Some(250)));
        assert_eq!(nearest_keyframes(&keyframes, 400), (Some(400), Some(400)));
        assert_eq!(nearest_keyframes(&keyframes, 999), (Some(400), None));
    }

    #[test]
    fn nearest_keyframes_returns_none_before_first_keyframe() {
        let keyframes = [100u32, 250, 400];
        assert_eq!(nearest_keyframes(&keyframes, 50), (None, Some(100)));
    }

    #[test]
    fn boundary_distances_on_uneven_keyframes_computes_correct_seconds() {
        // time_base 1/1、duration 1 のフレームで、キーフレーム間隔を敢えて不揃いにする
        // (0, 100, 250, 400)。floor(boundary / KFI) * KFI という等間隔前提の式では
        // 正しく選べないことを確認する。
        let mut header = HashMap::new();
        header.insert("time_base_num".to_string(), "1".to_string());
        header.insert("time_base_den".to_string(), "1".to_string());

        let keyframe_positions = [0u32, 100, 250, 400];
        let frame_count = 450;
        let frames = (0..frame_count)
            .map(|n| {
                let flags = if keyframe_positions.contains(&n) {
                    FLAG_KEY_PACKET
                } else {
                    0
                };
                DtviFrame {
                    frame_number: DisplayIdx(n),
                    sample_number: DecodeIdx(n),
                    random_access_sample: DecodeIdx(0),
                    file_offset: 0,
                    pts: n as i64,
                    dts: n as i64,
                    duration: 1,
                    flags,
                }
            })
            .collect();
        let dtvi = Dtvi {
            format_version: 1,
            header,
            frames,
        };

        let trim = TrimList::parse("Trim(230,399)").expect("should parse");
        let distances = boundary_distances(&trim, &dtvi);

        let start = &distances[0];
        assert_eq!(start.boundary, DisplayIdx(230));
        assert_eq!(start.prev_keyframe, Some(DisplayIdx(100)));
        assert_eq!(start.prev_keyframe_delta_sec, Some(-130.0));
        assert_eq!(start.next_keyframe, Some(DisplayIdx(250)));
        assert_eq!(start.next_keyframe_delta_sec, Some(20.0));

        let end = &distances[1];
        assert_eq!(end.boundary, DisplayIdx(400));
        assert_eq!(end.prev_keyframe, Some(DisplayIdx(400)));
        assert_eq!(end.prev_keyframe_delta_sec, Some(0.0));
        assert_eq!(end.next_keyframe, Some(DisplayIdx(400)));
        assert_eq!(end.next_keyframe_delta_sec, Some(0.0));
    }

    #[test]
    fn format_report_returns_plain_string_and_includes_summary_and_cm_errors() {
        let dtvi = make_gop120_dtvi(5041);
        let trim = TrimList::parse("Trim(63,5007)").expect("should parse");
        let jls = vec![
            JlsEntry {
                start: 0,
                end: 62,
                duration_sec: 2,
                error_frames: 0,
                logo_sec: 2,
                label: ":L".to_string(),
            },
            JlsEntry {
                start: 6128,
                end: 6577,
                duration_sec: 15,
                error_frames: -1,
                logo_sec: 0,
                label: ":CM".to_string(),
            },
        ];

        let report = format_report(&trim, &dtvi, &jls, true);

        assert!(report.contains("保持区間数 1"));
        assert!(report.contains("カット境界数 2"));
        assert!(report.contains("CM ブロックの15秒格子誤差"));
        assert!(report.contains("6128"));
        assert!(!report.contains(":L"));
    }

    #[test]
    fn format_report_omits_cm_section_when_no_cm_entries() {
        let dtvi = make_gop120_dtvi(5041);
        let trim = TrimList::parse("Trim(63,5007)").expect("should parse");

        let report = format_report(&trim, &dtvi, &[], true);

        assert!(!report.contains("CM ブロック"));
    }
}
