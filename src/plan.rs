// #30 以降の cut パイプラインから消費されるまで未使用。配線されたら外す。
#![allow(dead_code)]

//! Trim リスト（表示順、半開区間）をキーフレーム（同期サンプル）境界へスナップする。
//!
//! 再エンコードしないため、切れる位置は同期サンプル境界に限られる
//! （docs/lossless-cut.md「基本方針」節）。保持区間 `[S, E)` の両端をどちら向きに
//! 動かすかは [`crate::cli::Snap`] で選ぶ:
//!
//! - `Outward`（既定）: `S` は手前の同期サンプルへ、`E` は次の同期サンプルへ。
//!   本編を削らず CM を残す。
//! - `Inward`: 逆方向。本編が削れてもよいので CM を残さない。
//!
//! 同期サンプルは等間隔とは限らない（docs/lossless-cut.md「キーフレーム境界スナップの
//! 計算」節の注意）。そのため `sync_display_indices()` が返す実際の同期サンプル列に対して
//! 二分探索する。等間隔を仮定した `floor(S / KFI) * KFI` のような計算はしない。

use anyhow::{bail, Result};

use crate::cli::Snap;
use crate::order::DisplayIdx;
use crate::trim::TrimList;

/// スナップ前後の1境界（開始または終了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnappedBoundary {
    /// スナップ前の位置（Trim リストに書かれていた値）。
    pub original: DisplayIdx,
    /// スナップ後の位置（同期サンプル、またはファイル先頭/末尾）。
    pub snapped: DisplayIdx,
    /// `|snapped - original|`。レポートでの表示用。
    pub delta_frames: u32,
}

impl SnappedBoundary {
    fn new(original: DisplayIdx, snapped: DisplayIdx) -> Self {
        let delta_frames = snapped.0.abs_diff(original.0);
        SnappedBoundary {
            original,
            snapped,
            delta_frames,
        }
    }
}

/// スナップ後の1保持区間。半開区間 `[start.snapped, end.snapped)`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SnappedRange {
    pub start: SnappedBoundary,
    pub end: SnappedBoundary,
}

/// Trim リストの各保持区間を同期サンプル境界へスナップする。
///
/// - `trim`: パース済みの Trim リスト（表示順、半開区間、昇順・非重複が保証されている）。
/// - `sync_display`: 同期サンプルの `DisplayIdx` を昇順に並べたもの
///   （[`crate::mp4io::order_map::DisplayDecodeMap::sync_display_indices`] の戻り値）。
/// - `total_frames`: ファイル全体のフレーム数。終端がファイル末尾を超える、または
///   始端がファイル末尾以降になる場合の上限として使う（最後の GOP の後には同期サンプルが
///   存在しないため）。
/// - `direction`: `Outward`（CM を残す）/ `Inward`（CM を残さない）。
///
/// スナップ後に隣接する保持区間が重なっていればエラーを返す（マージはしない）。
pub fn snap(
    trim: &TrimList,
    sync_display: &[DisplayIdx],
    total_frames: u32,
    direction: Snap,
) -> Result<Vec<SnappedRange>> {
    let total = DisplayIdx(total_frames);

    let snapped: Vec<SnappedRange> = trim
        .ranges()
        .iter()
        .map(|range| {
            let (start_snapped, end_snapped) = match direction {
                Snap::Outward => (
                    floor_or_equal(sync_display, range.start()).unwrap_or(DisplayIdx(0)),
                    ceil_or_equal(sync_display, range.end()).unwrap_or(total),
                ),
                Snap::Inward => (
                    ceil_or_equal(sync_display, range.start()).unwrap_or(total),
                    floor_or_equal(sync_display, range.end()).unwrap_or(DisplayIdx(0)),
                ),
            };

            SnappedRange {
                start: SnappedBoundary::new(range.start(), start_snapped),
                end: SnappedBoundary::new(range.end(), end_snapped),
            }
        })
        .collect();

    // Trim リストは元々昇順・非重複が保証されているが、outward スナップは各区間を
    // 外側に広げるだけなので、隣接区間の開始インデックスの相対順序は変わらない。よって
    // 隣接ペアだけを見れば重なりを検出できる。
    for pair in snapped.windows(2) {
        let prev = &pair[0];
        let next = &pair[1];
        if next.start.snapped < prev.end.snapped {
            bail!(
                "スナップ後に保持区間が重なっています: 前区間の終端(半開)={}, 次区間の開始={}",
                prev.end.snapped.0,
                next.start.snapped.0
            );
        }
    }

    Ok(snapped)
}

