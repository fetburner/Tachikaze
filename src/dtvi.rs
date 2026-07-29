//! `dtvindex build` が生成する `.dtvi` 索引ファイルのパーサ。
//!
//! ## フォーマット
//!
//! UTF-8・タブ区切りのテキスト。1行目は `DTVINDEX\t<format_version>`。続いて
//! `key\tvalue` 形式のヘッダ行が並び、`FRAMES` という1行だけのマーカーの後にフレーム行が続く。
//! フレーム行は次の8列をタブ区切りで持つ:
//!
//! ```text
//! frame_number  sample_number  random_access_sample  file_offset  pts  dts  duration  flags
//! ```
//!
//! - `frame_number`: 0始まりの表示順（このモジュールでは [`DisplayIdx`] にする）
//! - `sample_number`: 0始まりのデコード順（[`DecodeIdx`] にする）
//! - `random_access_sample`: デコード順で直前のキーパケット（[`DecodeIdx`]）
//! - `flags`: 下記の定数を参照
//!
//! この仕様は https://github.com/tobitti0/dtvindex の
//! `docs/index-format-v1.md`（コミット時点の HEAD）に基づく。ヘッダのキー名・型・
//! `FRAMES` マーカーの正確な表記は、同リポジトリを `make` でビルドした `dtvindex` バイナリを
//! 実際に動かして得た `.dtvi` の実データで確認した（`tests/data/sample.dtvi` はその抜粋）。
//!
//! 実データを確認したところ、ヘッダ行は次のキーを持っていた（`DTVINDEX` 行を除く）:
//! `timeline_profile`, `source_size`, `source_mtime_ns`, `source_fingerprint`,
//! `stream_index`, `codec_id`, `width`, `height`, `field_order`, `time_base_num`,
//! `time_base_den`, `frame_rate_num`, `frame_rate_den`, `start_time`, `duration`,
//! `frame_count`。
//!
//! `leading_frame_count`（オープン GOP 判定に使う値）は **ヘッダのキーとしては存在しない**。
//! `dtvindex info` サブコマンドがフレーム表から動的に算出して表示している値であり、
//! `FLAG_LEADING_SAMPLE` が立っているフレームの数と一致することを実データで確認した。
//! そのため本モジュールでもヘッダから読むのではなく、[`Dtvi::leading_frame_count`] として
//! フレーム表から同じ方法で算出する。
//!
//! フレーム表の後には `END` という1行だけのマーカーが続くことがある（`index-format-v1.md`
//! には明記されていないが実データに存在した）。存在すればそこで読み込みを打ち切り、
//! 存在しなければ入力の終端まで読む。
//!
//! ヘッダは将来のバージョンでキーが増えることを想定し、未知のキーがあってもエラーに
//! せず素通りする（[`Dtvi::header`] に文字列のマップとして保持する）。

use std::collections::HashMap;
use std::fmt;

use crate::order::{DecodeIdx, DisplayIdx};

/// キーパケット（IDR等、他のパケットを参照せず単独でデコードできるパケット）。
pub const FLAG_KEY_PACKET: u8 = 0x01;
/// 最初のランダムアクセス可能な PTS より前に提示される先行サンプル。
/// オープン GOP の判定に使う（[`Dtvi::leading_frame_count`] 参照）。
pub const FLAG_LEADING_SAMPLE: u8 = 0x02;
// 現在の cut/analyze パイプラインは PTS・DTS・バイト位置の有効性を利用しない
// （キーパケット判定とオープン GOP 判定にしか flags を使わない）ため、以下の
// 3つのフラグとその判定メソッドはどこからも呼ばれない。`.dtvi` の仕様を
// 完全な形で残す（将来デバッグや詳細な報告に使う可能性がある）ためのフラグ
// 定義として意図的に残す。
#[allow(dead_code)]
/// 有効な PTS を持つ。
pub const FLAG_VALID_PTS: u8 = 0x04;
#[allow(dead_code)]
/// 有効な DTS を持つ。
pub const FLAG_VALID_DTS: u8 = 0x08;
#[allow(dead_code)]
/// 有効なファイル内バイト位置を持つ。
pub const FLAG_VALID_BYTE_POSITION: u8 = 0x10;
/// 提示タイムスタンプが直近のキーパケットより前になるため、
/// より前の RAP（ランダムアクセスポイント）が必要。
pub const FLAG_REQUIRES_EARLIER_RAP: u8 = 0x20;

