//! join_logo_scp が出す `trim.avs` のパースと生成。
//!
//! `trim.avs` の書式（1行のテキスト）:
//!
//! ```text
//! Trim(66,34201) ++ Trim(37798,53591) ++ Trim(57189,70974)
//! ```
//!
//! `Trim(s,e)` は**両端を含む**表示順のフレーム範囲。内部表現はこのモジュールのパース処理
//! （[`TrimList::parse`]）でのみ半開区間 `[s, e+1)` に正規化する。[`TrimRange`] のフィールドは
//! private にし、パース経由以外での構築を禁止することで、両端含む値と半開区間の値の混同を
//! 防ぐ。生成（[`std::fmt::Display`]）は逆に両端を含む形式へ戻す。

use crate::order::DisplayIdx;
use std::fmt;

/// パース済みで半開区間 `[start, end)` に正規化された Trim 区間。表示順のフレーム番号。
///
/// フィールドは private。[`TrimList::parse`] を経由してのみ生成される。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrimRange {
    start: DisplayIdx,
    end: DisplayIdx,
}

impl TrimRange {
    /// 区間の開始（含む）。
    pub fn start(&self) -> DisplayIdx {
        self.start
    }

    /// 区間の終端（含まない、半開区間）。
    pub fn end(&self) -> DisplayIdx {
        self.end
    }
}

/// パース済みの Trim リスト全体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrimList {
    ranges: Vec<TrimRange>,
}

/// `trim.avs` のパース失敗を表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TrimParseError {
    /// 入力が空、または空白のみ。
    Empty,
    /// `Trim(s,e)` の書式に合致しないトークンがあった。
    Malformed(String),
    /// `s` または `e` が数値としてパースできなかった。
    InvalidNumber(String),
    /// `e + 1` が `u32` を溢れる。
    Overflow { end: u32 },
    /// `s > e`（空区間）。
    EmptyRange { start: u32, end: u32 },
    /// 区間が昇順に並んでいない、または前の区間と重なっている。
    NotAscendingOrOverlapping {
        prev_end_exclusive: u32,
        next_start: u32,
    },
}

impl fmt::Display for TrimParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TrimParseError::Empty => write!(f, "Trim リストが空、または Trim(...) が0個"),
            TrimParseError::Malformed(token) => {
                write!(f, "Trim(s,e) の書式に合致しないトークン: {token:?}")
            }
            TrimParseError::InvalidNumber(token) => {
                write!(f, "数値としてパースできない: {token:?}")
            }
            TrimParseError::Overflow { end } => {
                write!(f, "e + 1 が u32 を溢れる: e={end}")
            }
            TrimParseError::EmptyRange { start, end } => {
                write!(f, "s > e の空区間: Trim({start},{end})")
            }
            TrimParseError::NotAscendingOrOverlapping {
                prev_end_exclusive,
                next_start,
            } => {
                write!(
                    f,
                    "区間が昇順でない、または重なっている: 前の区間の終端(半開)={prev_end_exclusive}, 次の区間の開始={next_start}"
                )
            }
        }
    }
}

impl std::error::Error for TrimParseError {}

impl TrimList {
    /// `trim.avs` の内容をパースする。
    ///
    /// 許容する揺れ:
    /// - `++` の前後の空白の有無
    /// - 行頭行末の空白
    /// - 複数行に分かれている（改行を空白として扱う）
    /// - 末尾の改行
    pub fn parse(input: &str) -> Result<Self, TrimParseError> {
        if input.trim().is_empty() {
            return Err(TrimParseError::Empty);
        }

        let mut ranges = Vec::new();
        for token in input.split("++") {
            let token = token.trim();
            let (start, end) = parse_trim_token(token)?;

            if start > end {
                return Err(TrimParseError::EmptyRange { start, end });
            }
            let end_exclusive = end.checked_add(1).ok_or(TrimParseError::Overflow { end })?;

            if let Some(prev) = ranges.last() {
                let prev: &TrimRange = prev;
                if start < prev.end.0 {
                    return Err(TrimParseError::NotAscendingOrOverlapping {
                        prev_end_exclusive: prev.end.0,
                        next_start: start,
                    });
                }
            }

            ranges.push(TrimRange {
                start: DisplayIdx(start),
                end: DisplayIdx(end_exclusive),
            });
        }

        if ranges.is_empty() {
            return Err(TrimParseError::Empty);
        }

        Ok(TrimList { ranges })
    }

    /// パース済みの区間列（半開区間）を返す。
    pub fn ranges(&self) -> &[TrimRange] {
        &self.ranges
    }
}

