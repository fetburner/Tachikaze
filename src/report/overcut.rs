//! 切りすぎ候補（本編なのにカットされる区間）を報告する。
//!
//! 本編が CM として扱われると、出力から本編が欠ける。CM が残る見逃しより実害が大きいが、
//! 候補を出す仕組みが無かった。ここでは `analyze` の成果物にある2つの材料から候補を出す。
//!
//! - カット区間にロゴが出ている（logoframe テキストから読んだロゴ表示区間）
//! - カット区間と重なる CM ブロックが15秒格子から外れている（`detail.jls` の誤差フレーム数）
//!
//! **mp4 は読まない。** 入力は [`TrimList`]・[`JlsEntry`] の列と、logoframe テキストを
//! [`crate::logo::interval::parse_text`] でパースしたロゴ表示区間だけ。
//!
//! **gate の「止める」条件には入れない**（[`crate::gate`] の「指標と扱い」）。ここは候補を
//! 返すだけで、呼び出し側が印を付けて人に見せる。
//!
//! ## 判定の対象
//!
//! 対象は「カットされる区間」、つまり `[0, total_frames)` から保持区間を除いた各区間。
//! 先頭と末尾の区間（前後の番組）も含める。前後の番組は本編ではないが、ここで除外すると
//! 本編の端が切られていても見逃すため、偽陽性を許して含める。その代わり先頭・末尾の区間
//! には常に印が付きうる。
//!
//! ## 呼び出し側の注意
//!
//! `logo` が `None`（ロゴ検出が無い録画）のとき、[`OvercutReason::Logo`] の判定を
//! していない。候補が0件でも「問題なし」ではない。呼び出し側は `logo` の有無を見て
//! 区別すること。

use crate::gate::CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES;
use crate::jls::JlsEntry;
use crate::trim::TrimList;

/// カット区間内でロゴ表示が続いたとみなす最小秒数（以上で [`OvercutReason::Logo`]）。
///
/// **未検証。** 実測の分布を見ていない。値を変えるときはこの doc comment と
/// `docs/architecture.md`「未解決事項」を同時に直す。
const LOGO_MIN_SEC: f64 = 5.0;

/// 切りすぎ候補（カットされる区間のうち、本編の可能性を示す材料があるもの）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OvercutCandidate {
    /// 表示順のフレーム番号（含む）
    pub start: u32,
    /// 表示順のフレーム番号（含まない、半開区間）
    pub end: u32,
    /// 候補とした理由。候補には必ず1つ以上入る。
    pub reasons: Vec<OvercutReason>,
}

/// 切りすぎ候補とした理由。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OvercutReason {
    /// カット区間内のロゴ表示が [`LOGO_MIN_SEC`] 秒以上。
    Logo,
    /// カット区間と重なる `:CM` エントリの15秒格子誤差が
    /// [`CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES`] を超える。
    Grid,
}

/// `[0, total_frames)` のうち保持区間に含まれない部分を、昇順の半開区間で返す。
///
/// 保持区間の終端が `total_frames` を超えていても、区間は `total_frames` で切る。
fn cut_ranges(trim: &TrimList, total_frames: u32) -> Vec<(u32, u32)> {
    let mut gaps = Vec::new();
    let mut cursor = 0u32;
    for range in trim.ranges() {
        let start = range.start().0.min(total_frames);
        if cursor < start {
            gaps.push((cursor, start));
        }
        cursor = cursor.max(range.end().0);
    }
    if cursor < total_frames {
        gaps.push((cursor, total_frames));
    }
    gaps
}

/// 2つの半開区間の重なりフレーム数（重ならなければ 0）。
fn overlap_frames(a: (u32, u32), b: (u32, u32)) -> u32 {
    a.1.min(b.1).saturating_sub(a.0.max(b.0))
}

/// カット区間 `gap` 内のロゴ表示フレーム数が [`LOGO_MIN_SEC`] 秒以上か。
fn has_logo(gap: (u32, u32), logo: &[(u32, u32)], fps: f64) -> bool {
    let frames: u64 = logo
        .iter()
        .map(|&l| u64::from(overlap_frames(gap, l)))
        .sum();
    frames as f64 >= LOGO_MIN_SEC * fps
}

