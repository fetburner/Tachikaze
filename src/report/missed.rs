// CLI (--report) からの配線待ち。配線されたら外す。
#![allow(dead_code)]

//! CM 検出の見逃し候補を警告するヒューリスティック。
//!
//! 検出済み CM ブロック（Trim の保持区間と保持区間の間のギャップ）の長さを集め、
//! 同じ長さ帯が複数見つかった場合、それを「既知の CM 長」とみなす。
//! 保持区間の内部にある `.jls` のエントリのうち、既知の CM 長と一致する長さを
//! 持つものを「見逃し候補」として報告する。
//!
//! `.dtvi` には依存しない設計にするため、fps は呼び出し側から渡してもらう。

use crate::jls::JlsEntry;
use crate::trim::TrimList;

/// 長さ一致の判定に使う許容幅（フレーム数）。実測のばらつきは1〜2フレーム程度。
const TOLERANCE_FRAMES: u32 = 3;

/// 「既知の CM 長」として採用するために必要な最小出現数。
const MIN_OCCURRENCES: usize = 2;

/// 見逃し candidate（検出漏れの疑いがある未カット区間）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissedCandidate {
    /// `.jls` 由来の開始フレーム番号（両端含む）
    pub start: u32,
    /// `.jls` 由来の終了フレーム番号（両端含む）
    pub end: u32,
    /// 区間の長さ（フレーム数）
    pub length_frames: u32,
    /// 一致した CM ギャップの長さ（参考情報）
    pub matched_gap_length: u32,
}

/// `TrimList` の連続する保持区間の間のギャップ長（= 検出済み CM ブロックの長さ）を列挙する。
fn detected_gap_lengths(trim: &TrimList) -> Vec<u32> {
    trim.ranges()
        .windows(2)
        .map(|w| w[1].start().0 - w[0].end().0)
        .collect()
}

/// ギャップ長の集合から、許容幅 ±[TOLERANCE_FRAMES] 以内で
/// [MIN_OCCURRENCES] 個以上まとまっている長さ帯だけを「既知の CM 長」として返す。
///
/// 戻り値はまとまりの代表長（グループ内の最初の値）のリスト。
fn known_cm_lengths(gap_lengths: &[u32]) -> Vec<u32> {
    let mut sorted = gap_lengths.to_vec();
    sorted.sort_unstable();

    let mut groups: Vec<Vec<u32>> = Vec::new();
    for len in sorted {
        if let Some(group) = groups.last_mut() {
            if len - group[0] <= TOLERANCE_FRAMES {
                group.push(len);
                continue;
            }
        }
        groups.push(vec![len]);
    }

    groups
        .into_iter()
        .filter(|g| g.len() >= MIN_OCCURRENCES)
        .map(|g| g[0])
        .collect()
}

/// `length` が `known` のいずれかと許容幅 ±[TOLERANCE_FRAMES] 以内で一致するなら、
/// その `known` の値を返す。
fn matching_known_length(length: u32, known: &[u32]) -> Option<u32> {
    known
        .iter()
        .copied()
        .find(|&k| length.abs_diff(k) <= TOLERANCE_FRAMES)
}

/// CM 検出の見逃し候補を探す。
///
/// - `trim`: join_logo_scp が生成した Trim リスト（保持区間の列）
/// - `jls_entries`: detail.jls の全エントリ
///
/// 保持区間に完全に含まれる `.jls` エントリのうち、検出済み CM ギャップと
/// 同じ長さ帯（かつそのギャップ長が2個以上検出されている）を持つものを返す。
pub fn find_missed_candidates(trim: &TrimList, jls_entries: &[JlsEntry]) -> Vec<MissedCandidate> {
    let known = known_cm_lengths(&detected_gap_lengths(trim));
    if known.is_empty() {
        return Vec::new();
    }

    let mut candidates = Vec::new();
    for range in trim.ranges() {
        for entry in jls_entries {
            // 保持区間（半開）に完全に含まれるか。`.jls` は両端含む区間。
            if entry.start >= range.start().0 && entry.end < range.end().0 {
                let length_frames = entry.end - entry.start + 1;
                if let Some(matched_gap_length) = matching_known_length(length_frames, &known) {
                    candidates.push(MissedCandidate {
                        start: entry.start,
                        end: entry.end,
                        length_frames,
                        matched_gap_length,
                    });
                }
            }
        }
    }

    candidates
}

/// フレーム番号を `fps` で秒に変換し、`mm:ss` 形式にする。
fn format_timestamp(frame: u32, fps: f64) -> String {
    let total_sec = (frame as f64 / fps).floor() as u64;
    let minutes = total_sec / 60;
    let seconds = total_sec % 60;
    format!("{minutes}:{seconds:02}")
}