/// ヘッダの先頭行に書かれているマジック文字列。
const MAGIC: &str = "DTVINDEX";
/// ヘッダとフレーム表の境界を示すマーカー行。
const FRAMES_MARKER: &str = "FRAMES";
/// フレーム表の終端を示すマーカー行。存在しない `.dtvi` もあり得るため必須ではない。
const END_MARKER: &str = "END";

/// `.dtvi` の1フレーム分のエントリ。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DtviFrame {
    /// 0始まりの表示順。
    pub frame_number: DisplayIdx,
    /// 0始まりのデコード順。
    pub sample_number: DecodeIdx,
    /// デコード順で直前のキーパケット。
    pub random_access_sample: DecodeIdx,
    /// ファイル内のバイト位置。`FLAG_VALID_BYTE_POSITION` が立っていない場合は無効。
    pub file_offset: u64,
    /// 提示タイムスタンプ（time_base 単位）。`FLAG_VALID_PTS` が立っていない場合は無効。
    pub pts: i64,
    /// デコードタイムスタンプ（time_base 単位）。`FLAG_VALID_DTS` が立っていない場合は無効。
    pub dts: i64,
    /// 表示時間の長さ（time_base 単位）。
    pub duration: i64,
    /// `FLAG_*` 定数の組み合わせ。
    pub flags: u8,
}

impl DtviFrame {
    /// キーパケットかどうか。
    pub fn is_key_packet(&self) -> bool {
        self.flags & FLAG_KEY_PACKET != 0
    }

    /// 最初のランダムアクセス可能な PTS より前の先行サンプルかどうか。
    pub fn is_leading_sample(&self) -> bool {
        self.flags & FLAG_LEADING_SAMPLE != 0
    }

    #[allow(dead_code)]
    /// 有効な PTS を持つかどうか。
    pub fn has_valid_pts(&self) -> bool {
        self.flags & FLAG_VALID_PTS != 0
    }

    #[allow(dead_code)]
    /// 有効な DTS を持つかどうか。
    pub fn has_valid_dts(&self) -> bool {
        self.flags & FLAG_VALID_DTS != 0
    }

    #[allow(dead_code)]
    /// 有効なファイル内バイト位置を持つかどうか。
    pub fn has_valid_byte_position(&self) -> bool {
        self.flags & FLAG_VALID_BYTE_POSITION != 0
    }

    /// 直近のキーパケットより前の RAP が必要かどうか。
    pub fn requires_earlier_rap(&self) -> bool {
        self.flags & FLAG_REQUIRES_EARLIER_RAP != 0
    }
}

/// `.dtvi` ファイル全体。ヘッダとフレーム表を保持する。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dtvi {
    /// 1行目 `DTVINDEX\t<version>` の `<version>`。
    pub format_version: u32,
    /// `DTVINDEX` 行と `FRAMES` マーカーを除く、ヘッダの key/value 行すべて。
    /// バージョンが上がってキーが増えても壊れないよう、未知のキーも文字列のまま保持する。
    pub header: HashMap<String, String>,
    /// `FRAMES` マーカー以降のフレーム行。`frame_number` の昇順（0始まり連番）で並ぶ。
    pub frames: Vec<DtviFrame>,
}

impl Dtvi {
    /// ヘッダの生の文字列値を取得する。
    pub fn header_value(&self, key: &str) -> Option<&str> {
        self.header.get(key).map(String::as_str)
    }

    /// オープン GOP 判定に使う先行フレーム数。
    ///
    /// `.dtvi` のヘッダには `leading_frame_count` というキーは存在しない。これは
    /// `dtvindex info` がフレーム表から動的に算出して表示している値であり、
    /// `FLAG_LEADING_SAMPLE` が立っているフレームの数と一致する（実データで確認済み）。
    /// 対象3ファイルはすべてこの値が0（クローズド GOP）である。
    pub fn leading_frame_count(&self) -> usize {
        self.frames.iter().filter(|f| f.is_leading_sample()).count()
    }
}

/// `.dtvi` のパースに失敗したことを表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DtviParseError {
    /// 入力が空、または1行目が `DTVINDEX\t<version>` の形式ではない。
    MissingOrInvalidMagic {
        /// 実際に読み取った1行目（存在すれば）。
        line: Option<String>,
    },
    /// ヘッダ中の `key\tvalue` 行がタブを含まない等、形式が壊れている。
    MalformedHeaderLine {
        /// 元ファイル中の行番号（1始まり）。
        line_no: usize,
        line: String,
    },
    /// `FRAMES` マーカーが見つからないまま入力が終わった。
    MissingFramesMarker,
    /// フレーム行の列数が8列に満たない、または数値列が数値として解釈できない。
    MalformedFrameLine {
        /// 元ファイル中の行番号（1始まり）。
        line_no: usize,
        line: String,
    },
    /// `frame_number` が0始まりの連番になっていない。
    NonSequentialFrameNumber {
        /// 元ファイル中の行番号（1始まり）。
        line_no: usize,
        /// 本来その行にあるべきだった値。
        expected: u32,
        /// 実際に読み取った値。
        found: u32,
    },
    /// `sample_number` がフレーム数分の順列になっていない
    /// （範囲外の値、または重複した値がある）。
    InvalidSampleNumberPermutation {
        /// フレーム表内での相対位置（0始まり）。
        frame_index: usize,
        /// 問題のあった `sample_number` の値。
        sample_number: u32,
    },
}

