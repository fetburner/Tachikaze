//! 表示順（display order）とデコード順（decode order）を型で区別する。
//!
//! - [`DisplayIdx`]: Trim の値、`.dtvi` の `frame_number` に対応する表示順のインデックス。
//! - [`DecodeIdx`]: mp4 のサンプル番号、`.dtvi` の `sample_number` に対応するデコード順のインデックス。
//!
//! 表示順とデコード順の混同がこのプロジェクト唯一の重大バグ源であり、混同しても
//! 例外は飛ばず間違った位置で切られたファイルが出てくるだけになる。そのため
//! `DisplayIdx` と `DecodeIdx` を別の型にし、相互変換を [`OrderMap`] 経由に限定することで
//! 混同をコンパイルエラーにする。
//!
//! 生の `u32` からの変換に `From<u32>` は実装しない。常に明示的なコンストラクタ
//! （`DisplayIdx(n)` / `DecodeIdx(n)` の直接構築）を通すことで、「どちらの順序の値か」を
//! 呼び出し側に毎回意識させる。

use std::ops::{Add, Sub};

/// 表示順（display order）のインデックス。Trim の値、`.dtvi` の `frame_number` に対応する。
///
/// `DecodeIdx` を要求する場所に誤って渡すとコンパイルエラーになる。
///
/// まず、正しい型を渡せばコンパイルできる:
///
/// ```
/// use tachikaze::order::DecodeIdx;
/// fn wants_decode(_: DecodeIdx) {}
///
/// wants_decode(DecodeIdx(0)); // 型が合っているのでコンパイルできる
/// ```
///
/// 上と**型以外は同じ**コードで、`DisplayIdx` を渡すとコンパイルに失敗する:
///
/// ```compile_fail
/// use tachikaze::order::{DecodeIdx, DisplayIdx};
/// fn wants_decode(_: DecodeIdx) {}
///
/// wants_decode(DisplayIdx(0)); // 型が違うのでコンパイルできない
/// ```
///
/// 上の2つを対にしているのは、`compile_fail` が**失敗した理由を区別しない**ため。
/// import ミスや構文エラーでも `compile_fail` は成立してしまうので、
/// 「型以外を同じにした版がコンパイルできる」ことを並べて示すことで、失敗の原因が
/// 型の取り違えであることを担保する。
///
/// なお `compile_fail,E0308` のようにエラーコードを添える書き方もあるが、
/// **stable の rustdoc はコードを検証しない**（無関係なコードを書いても通る。実際に
/// `E0425` に変えても通ることを確認した）。そのため書かずに上の対照方式を採っている。
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct DisplayIdx(pub u32);

/// デコード順（decode order）のインデックス。mp4 のサンプル番号、`.dtvi` の
/// `sample_number` に対応する。
///
/// `DisplayIdx` を要求する場所に誤って渡すとコンパイルエラーになる。
///
/// 正しい型を渡せばコンパイルできる（下の `compile_fail` の対照。対にしている理由は
/// [`DisplayIdx`] のドキュメント参照）:
///
/// ```
/// use tachikaze::order::DisplayIdx;
/// fn wants_display(_: DisplayIdx) {}
///
/// wants_display(DisplayIdx(0)); // 型が合っているのでコンパイルできる
/// ```
///
/// 上と**型以外は同じ**コードで、`DecodeIdx` を渡すとコンパイルに失敗する:
///
/// ```compile_fail
/// use tachikaze::order::{DecodeIdx, DisplayIdx};
/// fn wants_display(_: DisplayIdx) {}
///
/// wants_display(DecodeIdx(0)); // 型が違うのでコンパイルできない
/// ```
#[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
pub struct DecodeIdx(pub u32);

impl Sub for DisplayIdx {
    type Output = u32;

    /// 同種同士の差はフレーム数（`u32`）であり、インデックスではない。
    fn sub(self, rhs: Self) -> u32 {
        self.0 - rhs.0
    }
}

impl Sub for DecodeIdx {
    type Output = u32;

    /// 同種同士の差はフレーム数（`u32`）であり、インデックスではない。
    fn sub(self, rhs: Self) -> u32 {
        self.0 - rhs.0
    }
}

impl Add<u32> for DisplayIdx {
    type Output = DisplayIdx;

    fn add(self, rhs: u32) -> DisplayIdx {
        DisplayIdx(self.0 + rhs)
    }
}

impl Add<u32> for DecodeIdx {
    type Output = DecodeIdx;

    fn add(self, rhs: u32) -> DecodeIdx {
        DecodeIdx(self.0 + rhs)
    }
}

impl DisplayIdx {
    // 現状の cut パイプラインは表示順側の加算をオーバーフロー検査なしで行っている
    // （フレーム数が u32 を溢れる規模にはならない）。DecodeIdx::checked_add と対称に
    // 保つため、また将来の入力サイズ拡大に備えて残す。
    #[allow(dead_code)]
    /// オーバーフローしない加算。
    pub fn checked_add(self, rhs: u32) -> Option<DisplayIdx> {
        self.0.checked_add(rhs).map(DisplayIdx)
    }
}

impl DecodeIdx {
    /// オーバーフローしない加算。
    pub fn checked_add(self, rhs: u32) -> Option<DecodeIdx> {
        self.0.checked_add(rhs).map(DecodeIdx)
    }
}

