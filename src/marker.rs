//! Trim リストと `.dtvi` から、CM 位置を記録するための2つの文字列を作る純関数。
//!
//! 録画ファイルは切らず、プレイヤーで CM を飛ばせるように位置だけを書き出す。出力は次の2つ。
//!
//! - **EDL**（Kodi / MPlayer 形式）: CM 区間ごとに `開始秒\t終了秒\t3\n` の1行。
//!   Kodi は動画と同名の `.edl` を読んで commercial break（`3`）を自動で飛ばす。
//! - **ffmetadata**: 先頭に `;FFMETADATA1` を置き、本編 / CM が交互に並ぶ `[CHAPTER]` 区間で
//!   `[0, frame_count)` を隙間なく覆う。後続の処理が ffmpeg に渡して mp4 に埋め込む。
//!
//! ## 規則（後続と揃えるための決定事項）
//!
//! - **フレーム数**は `dtvi.frames.len()`。ヘッダの `frame_count` は見ない（抜粋の `.dtvi` と
//!   食い違うため。行数を正とする）。
//! - **CM 区間**は Trim の補集合。`[0, frame_count)` のうち保持区間以外で、先頭と末尾の区間も
//!   含める（前後の番組も飛ばしたい対象のため）。
//! - **境界の時刻**は time_base 単位の整数で持つ。境界 `b` が `0 < b < frame_count` なら
//!   `frames[b].pts`。`b == 0` なら 0、`b == frame_count` なら最終フレームの `pts + duration`。
//!   **フレーム表の添字は表示順**（`frame_number`）であり、デコード順の `sample_number` で引くと
//!   B フレームの並べ替えぶんずれた時刻になる。
//! - **時刻を秒の浮動小数で持ち回さない。** ffmetadata には pts の整数をそのまま書き、丸め誤差を
//!   持ち込まない。秒に直すのは EDL を書くときだけ（小数点以下3桁）。
//! - **切らないので境界をキーフレームに丸めない。** 境界は指定どおりのフレームの pts になる。
//! - **time_base は `.dtvi` ヘッダから整数で読む。** [`crate::report::time_base`] は使わない。
//!   あちらはキーが欠けると黙って `1/1` を返し、f64 を返すため整数の ffmetadata に合わないから。
//!
//! 次の場合はエラーにする（黙って別の値を書かない）:
//! - time_base が欠けている、0 以下、または整数としてパースできない
//! - Trim の終端が `frame_count` を超える
//! - 境界に使うフレームの pts が無効（`has_valid_pts()` が false）

use std::fmt;

use crate::dtvi::Dtvi;
use crate::trim::TrimList;

/// [`markers`] の出力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Markers {
    /// Kodi / MPlayer 形式の EDL。CM 区間が無ければ空文字列。
    pub edl: String,
    /// ffmpeg の ffmetadata（`;FFMETADATA1` で始まるチャプター列）。
    pub ffmetadata: String,
    /// CM 区間の数（先頭・末尾を含む）。
    pub cm_count: usize,
}

/// マーカー生成に失敗したことを表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerError {
    /// time_base のヘッダ値が欠けている、0 以下、または整数でない。
    InvalidTimeBase {
        key: &'static str,
        value: Option<String>,
    },
    /// Trim の終端（含まない）がフレーム数を超えている。
    TrimBeyondFrames { end: usize, frame_count: usize },
    /// 境界に使うフレームの pts が無効。`frame_number` は表示順の添字。
    InvalidPts { frame_number: usize },
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerError::InvalidTimeBase { key, value } => match value {
                Some(v) => write!(f, ".dtvi の {key} が正の整数ではない: {v:?}"),
                None => write!(f, ".dtvi に {key} がない"),
            },
            MarkerError::TrimBeyondFrames { end, frame_count } => write!(
                f,
                "Trim の終端 {end} がフレーム数 {frame_count} を超えている"
            ),
            MarkerError::InvalidPts { frame_number } => {
                write!(f, "表示順 {frame_number} のフレームの pts が無効")
            }
        }
    }
}

impl std::error::Error for MarkerError {}

/// 本編 / CM の1区間。`[start, end)` は表示順のフレーム番号。
struct Segment {
    start: usize,
    end: usize,
    is_cm: bool,
}

