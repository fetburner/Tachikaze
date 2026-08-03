//! `analyze` の成果物だけから「検出結果が機械的に疑わしいか」を判定する gate。
//!
//! `auto`（#62、実装済み、`src/auto.rs`）が人手を安全に外せるのは「機械的に疑わしい
//! ときは止まる」からで、その判定をここに切り出しておく。`auto` は「人手を外す」
//! 機能ではなく「人手を呼ぶ条件を機械化する」機能であり、ここが本体になる。
//!
//! **判定材料は `analyze` の成果物（`TrimList` / `JlsEntry` の列 / [`Dtvi`]）だけに
//! 限る。mp4 は一切読まない**（`Moov` を引数に取らない）。読むと解析側が mp4 の
//! 読み込みに依存してしまい、`docs/architecture.md`「モジュール構成」の
//! 「解析側（analyze）は mp4 の読み込みに依存しない」という性質を崩す。
//! 総フレーム数は `.dtvi` の `frames.len()` から取る。
//!
//! `auto` を使わない人間も同じ情報を見られるよう、`analyze --report` の末尾に
//! [`format_gate_report`] の内容を表示する（[`crate::commands`] の配線）。
//!
//! ## 指標と扱い
//!
//! | 指標 | 扱い |
//! |---|---|
//! | 見逃し候補（[`crate::report::missed`]）が1件以上 | 止める |
//! | カット区間が0（除去フレーム数の合計が0） | 止める |
//! | 保持率（保持フレーム数 / 総フレーム数） | 値のみ |
//! | 15秒格子から外れた CM ブロックの件数 | 値のみ |
//! | CM ブロックの `detail.jls` 誤差フレーム数（4列目）の絶対値の最大値 | 値のみ |
//!
//! 「止める」に昇格させたのは、実害が実測されている見逃し候補（#24、
//! `docs/jls-settings.md`「既知の失敗モード」）と、CM が1つも検出されていない
//! （＝解析が実質何もしていない）ことが明らかな2つだけ。保持率や格子誤差は
//! まだ閾値の実測的裏付けがない。根拠のない閾値で止めると `--ignore-gate`
//! （`auto` に実装済み、`src/auto.rs::AutoConfig::ignore_gate`）を常用させることになり、
//! gate 自体が無意味になる（CLAUDE.md「静かに壊れる罠」と同種の失敗）。まず値を
//! 出すだけにして、実運用で分布を見てから「止める」判定に昇格させる方針にしている。
//!
//! 指標は合成スコアにせず、常にすべて独立した値として出す。合成すると、
//! 「止まった／止まらなかった」ときにどの指標が原因かが分からなくなる。
//!
//! ## 見逃し候補ヒューリスティックの限界
//!
//! [`crate::report::missed::find_missed_candidates`] は「検出済み CM ブロック長が
//! 複数回同じ長さ帯で揃っている」ことを前提にしたヒューリスティックで、
//! ブロック長が番組ごとに揃わない場合は効かない（`docs/jls-settings.md`
//! 「既知の失敗モード: 内部区切りのない CM ブロックの見逃し」に明記）。
//! そのため、**この gate が「止めない」と判定しても「検出が当たっている」保証には
//! ならない。** 見逃し候補が0件なのは「揃った長さ帯との一致が見つからなかった」
//! だけであり、他の形の見逃しを検出できているわけではない。

use crate::dtvi::Dtvi;
use crate::jls::JlsEntry;
use crate::report::missed::{self, MissedCandidate};
use crate::trim::TrimList;

/// CM ブロックの `detail.jls` 誤差フレーム数（4列目）の絶対値がこれを超えたら
/// 「15秒格子から外れている」と数える。
///
/// **未検証。** `docs/jls-settings.md`「`detail.jls` の読み方」に載っている実測は
/// 誤差0（確度が高いと書かれた実例）と誤差14（怪しいと書かれた実例）の2点だけで、
/// 閾値を決めるための分布はまだ見ていない。そのため値を数えるだけに留め、
/// 「止める」判定には使わない。実運用でこの件数の分布を見てから調整すること。
const CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES: i32 = 5;