/// `sync` のうち `value` **以下で最大**の要素を返す（無ければ `None`）。
fn floor_or_equal(sync: &[DisplayIdx], value: DisplayIdx) -> Option<DisplayIdx> {
    let idx = sync.partition_point(|&s| s <= value);
    if idx == 0 {
        None
    } else {
        Some(sync[idx - 1])
    }
}

/// `sync` のうち `value` **以上で最小**の要素を返す（無ければ `None`）。
fn ceil_or_equal(sync: &[DisplayIdx], value: DisplayIdx) -> Option<DisplayIdx> {
    let idx = sync.partition_point(|&s| s < value);
    sync.get(idx).copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 不等間隔な同期サンプル列。0, 100, 250, 400。
    fn irregular_sync() -> Vec<DisplayIdx> {
        vec![
            DisplayIdx(0),
            DisplayIdx(100),
            DisplayIdx(250),
            DisplayIdx(400),
        ]
    }

    fn as_pairs(ranges: &[SnappedRange]) -> Vec<(u32, u32)> {
        ranges
            .iter()
            .map(|r| (r.start.snapped.0, r.end.snapped.0))
            .collect()
    }

    #[test]
    fn floor_or_equal_finds_largest_at_or_below() {
        let sync = irregular_sync();
        assert_eq!(floor_or_equal(&sync, DisplayIdx(0)), Some(DisplayIdx(0)));
        assert_eq!(floor_or_equal(&sync, DisplayIdx(99)), Some(DisplayIdx(0)));
        assert_eq!(
            floor_or_equal(&sync, DisplayIdx(100)),
            Some(DisplayIdx(100))
        );
        assert_eq!(
            floor_or_equal(&sync, DisplayIdx(399)),
            Some(DisplayIdx(250))
        );
        assert_eq!(
            floor_or_equal(&sync, DisplayIdx(500)),
            Some(DisplayIdx(400))
        );
    }

    #[test]
    fn ceil_or_equal_finds_smallest_at_or_above() {
        let sync = irregular_sync();
        assert_eq!(ceil_or_equal(&sync, DisplayIdx(0)), Some(DisplayIdx(0)));
        assert_eq!(ceil_or_equal(&sync, DisplayIdx(1)), Some(DisplayIdx(100)));
        assert_eq!(ceil_or_equal(&sync, DisplayIdx(250)), Some(DisplayIdx(250)));
        assert_eq!(ceil_or_equal(&sync, DisplayIdx(401)), None);
    }

    #[test]
    fn outward_snaps_start_back_and_end_forward_on_irregular_spacing() {
        let sync = irregular_sync();
        let trim = TrimList::parse("Trim(120,299)").expect("should parse"); // [120, 300)
        let result = snap(&trim, &sync, 500, Snap::Outward).expect("should not overlap");

        assert_eq!(as_pairs(&result), vec![(100, 400)]);
        assert_eq!(result[0].start.original, DisplayIdx(120));
        assert_eq!(result[0].start.delta_frames, 20);
        assert_eq!(result[0].end.original, DisplayIdx(300));
        assert_eq!(result[0].end.delta_frames, 100);
    }

    #[test]
    fn outward_does_not_extend_end_already_on_sync_sample() {
        let sync = irregular_sync();
        // [120, 250) の終端 250 はちょうど同期サンプル。伸ばさない。
        let trim = TrimList::parse("Trim(120,249)").expect("should parse");
        let result = snap(&trim, &sync, 500, Snap::Outward).expect("should not overlap");

        assert_eq!(result[0].end.snapped, DisplayIdx(250));
        assert_eq!(result[0].end.delta_frames, 0);
    }

    #[test]
    fn outward_does_not_move_start_already_on_sync_sample() {
        let sync = irregular_sync();
        let trim = TrimList::parse("Trim(100,249)").expect("should parse");
        let result = snap(&trim, &sync, 500, Snap::Outward).expect("should not overlap");

        assert_eq!(result[0].start.snapped, DisplayIdx(100));
        assert_eq!(result[0].start.delta_frames, 0);
    }

    #[test]
    fn inward_snaps_start_forward_and_end_back_on_irregular_spacing() {
        let sync = irregular_sync();
        let trim = TrimList::parse("Trim(120,299)").expect("should parse"); // [120, 300)
        let result = snap(&trim, &sync, 500, Snap::Inward).expect("should not overlap");

        assert_eq!(as_pairs(&result), vec![(250, 250)]);
        assert_eq!(result[0].start.delta_frames, 130);
        assert_eq!(result[0].end.delta_frames, 50);
    }

    #[test]
    fn inward_does_not_move_boundaries_already_on_sync_samples() {
        let sync = irregular_sync();
        let trim = TrimList::parse("Trim(100,249)").expect("should parse"); // [100, 250)
        let result = snap(&trim, &sync, 500, Snap::Inward).expect("should not overlap");

        assert_eq!(as_pairs(&result), vec![(100, 250)]);
        assert_eq!(result[0].start.delta_frames, 0);
        assert_eq!(result[0].end.delta_frames, 0);
    }

    #[test]
    fn start_at_zero_stays_at_zero() {
        let sync = irregular_sync();
        let trim = TrimList::parse("Trim(0,99)").expect("should parse"); // [0, 100)
        let outward = snap(&trim, &sync, 500, Snap::Outward).expect("should not overlap");
        assert_eq!(outward[0].start.snapped, DisplayIdx(0));
        assert_eq!(outward[0].start.delta_frames, 0);

        let inward = snap(&trim, &sync, 500, Snap::Inward).expect("should not overlap");
        assert_eq!(inward[0].start.snapped, DisplayIdx(0));
        assert_eq!(inward[0].start.delta_frames, 0);
    }

    #[test]
    fn end_beyond_last_sync_stops_at_total_frames() {
        let sync = irregular_sync(); // 最後の同期サンプルは 400
        let trim = TrimList::parse("Trim(300,498)").expect("should parse"); // [300, 499)
        let result = snap(&trim, &sync, 500, Snap::Outward).expect("should not overlap");

        // 499 以上の同期サンプルは存在しないので、ファイル末尾 (total_frames) で止める。
        assert_eq!(result[0].end.original, DisplayIdx(499));
        assert_eq!(result[0].end.snapped, DisplayIdx(500));
        assert_eq!(result[0].end.delta_frames, 1);
    }

    #[test]
    fn end_equal_to_total_frames_is_edge_case() {
        let sync = irregular_sync();
        let trim = TrimList::parse("Trim(250,499)").expect("should parse"); // [250, 500)
        let result = snap(&trim, &sync, 500, Snap::Outward).expect("should not overlap");

        assert_eq!(result[0].start.snapped, DisplayIdx(250));
        assert_eq!(result[0].end.snapped, DisplayIdx(500));
    }

    #[test]
    fn overlapping_after_outward_snap_is_error() {
        // 2 GOP相当の同期サンプル: 0, 100, 200, 300。
        let sync = vec![
            DisplayIdx(0),
            DisplayIdx(100),
            DisplayIdx(200),
            DisplayIdx(300),
        ];
        // [50,120) と [130,180) は元々重ならないが、outward で
        // [0,200) と [100,200) になり重なる。
        let trim = TrimList::parse("Trim(50,119) ++ Trim(130,179)").expect("should parse");

        let err = snap(&trim, &sync, 300, Snap::Outward).unwrap_err();
        assert!(err.to_string().contains("重なっています"));
    }

    #[test]
    fn non_overlapping_after_outward_snap_is_ok() {
        let sync = vec![
            DisplayIdx(0),
            DisplayIdx(100),
            DisplayIdx(200),
            DisplayIdx(300),
        ];
        let trim = TrimList::parse("Trim(10,90) ++ Trim(210,290)").expect("should parse");

        let result = snap(&trim, &sync, 300, Snap::Outward).expect("should not overlap");
        assert_eq!(as_pairs(&result), vec![(0, 100), (200, 300)]);
    }
}