impl fmt::Display for DtviParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DtviParseError::MissingOrInvalidMagic { line } => write!(
                f,
                ".dtvi の1行目が `{MAGIC}\\t<version>` の形式ではありません: {line:?}"
            ),
            DtviParseError::MalformedHeaderLine { line_no, line } => write!(
                f,
                ".dtvi の {line_no} 行目のヘッダをパースできません（`key\\tvalue` 形式ではありません）: {line:?}"
            ),
            DtviParseError::MissingFramesMarker => {
                write!(f, ".dtvi に `{FRAMES_MARKER}` マーカーが見つかりません")
            }
            DtviParseError::MalformedFrameLine { line_no, line } => write!(
                f,
                ".dtvi の {line_no} 行目のフレーム行をパースできません: {line:?}"
            ),
            DtviParseError::NonSequentialFrameNumber {
                line_no,
                expected,
                found,
            } => write!(
                f,
                ".dtvi の {line_no} 行目の frame_number が連番ではありません（期待値: {expected}, 実際: {found}）"
            ),
            DtviParseError::InvalidSampleNumberPermutation {
                frame_index,
                sample_number,
            } => write!(
                f,
                ".dtvi のフレーム表 {frame_index} 番目の sample_number ({sample_number}) が \
                 フレーム数分の順列になっていません（範囲外または重複）"
            ),
        }
    }
}

impl std::error::Error for DtviParseError {}

/// `.dtvi` の内容全体をパースする。
///
/// - ヘッダの未知のキーはエラーにしない（[`Dtvi::header`] にそのまま保持する）。
/// - フレーム行の列数が8に満たない行、数値列が数値として解釈できない行はエラーにする。
/// - `frame_number` が0始まりの連番であること、`sample_number` がフレーム数分の
///   順列になっていることを検証し、不整合ならエラーにする。
pub fn parse(input: &str) -> Result<Dtvi, DtviParseError> {
    let mut lines = input.lines().enumerate().map(|(idx, line)| (idx + 1, line));

    // 1行目: `DTVINDEX\t<version>`
    let (_, first_line) = lines
        .next()
        .ok_or(DtviParseError::MissingOrInvalidMagic { line: None })?;
    let format_version = first_line
        .split_once('\t')
        .filter(|(magic, _)| *magic == MAGIC)
        .and_then(|(_, version)| version.trim().parse::<u32>().ok())
        .ok_or_else(|| DtviParseError::MissingOrInvalidMagic {
            line: Some(first_line.to_string()),
        })?;

    // ヘッダ行: `FRAMES` マーカーまでの key/value 行。
    let mut header = HashMap::new();
    let mut found_frames_marker = false;
    for (line_no, line) in lines.by_ref() {
        if line == FRAMES_MARKER {
            found_frames_marker = true;
            break;
        }
        if line.trim().is_empty() {
            continue;
        }
        let (key, value) =
            line.split_once('\t')
                .ok_or_else(|| DtviParseError::MalformedHeaderLine {
                    line_no,
                    line: line.to_string(),
                })?;
        header.insert(key.to_string(), value.to_string());
    }
    if !found_frames_marker {
        return Err(DtviParseError::MissingFramesMarker);
    }

    // フレーム行: `END` マーカーが出るか入力が尽きるまで。
    let mut frames = Vec::new();
    for (line_no, line) in lines {
        if line == END_MARKER {
            break;
        }
        if line.trim().is_empty() {
            continue;
        }

        let fields: Vec<&str> = line.split('\t').collect();
        let malformed = || DtviParseError::MalformedFrameLine {
            line_no,
            line: line.to_string(),
        };
        let [frame_number, sample_number, random_access_sample, file_offset, pts, dts, duration, flags] =
            <[&str; 8]>::try_from(fields).map_err(|_| malformed())?;

        let frame_number: u32 = frame_number.parse().map_err(|_| malformed())?;
        let sample_number: u32 = sample_number.parse().map_err(|_| malformed())?;
        let random_access_sample: u32 = random_access_sample.parse().map_err(|_| malformed())?;
        let file_offset: u64 = file_offset.parse().map_err(|_| malformed())?;
        let pts: i64 = pts.parse().map_err(|_| malformed())?;
        let dts: i64 = dts.parse().map_err(|_| malformed())?;
        let duration: i64 = duration.parse().map_err(|_| malformed())?;
        let flags: u8 = flags.parse().map_err(|_| malformed())?;

        let expected_frame_number = frames.len() as u32;
        if frame_number != expected_frame_number {
            return Err(DtviParseError::NonSequentialFrameNumber {
                line_no,
                expected: expected_frame_number,
                found: frame_number,
            });
        }

        frames.push(DtviFrame {
            frame_number: DisplayIdx(frame_number),
            sample_number: DecodeIdx(sample_number),
            random_access_sample: DecodeIdx(random_access_sample),
            file_offset,
            pts,
            dts,
            duration,
            flags,
        });
    }

    validate_sample_number_permutation(&frames)?;

    Ok(Dtvi {
        format_version,
        header,
        frames,
    })
}