/// gate の判定結果。全指標の値と、そのうち「止める」判定に使ったものを持つ。
///
/// 止めない場合でも全フィールドに実際の値が入る。何を見て通したかが残らないと、
/// 後で見逃しが発覚したときに閾値の妥当性を検証できない
/// （CLAUDE.md「その測定で何を見ていなかったかを併記する」と同じ理由）。
#[derive(Debug, Clone, PartialEq)]
pub struct GateVerdict {
    /// 見逃し候補の一覧（[`crate::report::missed::find_missed_candidates`] の戻り値そのもの）。
    pub missed_candidates: Vec<MissedCandidate>,
    /// 保持区間の数。
    pub kept_ranges: usize,
    /// `.dtvi` から取った総フレーム数。
    pub total_frames: u32,
    /// 保持区間の長さの合計（フレーム数）。
    pub kept_frames_total: u32,
    /// 除去フレーム数の合計（`total_frames - kept_frames_total`）。
    pub cut_frames_total: u32,
    /// 保持率（`kept_frames_total / total_frames`）。総フレーム数が0なら0.0。
    pub kept_ratio: f64,
    /// 15秒格子から [`CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES`] を超えて外れた
    /// CM ブロックの件数。
    pub cm_blocks_off_grid_count: usize,
    /// CM ブロックの `detail.jls` 誤差フレーム数（4列目）の絶対値の最大値。
    /// `.jls` に `:CM` エントリが1つもなければ `None`。
    pub max_cm_block_grid_error_frames: Option<u32>,
    /// 止めるかどうか（`!missed_candidates.is_empty() || cut_frames_total == 0`）。
    pub stop: bool,
}

impl GateVerdict {
    /// 見逃し候補が1件以上あるために止めるかどうか。
    pub fn stops_for_missed_candidates(&self) -> bool {
        !self.missed_candidates.is_empty()
    }

    /// カット区間が0（除去フレーム数の合計が0）のために止めるかどうか。
    pub fn stops_for_no_cut(&self) -> bool {
        self.cut_frames_total == 0
    }
}

/// `trim` の保持区間の長さの合計（フレーム数）を求める。
fn kept_frames_total(trim: &TrimList) -> u32 {
    trim.ranges().iter().map(|r| r.end().0 - r.start().0).sum()
}

/// `.jls` の `:CM` エントリの誤差フレーム数（4列目）一覧を返す。
fn cm_block_grid_errors(jls_entries: &[JlsEntry]) -> Vec<i32> {
    jls_entries
        .iter()
        .filter(|e| e.is_cm())
        .map(|e| e.error_frames)
        .collect()
}

/// `analyze` の成果物から gate の判定を求める。
///
/// mp4 は読まない。総フレーム数は `dtvi.frames.len()` から取る
/// （`.dtvi` ヘッダの `frame_count` ではなくフレーム表の実際の長さを使うことで、
/// ヘッダとフレーム表が食い違っていても実データに合わせられる）。
pub fn evaluate(trim: &TrimList, jls_entries: &[JlsEntry], dtvi: &Dtvi) -> GateVerdict {
    let missed_candidates = missed::find_missed_candidates(trim, jls_entries);

    let total_frames = dtvi.frames.len() as u32;
    let kept_frames_total = kept_frames_total(trim);
    let cut_frames_total = total_frames.saturating_sub(kept_frames_total);
    let kept_ratio = if total_frames == 0 {
        0.0
    } else {
        kept_frames_total as f64 / total_frames as f64
    };

    let cm_errors = cm_block_grid_errors(jls_entries);
    let cm_blocks_off_grid_count = cm_errors
        .iter()
        .filter(|&&e| e.abs() > CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES)
        .count();
    let max_cm_block_grid_error_frames = cm_errors.iter().map(|e| e.unsigned_abs()).max();

    let stop = !missed_candidates.is_empty() || cut_frames_total == 0;

    GateVerdict {
        missed_candidates,
        kept_ranges: trim.ranges().len(),
        total_frames,
        kept_frames_total,
        cut_frames_total,
        kept_ratio,
        cm_blocks_off_grid_count,
        max_cm_block_grid_error_frames,
        stop,
    }
}