/// Trim リストと `.dtvi` から EDL と ffmetadata を作る。
pub fn markers(trim: &TrimList, dtvi: &Dtvi) -> Result<Markers, MarkerError> {
    let (num, den) = time_base(dtvi)?;
    let frame_count = dtvi.frames.len();

    let mut edl = String::new();
    let mut ffmetadata = String::from(";FFMETADATA1\n");
    let mut cm_count = 0;
    for seg in segments(trim, frame_count)? {
        let start = boundary_pts(dtvi, seg.start)?;
        let end = boundary_pts(dtvi, seg.end)?;
        let title = if seg.is_cm {
            cm_count += 1;
            // EDL: 3 は commercial break（自動で飛ばし、巻き戻せば見られる）。
            edl.push_str(&format!(
                "{:.3}\t{:.3}\t3\n",
                seconds(start, num, den),
                seconds(end, num, den)
            ));
            "CM"
        } else {
            "本編"
        };
        ffmetadata.push_str(&format!(
            "[CHAPTER]\nTIMEBASE={num}/{den}\nSTART={start}\nEND={end}\ntitle={title}\n"
        ));
    }

    Ok(Markers {
        edl,
        ffmetadata,
        cm_count,
    })
}

/// `.dtvi` ヘッダから time_base を整数で読む。0 以下は不正とする。
fn time_base(dtvi: &Dtvi) -> Result<(i64, i64), MarkerError> {
    let read = |key: &'static str| {
        let raw = dtvi.header_value(key);
        raw.and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|&v| v > 0)
            .ok_or_else(|| MarkerError::InvalidTimeBase {
                key,
                value: raw.map(str::to_owned),
            })
    };
    Ok((read("time_base_num")?, read("time_base_den")?))
}

/// Trim の補集合まで含めて `[0, frame_count)` を隙間なく覆う区間列を作る。
fn segments(trim: &TrimList, frame_count: usize) -> Result<Vec<Segment>, MarkerError> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for range in trim.ranges() {
        let start = range.start().0 as usize;
        let end = range.end().0 as usize;
        if end > frame_count {
            return Err(MarkerError::TrimBeyondFrames { end, frame_count });
        }
        if start > cursor {
            out.push(Segment {
                start: cursor,
                end: start,
                is_cm: true,
            });
        }
        out.push(Segment {
            start,
            end,
            is_cm: false,
        });
        cursor = end;
    }
    if cursor < frame_count {
        out.push(Segment {
            start: cursor,
            end: frame_count,
            is_cm: true,
        });
    }
    Ok(out)
}

/// 表示順の境界 `b` の時刻（time_base 単位）。`b == frame_count` は最終フレームの末端。
fn boundary_pts(dtvi: &Dtvi, b: usize) -> Result<i64, MarkerError> {
    if b == 0 {
        return Ok(0);
    }
    let n = dtvi.frames.len();
    let idx = b.min(n - 1);
    let frame = &dtvi.frames[idx];
    if !frame.has_valid_pts() {
        return Err(MarkerError::InvalidPts { frame_number: idx });
    }
    Ok(if b == n {
        frame.pts + frame.duration
    } else {
        frame.pts
    })
}