/// 見逃し候補の警告メッセージを整形する。
///
/// `trim.avs` をどう直せばよいか（該当区間を分割する Trim の例）も添える。
pub fn format_warning(candidate: &MissedCandidate, fps: f64) -> String {
    let start_ts = format_timestamp(candidate.start, fps);
    let end_ts = format_timestamp(candidate.end, fps);
    format!(
        "CM 検出の見逃し候補: {start_ts}〜{end_ts} ({start}〜{end}, {len} フレーム, \
         検出済み CM ブロック長 {gap} フレームと一致)。\
         trim.avs でこの区間を保持区間から除外する場合、\
         該当する Trim(a,{start_minus_1}) ++ Trim({end_plus_1},b) のように \
         分割することを検討してください。",
        start_ts = start_ts,
        end_ts = end_ts,
        start = candidate.start,
        end = candidate.end,
        len = candidate.length_frames,
        gap = candidate.matched_gap_length,
        start_minus_1 = candidate.start.saturating_sub(1),
        end_plus_1 = candidate.end + 1,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(start: u32, end: u32, label: &str) -> JlsEntry {
        JlsEntry {
            start,
            end,
            duration_sec: 0,
            error_frames: 0,
            logo_sec: 0,
            label: label.to_string(),
        }
    }

    /// ファイル C の実測値を使ったテスト（`docs/jls-settings.md` の「既知の失敗モード」節）。
    ///
    /// 検出済み CM ブロック（Trim のギャップ）は3つ:
    ///   53592→57189 (3597フレーム), 70975→74571 (3596フレーム), 86859→90455 (3596フレーム)
    /// 同じ長さ帯（許容幅 ±3）で3個検出されているので「既知の CM 長」となる。
    /// 最初の保持区間 [0, 53592) の内部に、同じ長さ帯を持つ 34202→37798（3597フレーム）が
    /// カットされずに残っている（= 見逃し候補）ので、これが検出されるはず。
    #[test]
    fn detects_missed_block_matching_known_cm_length() {
        let trim_input = "Trim(0,53591) ++ Trim(57189,70974) ++ Trim(74571,86858) \
             ++ Trim(90455,99999)";
        let trim = TrimList::parse(trim_input).expect("should parse");

        let jls_entries = vec![
            entry(0, 34201, ":L"),
            entry(34202, 37798, ":L"), // 見逃し候補（本来は :CM のはずが検出漏れ）
            entry(37799, 53591, ":L"),
            entry(53592, 57188, ":CM"), // 検出済み（Trim のギャップに現れる）
            entry(57189, 70974, ":L"),
            entry(70975, 74570, ":CM"), // 検出済み
            entry(74571, 86858, ":L"),
            entry(86859, 90454, ":CM"), // 検出済み
            entry(90455, 99999, ":L"),
        ];

        let candidates = find_missed_candidates(&trim, &jls_entries);

        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].start, 34202);
        assert_eq!(candidates[0].end, 37798);
        assert_eq!(candidates[0].length_frames, 3597);
        assert!(
            candidates[0].matched_gap_length == 3596 || candidates[0].matched_gap_length == 3597
        );
    }

    #[test]
    fn no_warning_when_only_one_gap_of_a_given_length_is_detected() {
        // ギャップが1個しか検出されていない場合は「既知の CM 長」とみなさず、発火しない。
        let trim_input = "Trim(0,34201) ++ Trim(37798,53591)";
        let trim = TrimList::parse(trim_input).expect("should parse");

        let jls_entries = vec![entry(0, 34201, ":L"), entry(37798, 53591, ":L")];

        let candidates = find_missed_candidates(&trim, &jls_entries);
        assert!(candidates.is_empty());
    }

    #[test]
    fn no_warning_when_cm_block_lengths_are_not_aligned() {
        // 検出済み CM ブロックの長さがバラバラ（許容幅を超えて異なる）場合、
        // 「既知の CM 長」が形成されないので何も警告しない。
        let trim_input = "Trim(0,1000) ++ Trim(1500,3000) ++ Trim(4000,6000) ++ Trim(9000,10000)";
        let trim = TrimList::parse(trim_input).expect("should parse");
        // ギャップ: 1001→1500 (499), 3001→4000 (999), 6001→9000 (2999) -- すべて長さが異なる

        let jls_entries = vec![
            entry(0, 1000, ":L"),
            entry(1001, 1499, ":CM"),
            entry(1500, 3000, ":L"),
            entry(3001, 3999, ":CM"),
            entry(4000, 6000, ":L"),
            entry(6001, 6600, ":L"), // 保持区間内部にあるが、既知の CM 長が存在しないので無視される
            entry(6601, 9000, ":L"),
            entry(9000, 10000, ":L"),
        ];

        let candidates = find_missed_candidates(&trim, &jls_entries);
        assert!(candidates.is_empty());
    }

    #[test]
    fn format_warning_does_not_panic_and_includes_key_numbers() {
        let candidate = MissedCandidate {
            start: 34202,
            end: 37798,
            length_frames: 3597,
            matched_gap_length: 3596,
        };
        let message = format_warning(&candidate, 29.97);

        assert!(message.contains("34202"));
        assert!(message.contains("37798"));
        assert!(message.contains("3597"));
        assert!(message.contains("19:0")); // 34202 / 29.97 ≈ 1141秒 ≈ 19:01付近
    }
}