/// カット区間 `gap` と重なる `:CM` エントリのうち、15秒格子誤差が閾値を超えるものが
/// 1つでもあるか。
///
/// `JlsEntry` の `end` は両端を含むので、半開区間 `[start, end + 1)` にして比べる。
fn has_off_grid_cm(gap: (u32, u32), jls_entries: &[JlsEntry]) -> bool {
    jls_entries.iter().any(|e| {
        e.is_cm()
            && i64::from(e.error_frames).abs() > i64::from(CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES)
            && overlap_frames(gap, (e.start, e.end.saturating_add(1))) > 0
    })
}

/// 切りすぎ候補を探す。
///
/// - `trim`: 保持区間（[`TrimList`]）
/// - `jls_entries`: detail.jls の全エントリ
/// - `logo`: ロゴ表示区間（半開区間）。ロゴ検出が無ければ `None`。`None` のとき
///   [`OvercutReason::Logo`] は付かず、[`OvercutReason::Grid`] だけで判定する
/// - `total_frames`: 表示順の総フレーム数
/// - `fps`: [`LOGO_MIN_SEC`] を秒からフレーム数に換算するためのフレームレート
///
/// 理由が1つも付かないカット区間は返さない。結果は開始位置の昇順。
/// 全編を保持する `trim` なら空の列を返す（カットされる区間が無い）。
pub fn find_overcut_candidates(
    trim: &TrimList,
    jls_entries: &[JlsEntry],
    logo: Option<&[(u32, u32)]>,
    total_frames: u32,
    fps: f64,
) -> Vec<OvercutCandidate> {
    cut_ranges(trim, total_frames)
        .into_iter()
        .filter_map(|gap| {
            let mut reasons = Vec::new();
            if logo.is_some_and(|l| has_logo(gap, l, fps)) {
                reasons.push(OvercutReason::Logo);
            }
            if has_off_grid_cm(gap, jls_entries) {
                reasons.push(OvercutReason::Grid);
            }
            (!reasons.is_empty()).then_some(OvercutCandidate {
                start: gap.0,
                end: gap.1,
                reasons,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(start: u32, end: u32, error_frames: i32, label: &str) -> JlsEntry {
        JlsEntry {
            start,
            end,
            duration_sec: 0,
            error_frames,
            logo_sec: 0,
            label: label.to_string(),
        }
    }

    #[test]
    fn full_keep_trim_gives_no_candidates() {
        let trim = TrimList::parse("Trim(0,999)").expect("should parse");
        let jls = vec![entry(0, 999, 9, ":CM")];
        let logo = [(0, 1000)];

        let got = find_overcut_candidates(&trim, &jls, Some(&logo), 1000, 10.0);

        assert!(got.is_empty());
    }

    #[test]
    fn logo_of_at_least_five_seconds_in_cut_gives_logo_reason() {
        // fps=10 なので5秒は50フレーム。カット区間 [100,400) に [100,150) = 50フレーム。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let logo = [(100, 150)];

        let got = find_overcut_candidates(&trim, &[], Some(&logo), 1000, 10.0);

        assert_eq!(
            got,
            vec![OvercutCandidate {
                start: 100,
                end: 400,
                reasons: vec![OvercutReason::Logo],
            }]
        );
    }

    #[test]
    fn logo_shorter_than_five_seconds_in_cut_gives_no_logo_reason() {
        // 49フレーム（4.9秒）は足りない。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let logo = [(100, 149)];

        let got = find_overcut_candidates(&trim, &[], Some(&logo), 1000, 10.0);

        assert!(got.is_empty());
    }

    #[test]
    fn logo_inside_kept_range_is_not_counted() {
        // ロゴは保持区間 [0,100) の中だけにある。カット区間 [100,400) には無い。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let logo = [(0, 100)];

        let got = find_overcut_candidates(&trim, &[], Some(&logo), 1000, 10.0);

        assert!(got.is_empty());
    }

    #[test]
    fn logo_none_gives_only_grid_reason() {
        // ロゴ区間は None。カット区間 [100,400) は :CM 誤差 6 で Grid だけが付く。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let jls = vec![entry(100, 399, 6, ":CM")];

        let got = find_overcut_candidates(&trim, &jls, None, 1000, 10.0);

        assert_eq!(
            got,
            vec![OvercutCandidate {
                start: 100,
                end: 400,
                reasons: vec![OvercutReason::Grid],
            }]
        );
    }

    #[test]
    fn grid_error_above_threshold_gives_grid_reason() {
        // 閾値は |誤差| > 5。6 は超え、-6 も絶対値で超える。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");

        for err in [6, -6] {
            let jls = vec![entry(100, 399, err, ":CM")];
            let got = find_overcut_candidates(&trim, &jls, None, 1000, 10.0);
            assert_eq!(got.len(), 1, "誤差 {err} は閾値を超えるので候補のはず");
            assert_eq!(got[0].reasons, vec![OvercutReason::Grid]);
        }
    }

    #[test]
    fn grid_error_at_threshold_gives_no_grid_reason() {
        // 閾値ちょうど（5）は超えていないので付かない。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let jls = vec![entry(100, 399, 5, ":CM")];

        let got = find_overcut_candidates(&trim, &jls, None, 1000, 10.0);

        assert!(got.is_empty());
    }

    #[test]
    fn off_grid_entry_that_is_not_cm_gives_no_grid_reason() {
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let jls = vec![entry(100, 399, 14, ":L")];

        let got = find_overcut_candidates(&trim, &jls, None, 1000, 10.0);

        assert!(got.is_empty());
    }

    #[test]
    fn grid_entry_touching_cut_gap_only_at_boundary_is_not_overlapping() {
        // カット区間は [100,400)。:CM [..,99] は隣接するだけで重ならない。
        // :CM [400,..] も半開区間の外。どちらも重なりなしで付かない。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let jls = vec![entry(50, 99, 14, ":CM"), entry(400, 450, 14, ":CM")];

        let got = find_overcut_candidates(&trim, &jls, None, 1000, 10.0);

        assert!(got.is_empty());
    }

    #[test]
    fn grid_entry_sharing_one_frame_with_cut_gap_is_overlapping() {
        // :CM [399,..] は、カット区間 [100,400) の最後のフレーム 399 と重なる。
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let jls = vec![entry(399, 450, 14, ":CM")];

        let got = find_overcut_candidates(&trim, &jls, None, 1000, 10.0);

        assert_eq!(got.len(), 1);
        assert_eq!(got[0].reasons, vec![OvercutReason::Grid]);
    }

    #[test]
    fn leading_and_trailing_cut_gaps_are_candidates() {
        // 先頭 [0,50) と末尾 [900,1000) がカットされている。
        // 先頭にロゴ、末尾に格子外れの :CM を置いて、両方が候補になることを見る。
        let trim = TrimList::parse("Trim(50,899)").expect("should parse");
        let logo = [(0, 100)];
        let jls = vec![entry(900, 999, 9, ":CM")];

        let got = find_overcut_candidates(&trim, &jls, Some(&logo), 1000, 10.0);

        assert_eq!(
            got,
            vec![
                OvercutCandidate {
                    start: 0,
                    end: 50,
                    reasons: vec![OvercutReason::Logo],
                },
                OvercutCandidate {
                    start: 900,
                    end: 1000,
                    reasons: vec![OvercutReason::Grid],
                },
            ]
        );
    }

    #[test]
    fn both_reasons_are_reported_in_order() {
        let trim = TrimList::parse("Trim(0,99) ++ Trim(400,999)").expect("should parse");
        let logo = [(100, 200)];
        let jls = vec![entry(100, 399, 9, ":CM")];

        let got = find_overcut_candidates(&trim, &jls, Some(&logo), 1000, 10.0);

        assert_eq!(
            got[0].reasons,
            vec![OvercutReason::Logo, OvercutReason::Grid]
        );
    }
}