/// `--report` 末尾に表示する判定内訳のプレーンテキストを組み立てる。
///
/// [`crate::report::format_report`] と同様、標準出力への書き込みは一切行わない。
/// 呼び出し側（`analyze --report` の配線）がこの戻り値を表示する。
pub fn format_gate_report(verdict: &GateVerdict) -> String {
    let mut out = String::new();

    out.push_str("gate 判定:\n");
    out.push_str(&format!(
        "  見逃し候補: {} 件{}\n",
        verdict.missed_candidates.len(),
        if verdict.stops_for_missed_candidates() {
            " → 止める"
        } else {
            ""
        }
    ));
    out.push_str(&format!(
        "  保持区間数 {} / 除去フレーム数 {}（総 {} フレーム中）{}\n",
        verdict.kept_ranges,
        verdict.cut_frames_total,
        verdict.total_frames,
        if verdict.stops_for_no_cut() {
            " → 止める"
        } else {
            ""
        }
    ));
    out.push_str(&format!(
        "  保持率 {:.1}%（閾値なし、参考値）\n",
        verdict.kept_ratio * 100.0
    ));
    out.push_str(&format!(
        "  15秒格子から外れた CM ブロック数 {}（|誤差| > {} フレーム、閾値なし、参考値）\n",
        verdict.cm_blocks_off_grid_count, CM_BLOCK_GRID_ERROR_THRESHOLD_FRAMES
    ));
    match verdict.max_cm_block_grid_error_frames {
        Some(max) => out.push_str(&format!(
            "  CM ブロックの格子誤差フレーム数の最大値 {max}（閾値なし、参考値）\n"
        )),
        None => out.push_str("  CM ブロックの格子誤差フレーム数の最大値 -（CM ブロックなし）\n"),
    }
    out.push_str(&format!(
        "  総合判定: {}\n",
        if verdict.stop {
            "疑わしいので止める"
        } else {
            "止めない（見逃し候補ヒューリスティックが効かない番組もあるため、\
             「検出が当たっている」保証ではない）"
        }
    ));

    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::{DecodeIdx, DisplayIdx};
    use std::collections::HashMap;

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

    /// `total_frames` だけの長さを持つ最小限の `Dtvi`（フレームの中身は使わないので
    /// フィールドは0埋めでよい）。
    fn make_dtvi_with_frame_count(total_frames: u32) -> Dtvi {
        let frames = (0..total_frames)
            .map(|n| crate::dtvi::DtviFrame {
                frame_number: DisplayIdx(n),
                sample_number: DecodeIdx(n),
                random_access_sample: DecodeIdx(0),
                file_offset: 0,
                pts: 0,
                dts: 0,
                duration: 0,
                flags: 0,
            })
            .collect();
        Dtvi {
            format_version: 1,
            header: HashMap::new(),
            frames,
        }
    }

    /// `docs/jls-settings.md`「既知の失敗モード」＝ファイル C の実測値
    /// （`src/report/missed.rs` のテストと同じデータ）。
    fn file_c_trim_and_jls() -> (TrimList, Vec<JlsEntry>) {
        let trim_input = "Trim(0,53591) ++ Trim(57189,70974) ++ Trim(74571,86858) \
             ++ Trim(90455,99999)";
        let trim = TrimList::parse(trim_input).expect("should parse");

        let jls_entries = vec![
            entry(0, 34201, 0, ":L"),
            entry(34202, 37798, 0, ":L"), // 見逃し候補（本来は :CM のはずが検出漏れ）
            entry(37799, 53591, 0, ":L"),
            entry(53592, 57188, 0, ":CM"),
            entry(57189, 70974, 0, ":L"),
            entry(70975, 74570, 0, ":CM"),
            entry(74571, 86858, 0, ":L"),
            entry(86859, 90454, 0, ":CM"),
            entry(90455, 99999, 0, ":L"),
        ];
        (trim, jls_entries)
    }

    #[test]
    fn stops_when_missed_candidate_exists() {
        let (trim, jls_entries) = file_c_trim_and_jls();
        let dtvi = make_dtvi_with_frame_count(100_000);

        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        assert_eq!(verdict.missed_candidates.len(), 1);
        assert!(verdict.stops_for_missed_candidates());
        assert!(verdict.stop);
    }

    #[test]
    fn stops_when_no_cut_happened() {
        // 保持区間が1つだけ、かつ総フレーム数と完全一致 → 除去フレーム数0。
        let trim = TrimList::parse("Trim(0,9999)").expect("should parse");
        let jls_entries = vec![entry(0, 9999, 0, ":L")];
        let dtvi = make_dtvi_with_frame_count(10_000);

        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        assert_eq!(verdict.cut_frames_total, 0);
        assert!(verdict.stops_for_no_cut());
        assert!(verdict.stop);
        // 見逃し候補ヒューリスティックは、そもそも既知の CM 長が1つもないので発火しない。
        assert!(!verdict.stops_for_missed_candidates());
    }

    #[test]
    fn does_not_stop_but_reports_all_indicators() {
        // 検出済み CM ブロックが1種類の長さで複数回揃っていない（見逃し候補は出ない）、
        // かつ除去フレーム数 > 0 の、疑わしくない入力。
        let trim = TrimList::parse("Trim(0,999) ++ Trim(1500,2999)").expect("should parse");
        let jls_entries = vec![
            entry(0, 999, 0, ":L"),
            entry(1000, 1499, 2, ":CM"), // 格子誤差2（閾値5以下）
            entry(1500, 2999, 0, ":L"),
        ];
        let dtvi = make_dtvi_with_frame_count(3000);

        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        assert!(!verdict.stop);
        assert!(!verdict.stops_for_missed_candidates());
        assert!(!verdict.stops_for_no_cut());

        // 止めない場合でも指標の値はすべて出る。
        assert_eq!(verdict.kept_ranges, 2);
        assert_eq!(verdict.total_frames, 3000);
        assert_eq!(verdict.kept_frames_total, 1000 + 1500);
        assert_eq!(verdict.cut_frames_total, 500);
        assert!((verdict.kept_ratio - (2500.0 / 3000.0)).abs() < 1e-9);
        assert_eq!(verdict.cm_blocks_off_grid_count, 0);
        assert_eq!(verdict.max_cm_block_grid_error_frames, Some(2));
    }

    #[test]
    fn counts_cm_blocks_off_grid_using_threshold() {
        let trim = TrimList::parse("Trim(0,999) ++ Trim(1500,2999) ++ Trim(4000,4999)")
            .expect("should parse");
        let jls_entries = vec![
            entry(0, 999, 0, ":L"),
            entry(1000, 1499, 2, ":CM"), // 閾値(5)以下 → 外れていない
            entry(1500, 2999, 0, ":L"),
            entry(3000, 3999, 14, ":CM"), // 閾値超え → 外れている
            entry(4000, 4999, 0, ":L"),
        ];
        let dtvi = make_dtvi_with_frame_count(5000);

        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        assert_eq!(verdict.cm_blocks_off_grid_count, 1);
        assert_eq!(verdict.max_cm_block_grid_error_frames, Some(14));
    }

    #[test]
    fn max_cm_block_grid_error_is_none_without_cm_entries() {
        let trim = TrimList::parse("Trim(0,999) ++ Trim(1500,2999)").expect("should parse");
        let jls_entries = vec![entry(0, 999, 0, ":L"), entry(1500, 2999, 0, ":L")];
        let dtvi = make_dtvi_with_frame_count(3000);

        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        assert_eq!(verdict.max_cm_block_grid_error_frames, None);
    }

    #[test]
    fn format_gate_report_mentions_stop_reason() {
        let (trim, jls_entries) = file_c_trim_and_jls();
        let dtvi = make_dtvi_with_frame_count(100_000);
        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        let report = format_gate_report(&verdict);
        assert!(report.contains("見逃し候補: 1 件"));
        assert!(report.contains("止める"));
    }

    #[test]
    fn format_gate_report_does_not_panic_when_not_stopping() {
        let trim = TrimList::parse("Trim(0,999) ++ Trim(1500,2999)").expect("should parse");
        let jls_entries = vec![entry(0, 999, 0, ":L"), entry(1500, 2999, 0, ":L")];
        let dtvi = make_dtvi_with_frame_count(3000);
        let verdict = evaluate(&trim, &jls_entries, &dtvi);

        let report = format_gate_report(&verdict);
        assert!(report.contains("止めない"));
    }
}
