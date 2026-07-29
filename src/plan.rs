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
use crate::order::{DecodeIdx, DisplayIdx, OrderMap};
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

/// スナップ済み区間から、出力に含める映像サンプルの `DecodeIdx` の列を作る。
///
/// 唯一の規則（docs/lossless-cut.md「【最重要】切り出しはパケット数で行う」節）:
///
/// > S の同期サンプルから、デコード順にちょうど `E - S` パケット取る
///
/// 閉じた GOP なら常に成立する（2 つの IDR の間にあるデコード順パケットの集合 ==
/// その間に表示されるフレームの集合）。並べ替え深度を知る必要はなく、時間指定
/// （`ffmpeg -t` 相当）も一切使わない。区間ごとに:
///
/// 1. 開始 `S`（表示順。`snap()` により同期サンプル上にあることが保証されている）に
///    対応する `DecodeIdx` を [`OrderMap::to_decode`] で引く。
/// 2. そこからデコード順に連番で `E - S` 個取る。
///
/// 全区間分を連結したものを返す。
pub fn keep_list(snapped: &[SnappedRange], order: &OrderMap) -> Result<Vec<DecodeIdx>> {
    let mut result = Vec::new();

    for range in snapped {
        let start_display = range.start.snapped;
        let end_display = range.end.snapped;
        let count = end_display - start_display; // DisplayIdx の Sub -> u32（フレーム数）

        let start_decode = order.to_decode(start_display).ok_or_else(|| {
            anyhow::anyhow!(
                "表示順インデックス {} に対応するデコード順インデックスが見つかりません",
                start_display.0
            )
        })?;

        for offset in 0..count {
            let decode = start_decode.checked_add(offset).ok_or_else(|| {
                anyhow::anyhow!("デコード順インデックスの計算がオーバーフローしました")
            })?;
            result.push(decode);
        }
    }

    Ok(result)
}

/// 保持区間の集合の**補集合**を返す（CM として除去した区間を別ファイルに出す機能で使う）。
///
/// docs/lossless-cut.md「CM 側（除去した区間）を別ファイルに出す」節の設計をそのまま
/// 実装したもの。`snapped` は `snap()` が返す、外側スナップ済み・昇順・非重複な保持区間
/// の列であることが前提（`snap(..., Snap::Outward)` の結果のみを渡すこと。`Snap::Inward`
/// では保持区間が退化（`end < start`）しうり、呼び出し側でこの関数を呼ぶ前に拒否する
/// 設計になっている。[`crate::commands`] 参照）。
///
/// 補集合の各区間は `[前の保持区間の終端, 次の保持区間の開始)`（先頭は `[0, 最初の開始)`、
/// 末尾は `[最後の終端, total_frames)`）。外側スナップ後の保持区間の両端は必ず同期サンプル
/// 上にあるため、補集合の両端も必ず同期サンプル上に来る。したがって「S の同期サンプルから
/// デコード順に `E - S` パケット取る」規則が補集合にもそのまま成立し、追加のスナップ処理は
/// 一切不要（[`keep_list`] にそのまま渡せる）。
///
/// # 空区間は捨てる
///
/// 次のいずれかに当てはまる場合、その区間は `start >= end` になるため結果から除く
/// （境界ケースは docs/lossless-cut.md に列挙されている）:
///
/// - 先頭の保持区間が `0` から始まる → 先頭の補集合が空
/// - 最後の保持区間の終端が `total_frames`（ファイル末尾）→ 末尾の補集合が空
/// - 保持区間同士が隣接している（間に隙間が無い）→ その間の補集合が空
/// - `snapped` が空 → 補集合は `[0, total_frames)` の1区間のみ
///
/// # `original` / `delta_frames` の意味について
///
/// [`SnappedBoundary::original`] は本来「Trim リストに書かれていた値」、
/// [`SnappedBoundary::delta_frames`] は「そこからのスナップ距離」を表す。しかし補集合の
/// 境界（保持区間の終端や次の保持区間の開始そのもの）は Trim リストの値に由来しないため、
/// この意味を持たせられない。そのため本関数が返す境界は一律 `original == snapped`
/// （= スナップ距離 `delta_frames == 0`）にする。「スナップした」のではなく「保持区間の
/// 境界をそのまま補集合の境界として採用した」ことを表す値であり、レポート表示で
/// 「スナップでずれた量」として解釈しないよう注意すること。
pub fn complement_ranges(snapped: &[SnappedRange], total_frames: u32) -> Vec<SnappedRange> {
    let total = DisplayIdx(total_frames);
    let mut result = Vec::new();
    let mut cursor = DisplayIdx(0);

    for range in snapped {
        push_gap_if_nonempty(&mut result, cursor, range.start.snapped);
        cursor = range.end.snapped;
    }
    push_gap_if_nonempty(&mut result, cursor, total);

    result
}