/// `sample_number` がフレーム数分の順列（`0..frames.len()` の値がそれぞれ1回ずつ）に
/// なっていることを検証する。
fn validate_sample_number_permutation(frames: &[DtviFrame]) -> Result<(), DtviParseError> {
    let mut seen = vec![false; frames.len()];
    for (frame_index, frame) in frames.iter().enumerate() {
        let sample_number = frame.sample_number.0 as usize;
        match seen.get_mut(sample_number) {
            Some(slot) if !*slot => *slot = true,
            _ => {
                return Err(DtviParseError::InvalidSampleNumberPermutation {
                    frame_index,
                    sample_number: frame.sample_number.0,
                })
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 実際に `dtvindex build` で生成した `.dtvi` の抜粋（ヘッダ全体 + 先頭40フレーム）。
    const SAMPLE: &str = include_str!("../tests/data/sample.dtvi");

    #[test]
    fn parses_real_header_and_frames() {
        let dtvi = parse(SAMPLE).expect("実データの抜粋をパースできるはず");

        assert_eq!(dtvi.format_version, 1);
        assert_eq!(
            dtvi.header_value("timeline_profile"),
            Some("dtv-display-order-v1")
        );
        assert_eq!(dtvi.header_value("frame_count"), Some("599"));
        assert_eq!(dtvi.header_value("codec_id"), Some("27"));
        assert_eq!(dtvi.frames.len(), 40);
    }

    #[test]
    fn first_frame_is_a_key_packet_with_expected_fields() {
        let dtvi = parse(SAMPLE).expect("パースに失敗した");
        let first = &dtvi.frames[0];

        assert_eq!(first.frame_number, DisplayIdx(0));
        assert_eq!(first.sample_number, DecodeIdx(0));
        assert_eq!(first.random_access_sample, DecodeIdx(0));
        assert_eq!(first.pts, 0);
        assert_eq!(first.dts, -2002);
        assert!(first.is_key_packet());
        assert!(!first.is_leading_sample());
        assert!(first.has_valid_pts());
        assert!(first.has_valid_dts());
        assert!(first.has_valid_byte_position());
        assert!(!first.requires_earlier_rap());
    }

    #[test]
    fn non_first_frame_is_not_a_key_packet() {
        let dtvi = parse(SAMPLE).expect("パースに失敗した");
        assert!(!dtvi.frames[1].is_key_packet());
    }

    #[test]
    fn sample_data_has_zero_leading_frames() {
        // タスク前提: 対象3ファイルはすべて leading_frame_count が0（クローズド GOP）。
        let dtvi = parse(SAMPLE).expect("パースに失敗した");
        assert_eq!(dtvi.leading_frame_count(), 0);
    }

    #[test]
    fn unknown_header_key_does_not_fail_parsing() {
        let input = "\
DTVINDEX\t1
timeline_profile\tdtv-display-order-v1
frame_count\t1
future_new_field_v2\tsomething-unexpected
FRAMES
0\t0\t0\t0\t0\t0\t1001\t29
";
        let dtvi = parse(input).expect("未知のヘッダキーがあってもエラーになってはいけない");
        assert_eq!(
            dtvi.header_value("future_new_field_v2"),
            Some("something-unexpected")
        );
        assert_eq!(dtvi.frames.len(), 1);
    }

    #[test]
    fn stops_frame_parsing_at_end_marker() {
        let input = "\
DTVINDEX\t1
FRAMES
0\t0\t0\t0\t0\t0\t1001\t29
1\t1\t0\t0\t1001\t1001\t1001\t28
END
このあとの行は読み飛ばされるべき garbage
";
        let dtvi = parse(input).expect("END マーカーまでは正しくパースできるはず");
        assert_eq!(dtvi.frames.len(), 2);
    }

    #[test]
    fn works_without_trailing_end_marker() {
        let input = "\
DTVINDEX\t1
FRAMES
0\t0\t0\t0\t0\t0\t1001\t29
1\t1\t0\t0\t1001\t1001\t1001\t28
";
        let dtvi = parse(input).expect("END マーカーがなくても入力の終端まで読めるはず");
        assert_eq!(dtvi.frames.len(), 2);
    }

    #[test]
    fn missing_magic_line_is_an_error() {
        let input = "not-dtvindex\t1\nFRAMES\n";
        assert!(matches!(
            parse(input),
            Err(DtviParseError::MissingOrInvalidMagic { .. })
        ));
    }

    #[test]
    fn missing_frames_marker_is_an_error() {
        let input = "DTVINDEX\t1\nsome_key\tsome_value\n";
        assert_eq!(parse(input), Err(DtviParseError::MissingFramesMarker));
    }

    #[test]
    fn frame_line_with_too_few_columns_is_an_error() {
        let input = "DTVINDEX\t1\nFRAMES\n0\t0\t0\t0\t0\n";
        assert!(matches!(
            parse(input),
            Err(DtviParseError::MalformedFrameLine { line_no: 3, .. })
        ));
    }

    #[test]
    fn frame_line_with_non_numeric_value_is_an_error() {
        let input = "DTVINDEX\t1\nFRAMES\nzero\t0\t0\t0\t0\t0\t1001\t29\n";
        assert!(matches!(
            parse(input),
            Err(DtviParseError::MalformedFrameLine { line_no: 3, .. })
        ));
    }

    #[test]
    fn header_line_without_tab_is_an_error() {
        let input = "DTVINDEX\t1\nno_tab_here\nFRAMES\n";
        assert!(matches!(
            parse(input),
            Err(DtviParseError::MalformedHeaderLine { line_no: 2, .. })
        ));
    }

    #[test]
    fn non_sequential_frame_number_is_an_error() {
        let input =
            "DTVINDEX\t1\nFRAMES\n0\t0\t0\t0\t0\t0\t1001\t29\n2\t1\t0\t0\t1001\t1001\t1001\t28\n";
        assert_eq!(
            parse(input),
            Err(DtviParseError::NonSequentialFrameNumber {
                line_no: 4,
                expected: 1,
                found: 2,
            })
        );
    }

    #[test]
    fn frame_number_not_starting_at_zero_is_an_error() {
        let input = "DTVINDEX\t1\nFRAMES\n1\t0\t0\t0\t0\t0\t1001\t29\n";
        assert_eq!(
            parse(input),
            Err(DtviParseError::NonSequentialFrameNumber {
                line_no: 3,
                expected: 0,
                found: 1,
            })
        );
    }

    #[test]
    fn duplicate_sample_number_is_an_error() {
        let input = "\
DTVINDEX\t1
FRAMES
0\t0\t0\t0\t0\t0\t1001\t29
1\t0\t0\t0\t1001\t1001\t1001\t28
";
        assert!(matches!(
            parse(input),
            Err(DtviParseError::InvalidSampleNumberPermutation { .. })
        ));
    }

    #[test]
    fn out_of_range_sample_number_is_an_error() {
        let input = "\
DTVINDEX\t1
FRAMES
0\t5\t0\t0\t0\t0\t1001\t29
1\t1\t0\t0\t1001\t1001\t1001\t28
";
        assert!(matches!(
            parse(input),
            Err(DtviParseError::InvalidSampleNumberPermutation { .. })
        ));
    }

    #[test]
    fn flag_helpers_reflect_bit_combinations() {
        let frame = DtviFrame {
            frame_number: DisplayIdx(0),
            sample_number: DecodeIdx(0),
            random_access_sample: DecodeIdx(0),
            file_offset: 0,
            pts: 0,
            dts: 0,
            duration: 0,
            flags: FLAG_LEADING_SAMPLE | FLAG_REQUIRES_EARLIER_RAP,
        };
        assert!(!frame.is_key_packet());
        assert!(frame.is_leading_sample());
        assert!(!frame.has_valid_pts());
        assert!(!frame.has_valid_dts());
        assert!(!frame.has_valid_byte_position());
        assert!(frame.requires_earlier_rap());
    }
}