/// `Trim(s,e)` の1トークンをパースし、両端を含む `(s, e)` を `u32` で返す。
fn parse_trim_token(token: &str) -> Result<(u32, u32), TrimParseError> {
    let inner = token
        .strip_prefix("Trim(")
        .and_then(|rest| rest.strip_suffix(')'))
        .ok_or_else(|| TrimParseError::Malformed(token.to_string()))?;

    let mut parts = inner.split(',');
    let start_str = parts
        .next()
        .ok_or_else(|| TrimParseError::Malformed(token.to_string()))?;
    let end_str = parts
        .next()
        .ok_or_else(|| TrimParseError::Malformed(token.to_string()))?;
    if parts.next().is_some() {
        return Err(TrimParseError::Malformed(token.to_string()));
    }

    let start = start_str
        .trim()
        .parse::<u32>()
        .map_err(|_| TrimParseError::InvalidNumber(start_str.to_string()))?;
    let end = end_str
        .trim()
        .parse::<u32>()
        .map_err(|_| TrimParseError::InvalidNumber(end_str.to_string()))?;

    Ok((start, end))
}

impl fmt::Display for TrimList {
    /// join_logo_scp と同じ、両端を含む書式に戻す: `Trim(s,e) ++ Trim(s,e) ...`
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let rendered: Vec<String> = self
            .ranges
            .iter()
            .map(|r| format!("Trim({},{})", r.start.0, r.end.0 - 1))
            .collect();
        write!(f, "{}", rendered.join(" ++ "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn as_pairs(list: &TrimList) -> Vec<(u32, u32)> {
        list.ranges()
            .iter()
            .map(|r| (r.start().0, r.end().0))
            .collect()
    }

    #[test]
    fn parses_example_into_half_open_ranges() {
        let input = "Trim(66,34201) ++ Trim(37798,53591) ++ Trim(57189,70974)";
        let list = TrimList::parse(input).expect("should parse");
        assert_eq!(
            as_pairs(&list),
            vec![(66, 34202), (37798, 53592), (57189, 70975)]
        );
    }

    #[test]
    fn tolerates_whitespace_and_multiline_and_trailing_newline() {
        let input = "  Trim(0,10)\n   ++\nTrim(20,30) ++Trim(40,50)\n\n";
        let list = TrimList::parse(input).expect("should parse");
        assert_eq!(as_pairs(&list), vec![(0, 11), (20, 31), (40, 51)]);
    }

    #[test]
    fn round_trip_parse_display_parse() {
        let input = "Trim(66,34201) ++ Trim(37798,53591) ++ Trim(57189,70974)";
        let first = TrimList::parse(input).expect("should parse");
        let rendered = first.to_string();
        let second = TrimList::parse(&rendered).expect("should re-parse");
        assert_eq!(first, second);
        // 両端含む形式に戻っていることも確認する。
        assert_eq!(rendered, input);
    }

    #[test]
    fn empty_string_is_error() {
        assert_eq!(TrimList::parse(""), Err(TrimParseError::Empty));
        assert_eq!(TrimList::parse("   \n  "), Err(TrimParseError::Empty));
    }

    #[test]
    fn start_greater_than_end_is_error() {
        let err = TrimList::parse("Trim(100,50)").unwrap_err();
        assert_eq!(
            err,
            TrimParseError::EmptyRange {
                start: 100,
                end: 50
            }
        );
    }

    #[test]
    fn overlapping_ranges_are_error() {
        let err = TrimList::parse("Trim(0,100) ++ Trim(50,200)").unwrap_err();
        assert_eq!(
            err,
            TrimParseError::NotAscendingOrOverlapping {
                prev_end_exclusive: 101,
                next_start: 50,
            }
        );
    }

    #[test]
    fn non_ascending_ranges_are_error() {
        let err = TrimList::parse("Trim(100,200) ++ Trim(0,50)").unwrap_err();
        assert_eq!(
            err,
            TrimParseError::NotAscendingOrOverlapping {
                prev_end_exclusive: 201,
                next_start: 0,
            }
        );
    }

    #[test]
    fn non_numeric_value_is_error() {
        let err = TrimList::parse("Trim(abc,100)").unwrap_err();
        assert_eq!(err, TrimParseError::InvalidNumber("abc".to_string()));
    }

    #[test]
    fn end_plus_one_overflow_is_error() {
        let input = format!("Trim(0,{})", u32::MAX);
        let err = TrimList::parse(&input).unwrap_err();
        assert_eq!(err, TrimParseError::Overflow { end: u32::MAX });
    }

    #[test]
    fn malformed_token_is_error() {
        let err = TrimList::parse("not a trim list").unwrap_err();
        assert!(matches!(err, TrimParseError::Malformed(_)));
    }

    #[test]
    fn adjacent_ranges_touching_at_boundary_are_allowed() {
        // 半開区間として前の終端 == 次の開始は重なりではない。
        let list = TrimList::parse("Trim(0,9) ++ Trim(10,19)").expect("should parse");
        assert_eq!(as_pairs(&list), vec![(0, 10), (10, 20)]);
    }
}