/// `DisplayIdx` と `DecodeIdx` の相互変換を担う写像。
///
/// 表示順とデコード順の相互変換はこの型を経由するときのみ許可する。実データからの構築は
/// 別 issue（#27）で行うため、ここでは API と往復可能な最小実装のみを提供する。
pub struct OrderMap {
    /// (display, decode) のペアを保持する。過剰な最適化はせず、線形探索で十分とする。
    pairs: Vec<(DisplayIdx, DecodeIdx)>,
}

impl OrderMap {
    /// (display, decode) のペア列から `OrderMap` を構築する。
    pub fn new(pairs: Vec<(DisplayIdx, DecodeIdx)>) -> Self {
        Self { pairs }
    }

    /// 表示順インデックスからデコード順インデックスを求める。
    pub fn to_decode(&self, i: DisplayIdx) -> Option<DecodeIdx> {
        self.pairs
            .iter()
            .find_map(|&(d, s)| if d == i { Some(s) } else { None })
    }

    /// デコード順インデックスから表示順インデックスを求める。
    pub fn to_display(&self, i: DecodeIdx) -> Option<DisplayIdx> {
        self.pairs
            .iter()
            .find_map(|&(d, s)| if s == i { Some(d) } else { None })
    }

    /// 対応の件数。
    pub fn len(&self) -> usize {
        self.pairs.len()
    }

    // len() は verify.rs 等から使われるが、is_empty() 自体を呼ぶ箇所は
    // まだ無い（`clippy::len_without_is_empty` を避けるための対の実装として残す）。
    #[allow(dead_code)]
    /// 対応が空かどうか。
    pub fn is_empty(&self) -> bool {
        self.pairs.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_idx_sub_returns_u32_frame_count() {
        let a = DisplayIdx(10);
        let b = DisplayIdx(3);
        let diff: u32 = a - b;
        assert_eq!(diff, 7);
    }

    #[test]
    fn decode_idx_sub_returns_u32_frame_count() {
        let a = DecodeIdx(10);
        let b = DecodeIdx(3);
        let diff: u32 = a - b;
        assert_eq!(diff, 7);
    }

    #[test]
    fn display_idx_add_u32() {
        assert_eq!(DisplayIdx(5) + 2, DisplayIdx(7));
    }

    #[test]
    fn decode_idx_add_u32() {
        assert_eq!(DecodeIdx(5) + 2, DecodeIdx(7));
    }

    #[test]
    fn checked_add_overflow_returns_none() {
        assert_eq!(DisplayIdx(u32::MAX).checked_add(1), None);
        assert_eq!(DecodeIdx(u32::MAX).checked_add(1), None);
    }

    #[test]
    fn checked_add_no_overflow_returns_some() {
        assert_eq!(DisplayIdx(1).checked_add(1), Some(DisplayIdx(2)));
        assert_eq!(DecodeIdx(1).checked_add(1), Some(DecodeIdx(2)));
    }

    #[test]
    fn order_map_round_trip_display_to_decode_to_display() {
        // 恒等写像ではなく、意図的にずらした対応（B フレームの並べ替えを想定）で往復を確認する。
        let pairs = vec![
            (DisplayIdx(0), DecodeIdx(0)),
            (DisplayIdx(1), DecodeIdx(2)),
            (DisplayIdx(2), DecodeIdx(1)),
            (DisplayIdx(3), DecodeIdx(3)),
        ];
        let map = OrderMap::new(pairs);

        for display in [DisplayIdx(0), DisplayIdx(1), DisplayIdx(2), DisplayIdx(3)] {
            let decode = map.to_decode(display).expect("decode index should exist");
            let round_tripped = map.to_display(decode).expect("display index should exist");
            assert_eq!(round_tripped, display);
        }
    }

    #[test]
    fn order_map_round_trip_decode_to_display_to_decode() {
        let pairs = vec![
            (DisplayIdx(0), DecodeIdx(0)),
            (DisplayIdx(1), DecodeIdx(2)),
            (DisplayIdx(2), DecodeIdx(1)),
            (DisplayIdx(3), DecodeIdx(3)),
        ];
        let map = OrderMap::new(pairs);

        for decode in [DecodeIdx(0), DecodeIdx(1), DecodeIdx(2), DecodeIdx(3)] {
            let display = map.to_display(decode).expect("display index should exist");
            let round_tripped = map.to_decode(display).expect("decode index should exist");
            assert_eq!(round_tripped, decode);
        }
    }

    #[test]
    fn order_map_missing_mapping_returns_none() {
        let map = OrderMap::new(vec![(DisplayIdx(0), DecodeIdx(0))]);
        assert_eq!(map.to_decode(DisplayIdx(99)), None);
        assert_eq!(map.to_display(DecodeIdx(99)), None);
    }

    #[test]
    fn order_map_len_and_is_empty() {
        let empty = OrderMap::new(vec![]);
        assert_eq!(empty.len(), 0);
        assert!(empty.is_empty());

        let non_empty = OrderMap::new(vec![(DisplayIdx(0), DecodeIdx(0))]);
        assert_eq!(non_empty.len(), 1);
        assert!(!non_empty.is_empty());
    }
}