/// `[start, end)` が空でなければ（`start < end` なら）`SnappedRange` として `result` に積む。
///
/// `original == snapped` / `delta_frames == 0` にする理由は [`complement_ranges`] の
/// doc comment を参照。
fn push_gap_if_nonempty(result: &mut Vec<SnappedRange>, start: DisplayIdx, end: DisplayIdx) {
    if start >= end {
        return;
    }
    result.push(SnappedRange {
        start: SnappedBoundary::new(start, start),
        end: SnappedBoundary::new(end, end),
    });
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

    use std::collections::BTreeSet;

    /// 3 GOP（各 4 フレーム）分の `OrderMap` を合成する。各 GOP は
    /// 表示順 `I B B P`・デコード順 `I P B B` という典型的な B フレーム並べ替えを
    /// 模している（`order.rs` のテストにある並べ替えパターンを GOP 単位に拡張したもの）。
    /// 各 GOP は閉じている（デコード順のブロックが GOP をまたがない）ので、
    /// GOP の先頭（同期サンプル）だけがデコード順ブロックの先頭になる。
    fn three_gop_order_map() -> OrderMap {
        let mut pairs = Vec::new();
        for gop in 0..3u32 {
            let d = gop * 4; // display の GOP 先頭
            let c = gop * 4; // decode の GOP 先頭
            pairs.push((DisplayIdx(d), DecodeIdx(c))); // I（同期サンプル）
            pairs.push((DisplayIdx(d + 3), DecodeIdx(c + 1))); // P（2 番目にデコード）
            pairs.push((DisplayIdx(d + 1), DecodeIdx(c + 2))); // B（3 番目にデコード、2 番目に表示）
            pairs.push((DisplayIdx(d + 2), DecodeIdx(c + 3))); // B（4 番目にデコード、3 番目に表示）
        }
        OrderMap::new(pairs)
    }

    fn three_gop_sync_display() -> Vec<DisplayIdx> {
        vec![DisplayIdx(0), DisplayIdx(4), DisplayIdx(8)]
    }

    /// snap() を経由せず、同期サンプル上にある区間を直接 `SnappedRange` として組み立てる
    /// （`snap()` が「S は同期サンプル上」を保証するという前提を直接利用する）。
    fn range_on_sync(start: u32, end: u32) -> SnappedRange {
        SnappedRange {
            start: SnappedBoundary::new(DisplayIdx(start), DisplayIdx(start)),
            end: SnappedBoundary::new(DisplayIdx(end), DisplayIdx(end)),
        }
    }

    #[test]
    fn keep_list_packet_count_matches_e_minus_s_per_range() {
        let order = three_gop_order_map();
        // GOP0 を保持し GOP1（CM 相当）を捨て、GOP2 を保持する。
        let ranges = vec![range_on_sync(0, 4), range_on_sync(8, 12)];

        let keep = keep_list(&ranges, &order).expect("keep_list should succeed");

        assert_eq!(keep.len(), 8);
        // 区間ごとの内訳（4 パケット + 4 パケット）も確認する。
        let first_range_count: u32 = 4;
        let second_range_count: u32 = 12 - 8;
        assert_eq!(first_range_count + second_range_count, keep.len() as u32);
    }

    #[test]
    fn keep_list_first_packet_of_each_range_is_a_sync_sample() {
        let order = three_gop_order_map();
        let sync_display = three_gop_sync_display();
        let ranges = vec![range_on_sync(0, 4), range_on_sync(8, 12)];

        let keep = keep_list(&ranges, &order).expect("keep_list should succeed");

        // 各区間の先頭パケット（オフセット 0）が同期サンプルであることを確認する。
        // 区間0はkeep[0]、区間1はkeep[4]から始まる（各区間4パケット）。
        let first_of_range0 = keep[0];
        let first_of_range1 = keep[4];

        for decode in [first_of_range0, first_of_range1] {
            let display = order
                .to_display(decode)
                .expect("decode index should map back to a display index");
            assert!(
                sync_display.contains(&display),
                "先頭パケット (decode={:?}, display={:?}) が同期サンプルではありません",
                decode,
                display
            );
        }
    }

    #[test]
    fn keep_list_display_order_has_no_gaps_within_each_range() {
        let order = three_gop_order_map();
        let ranges = vec![range_on_sync(0, 4), range_on_sync(8, 12)];

        let keep = keep_list(&ranges, &order).expect("keep_list should succeed");

        // 区間0 (decode 0..4 相当) の表示順集合が [0,4) と一致することを確認する。
        let range0_display: BTreeSet<u32> = keep[0..4]
            .iter()
            .map(|&d| order.to_display(d).expect("should map to display").0)
            .collect();
        let expected0: BTreeSet<u32> = (0..4).collect();
        assert_eq!(range0_display, expected0);

        // 区間1 (decode 8..12 相当) の表示順集合が [8,12) と一致することを確認する。
        let range1_display: BTreeSet<u32> = keep[4..8]
            .iter()
            .map(|&d| order.to_display(d).expect("should map to display").0)
            .collect();
        let expected1: BTreeSet<u32> = (8..12).collect();
        assert_eq!(range1_display, expected1);
    }

    #[test]
    fn keep_list_has_no_duplicate_decode_indices_across_ranges() {
        let order = three_gop_order_map();
        // 3 GOP すべてを別々の区間として保持し、区間をまたいだ重複が無いことを確認する。
        let ranges = vec![
            range_on_sync(0, 4),
            range_on_sync(4, 8),
            range_on_sync(8, 12),
        ];

        let keep = keep_list(&ranges, &order).expect("keep_list should succeed");

        let unique: BTreeSet<DecodeIdx> = keep.iter().copied().collect();
        assert_eq!(
            unique.len(),
            keep.len(),
            "区間をまたいで DecodeIdx が重複しています"
        );
        assert_eq!(keep.len(), 12);
    }

    // ---------------------------------------------------------------
    // complement_ranges のテスト。
    // ---------------------------------------------------------------

    /// `complement_ranges` の結果を `(start, end)` の組に変換する（`as_pairs` と同じ用途）。
    fn complement_pairs(snapped: &[SnappedRange], total_frames: u32) -> Vec<(u32, u32)> {
        as_pairs(&complement_ranges(snapped, total_frames))
    }

    #[test]
    fn complement_of_single_middle_range_has_two_gaps() {
        // 手作業での確認結果（issue本文）と同じ設定: 20秒599フレームのフィクスチャで
        // Trim(360,479) 相当（半開区間 [360,480)）を保持した場合。
        let ranges = vec![range_on_sync(360, 480)];
        assert_eq!(complement_pairs(&ranges, 599), vec![(0, 360), (480, 599)]);
    }

    #[test]
    fn complement_is_empty_when_single_range_covers_whole_file() {
        let ranges = vec![range_on_sync(0, 599)];
        assert_eq!(complement_pairs(&ranges, 599), Vec::<(u32, u32)>::new());
    }

    #[test]
    fn complement_drops_empty_head_when_first_range_starts_at_zero() {
        // 先頭が0始まり → 先頭の補集合は空になるので、末尾の1区間だけが残る。
        let ranges = vec![range_on_sync(0, 120)];
        assert_eq!(complement_pairs(&ranges, 599), vec![(120, 599)]);
    }

    #[test]
    fn complement_drops_empty_tail_when_last_range_ends_at_total_frames() {
        // 末尾がファイル末尾 → 末尾の補集合は空になるので、先頭の1区間だけが残る。
        let ranges = vec![range_on_sync(480, 599)];
        assert_eq!(complement_pairs(&ranges, 599), vec![(0, 480)]);
    }

    #[test]
    fn complement_drops_both_empty_ends_when_range_covers_middle_only_between_bounds() {
        // 先頭 (0始まり) と末尾 (ファイル末尾) の両方が空になり、中間の隙間だけが残る
        // ケース。複数の保持区間で先頭・末尾それぞれが境界に接している。
        let ranges = vec![range_on_sync(0, 120), range_on_sync(360, 599)];
        assert_eq!(complement_pairs(&ranges, 599), vec![(120, 360)]);
    }

    #[test]
    fn complement_is_empty_when_adjacent_ranges_leave_no_gap() {
        // 保持区間が隣接している（間に隙間が無い）→ その間の補集合は空になる。
        let ranges = vec![range_on_sync(0, 120), range_on_sync(120, 599)];
        assert_eq!(complement_pairs(&ranges, 599), Vec::<(u32, u32)>::new());
    }

    #[test]
    fn complement_of_empty_keep_list_is_whole_file() {
        // 保持区間が空 → 補集合はファイル全体の1区間になる。
        let ranges: Vec<SnappedRange> = Vec::new();
        assert_eq!(complement_pairs(&ranges, 599), vec![(0, 599)]);
    }

    #[test]
    fn complement_of_multiple_ranges_has_gap_between_each_pair() {
        let ranges = vec![
            range_on_sync(0, 100),
            range_on_sync(200, 300),
            range_on_sync(400, 500),
        ];
        assert_eq!(
            complement_pairs(&ranges, 599),
            vec![(100, 200), (300, 400), (500, 599)]
        );
    }

    #[test]
    fn complement_boundaries_have_original_equal_to_snapped_and_zero_delta() {
        // 補集合の境界は Trim の値に由来しないため、original == snapped / delta_frames == 0
        // であることを直接確認する(complement_ranges のdoc comment参照)。
        let ranges = vec![range_on_sync(360, 480)];
        let result = complement_ranges(&ranges, 599);

        assert_eq!(result.len(), 2);
        for r in &result {
            assert_eq!(r.start.original, r.start.snapped);
            assert_eq!(r.start.delta_frames, 0);
            assert_eq!(r.end.original, r.end.snapped);
            assert_eq!(r.end.delta_frames, 0);
        }
    }

    #[test]
    fn complement_then_keep_list_packet_counts_sum_to_total_frames() {
        // 手作業での確認結果（issue本文）: 保持側120パケット + CM側479パケット = 599。
        let order = {
            // 599フレーム分の閉じたGOP相当のOrderMapは重いので、ここではdisplay==decode
            // の単純な対応で十分（complement_ranges自体はOrderMapを使わないため、
            // keep_listの入力としての整合性だけ確認できればよい）。
            let pairs: Vec<(DisplayIdx, DecodeIdx)> =
                (0..599u32).map(|i| (DisplayIdx(i), DecodeIdx(i))).collect();
            OrderMap::new(pairs)
        };

        let ranges = vec![range_on_sync(360, 480)];
        let complement = complement_ranges(&ranges, 599);

        let keep = keep_list(&ranges, &order).expect("keep_list should succeed");
        let cm_keep = keep_list(&complement, &order).expect("keep_list should succeed");

        assert_eq!(keep.len(), 120);
        assert_eq!(cm_keep.len(), 479);
        assert_eq!(keep.len() + cm_keep.len(), 599);

        // 互いに素であることも合わせて確認する。
        let keep_set: BTreeSet<DecodeIdx> = keep.iter().copied().collect();
        let cm_set: BTreeSet<DecodeIdx> = cm_keep.iter().copied().collect();
        assert!(keep_set.is_disjoint(&cm_set));
    }

    #[test]
    fn keep_list_errors_when_start_has_no_decode_mapping() {
        // OrderMap に存在しない表示順インデックスを起点にした区間はエラーになる。
        let order = OrderMap::new(vec![(DisplayIdx(0), DecodeIdx(0))]);
        let ranges = vec![range_on_sync(5, 10)];

        let err = keep_list(&ranges, &order).unwrap_err();
        assert!(err.to_string().contains("デコード順インデックス"));
    }
}
