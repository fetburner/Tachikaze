//! join_logo_scp の `-oscp` 出力（detail.jls）のパーサ。
//!
//! 各行は次の6列を空白区切りで持つ（1行目はヘッダで読み飛ばす）:
//!
//! 1. 単位フレーム開始位置（表示順）
//! 2. 単位フレーム終了位置
//! 3. 期間（秒数）
//! 4. 期間秒数からの誤差（フレーム数）。負の値も出る
//! 5. 期間内のロゴ表示期間（秒数）
//! 6. 推測した構成（ラベル。`:CM` / `:L` / `:Nologo` / `:Trailer(add)` など）
//!
//! ラベルは種類が多く JLファイル次第で増えるため列挙型にせず `String` で保持する。
//! このパーサは analyze --report の情報表示・見逃し警告にのみ使う。
//! カット位置の決定には使わない（それは Trim が担当）。

use std::fmt;

/// detail.jls の1行分のエントリ。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JlsEntry {
    /// 単位フレーム開始位置（表示順）
    pub start: u32,
    /// 単位フレーム終了位置
    pub end: u32,
    /// 期間（秒数）
    pub duration_sec: i32,
    /// 期間秒数からの誤差（フレーム数）。負の値も出る
    pub error_frames: i32,
    /// 期間内のロゴ表示期間（秒数）
    pub logo_sec: i32,
    /// 推測した構成（ラベル）。未知のラベルも許容するため文字列で保持する
    pub label: String,
}

impl JlsEntry {
    /// ラベルが `:CM` かどうか。
    pub fn is_cm(&self) -> bool {
        self.label == ":CM"
    }

    // 現状 --report の出力はCMブロックの一覧のみで、キャンセルされた番宣の
    // 判別はまだ使っていない（将来 --report を拡張する際の材料として残す）。
    #[allow(dead_code)]
    /// ラベルが `(cut-cancel)` を含むかどうか。
    /// 設定でキャンセルされた番宣（例: `:Trailer(cut-cancel)`）を判別するために使う。
    pub fn is_trailer_cut_cancel(&self) -> bool {
        self.label.contains("(cut-cancel)")
    }
}

/// detail.jls の1行のパースに失敗したことを表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JlsParseError {
    /// 元ファイル中の行番号（1始まり）
    pub line_no: usize,
    /// パースに失敗した行の内容
    pub line: String,
}

impl fmt::Display for JlsParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "detail.jls の {} 行目をパースできません: {:?}",
            self.line_no, self.line
        )
    }
}

impl std::error::Error for JlsParseError {}

/// detail.jls の内容全体をパースする。1行目のヘッダは読み飛ばす。
///
/// 未知のラベルはエラーにしない（文字列としてそのまま保持する）。
/// 列数が6に満たない行や、数値列が数値として解釈できない行はエラーにする。
pub fn parse(input: &str) -> Result<Vec<JlsEntry>, JlsParseError> {
    let mut entries = Vec::new();

    for (idx, line) in input.lines().enumerate() {
        let line_no = idx + 1;

        // ヘッダ行を読み飛ばす
        if line_no == 1 {
            continue;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let fields: Vec<&str> = trimmed.split_whitespace().collect();
        if fields.len() != 6 {
            return Err(JlsParseError {
                line_no,
                line: line.to_string(),
            });
        }

        let parse_field = |s: &str| s.parse::<i64>().ok();

        let start = parse_field(fields[0]).and_then(|v| u32::try_from(v).ok());
        let end = parse_field(fields[1]).and_then(|v| u32::try_from(v).ok());
        let duration_sec = parse_field(fields[2]).and_then(|v| i32::try_from(v).ok());
        let error_frames = parse_field(fields[3]).and_then(|v| i32::try_from(v).ok());
        let logo_sec = parse_field(fields[4]).and_then(|v| i32::try_from(v).ok());

        let (Some(start), Some(end), Some(duration_sec), Some(error_frames), Some(logo_sec)) =
            (start, end, duration_sec, error_frames, logo_sec)
        else {
            return Err(JlsParseError {
                line_no,
                line: line.to_string(),
            });
        };

        entries.push(JlsEntry {
            start,
            end,
            duration_sec,
            error_frames,
            logo_sec,
            label: fields[5].to_string(),
        });
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "\
開始   終了  秒数 誤差 ロゴ秒 ラベル
   0    73    2   14    0 :Nologo
  74  6127   15    0   15 :L
6128  6577   15    0    0 :CM
";

    #[test]
    fn parses_the_three_sample_lines() {
        let entries = parse(SAMPLE).expect("パースに失敗した");

        assert_eq!(entries.len(), 3);

        assert_eq!(
            entries[0],
            JlsEntry {
                start: 0,
                end: 73,
                duration_sec: 2,
                error_frames: 14,
                logo_sec: 0,
                label: ":Nologo".to_string(),
            }
        );
        assert_eq!(
            entries[1],
            JlsEntry {
                start: 74,
                end: 6127,
                duration_sec: 15,
                error_frames: 0,
                logo_sec: 15,
                label: ":L".to_string(),
            }
        );
        assert_eq!(
            entries[2],
            JlsEntry {
                start: 6128,
                end: 6577,
                duration_sec: 15,
                error_frames: 0,
                logo_sec: 0,
                label: ":CM".to_string(),
            }
        );

        assert!(entries[2].is_cm());
        assert!(!entries[0].is_cm());
        assert!(!entries[1].is_cm());
    }

    #[test]
    fn detects_trailer_cut_cancel_as_cancelled_trailer() {
        let input = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 100 3 0 3 :Trailer(cut-cancel)\n";
        let entries = parse(input).expect("パースに失敗した");

        assert_eq!(entries.len(), 1);
        assert!(entries[0].is_trailer_cut_cancel());
        assert!(!entries[0].is_cm());
    }

    #[test]
    fn does_not_error_on_unknown_label() {
        let input = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 100 3 0 3 :SomethingNew\n";
        let entries = parse(input).expect("未知ラベルでエラーになってはいけない");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].label, ":SomethingNew");
        assert!(!entries[0].is_cm());
        assert!(!entries[0].is_trailer_cut_cancel());
    }

    #[test]
    fn parses_negative_error_frames() {
        let input = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 100 3 -4 3 :L\n";
        let entries = parse(input).expect("パースに失敗した");

        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].error_frames, -4);
    }
}