fn seconds(t: i64, num: i64, den: i64) -> f64 {
    t as f64 * num as f64 / den as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtvi::{self, DtviFrame, FLAG_KEY_PACKET, FLAG_VALID_PTS};
    use crate::order::{DecodeIdx, DisplayIdx};
    use std::collections::HashMap;

    /// 実際の `.dtvi` の抜粋（表示順40フレーム、time_base 1/30000）。
    const SAMPLE: &str = include_str!("../tests/data/sample.dtvi");

    fn sample() -> Dtvi {
        dtvi::parse(SAMPLE).expect("実データの抜粋をパースできるはず")
    }

    fn frame(n: u32, pts: i64, duration: i64) -> DtviFrame {
        DtviFrame {
            frame_number: DisplayIdx(n),
            sample_number: DecodeIdx(n),
            random_access_sample: DecodeIdx(0),
            file_offset: 0,
            pts,
            dts: pts,
            duration,
            flags: FLAG_KEY_PACKET | FLAG_VALID_PTS,
        }
    }

    /// `frames` と time_base `1/1000` を持つ手組みの `.dtvi`。
    fn synthetic(frames: Vec<DtviFrame>) -> Dtvi {
        let header = HashMap::from([
            ("time_base_num".to_string(), "1".to_string()),
            ("time_base_den".to_string(), "1000".to_string()),
        ]);
        Dtvi {
            format_version: 1,
            header,
            frames,
        }
    }

    /// 表示順 `i` のフレームが pts `i*10`、duration 10 の10フレーム。
    fn ten_frames() -> Dtvi {
        synthetic((0..10).map(|i| frame(i, i as i64 * 10, 10)).collect())
    }

    fn trim(s: &str) -> TrimList {
        TrimList::parse(s).expect("手書きの Trim はパースできるはず")
    }

    #[test]
    fn boundaries_use_display_order_not_decode_order() {
        // 表示順1はデコード順2、表示順3はデコード順1。デコード順で引くと pts がずれる。
        let dtvi = sample();
        assert_eq!(dtvi.frames[1].sample_number, DecodeIdx(2));
        let m = markers(&trim("Trim(1,9)"), &dtvi).unwrap();

        // 保持は表示順 [1, 10)。CM は [0,1) と [10,40)。
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/30000\nSTART=0\nEND=1001\ntitle=CM\n\
             [CHAPTER]\nTIMEBASE=1/30000\nSTART=1001\nEND=10010\ntitle=本編\n\
             [CHAPTER]\nTIMEBASE=1/30000\nSTART=10010\nEND=40040\ntitle=CM\n"
        );
        assert_eq!(m.cm_count, 2);
    }

    #[test]
    fn edl_lists_only_cm_sections_with_seconds() {
        let m = markers(&trim("Trim(1,9)"), &sample()).unwrap();
        // 1001/30000 = 0.03337 → 0.033、10010/30000 = 0.33367 → 0.334、40040/30000 → 1.335
        assert_eq!(m.edl, "0.000\t0.033\t3\n0.334\t1.335\t3\n");
    }

    #[test]
    fn no_cm_when_trim_covers_everything() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(0,9)"), &dtvi).unwrap();
        assert_eq!(m.edl, "");
        assert_eq!(m.cm_count, 0);
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=100\ntitle=本編\n"
        );
    }

    #[test]
    fn trim_starting_at_zero_has_no_leading_cm() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(0,4)"), &dtvi).unwrap();
        assert_eq!(m.cm_count, 1);
        assert_eq!(m.edl, "0.050\t0.100\t3\n");
        assert!(m.ffmetadata.contains("START=0\nEND=50\ntitle=本編\n"));
    }

    #[test]
    fn trim_running_to_last_frame_ends_at_pts_plus_duration() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(3,9)"), &dtvi).unwrap();
        assert_eq!(m.cm_count, 1);
        assert_eq!(m.edl, "0.000\t0.030\t3\n");
        assert!(m.ffmetadata.ends_with("START=30\nEND=100\ntitle=本編\n"));
    }

    #[test]
    fn multiple_trims_alternate_sections() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(1,2) ++ Trim(5,6)"), &dtvi).unwrap();
        // CM [0,1) 本編 [1,3) CM [3,5) 本編 [5,7) CM [7,10)
        assert_eq!(m.cm_count, 3);
        assert_eq!(m.edl, "0.000\t0.010\t3\n0.030\t0.050\t3\n0.070\t0.100\t3\n");
        let titles: Vec<&str> = m
            .ffmetadata
            .lines()
            .filter_map(|l| l.strip_prefix("title="))
            .collect();
        assert_eq!(titles, ["CM", "本編", "CM", "本編", "CM"]);
    }

    #[test]
    fn missing_time_base_is_an_error() {
        let mut dtvi = ten_frames();
        dtvi.header.remove("time_base_den");
        assert!(matches!(
            markers(&trim("Trim(0,4)"), &dtvi),
            Err(MarkerError::InvalidTimeBase {
                key: "time_base_den",
                value: None
            })
        ));
    }

    #[test]
    fn zero_or_non_integer_time_base_is_an_error() {
        for bad in ["0", "1.5", "abc"] {
            let mut dtvi = ten_frames();
            dtvi.header.insert("time_base_num".into(), bad.into());
            assert!(
                matches!(
                    markers(&trim("Trim(0,4)"), &dtvi),
                    Err(MarkerError::InvalidTimeBase {
                        key: "time_base_num",
                        ..
                    })
                ),
                "{bad} は拒否されるべき"
            );
        }
    }

    #[test]
    fn trim_beyond_frame_count_is_an_error() {
        let dtvi = ten_frames();
        assert_eq!(
            markers(&trim("Trim(5,10)"), &dtvi),
            Err(MarkerError::TrimBeyondFrames {
                end: 11,
                frame_count: 10
            })
        );
    }

    #[test]
    fn invalid_pts_on_boundary_frame_is_an_error() {
        let mut dtvi = ten_frames();
        dtvi.frames[3].flags &= !FLAG_VALID_PTS;
        assert_eq!(
            markers(&trim("Trim(3,9)"), &dtvi),
            Err(MarkerError::InvalidPts { frame_number: 3 })
        );
    }
}
