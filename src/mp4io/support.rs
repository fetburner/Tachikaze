//! 未対応の入力構成を早期に検出する。
//!
//! CLAUDE.md の方針: 未検証・未実装の構成は、静かに間違った出力を出すより
//! 明示的に落とす。ここでチェックする構成はどれも「エラーは出ないが結果が
//! 壊れる」可能性がある未検証パスであり、`--force` のような続行フラグは
//! 用意しない(誤った出力を作れる経路を残さないため)。
//!
//! 検出する5条件は docs/implementation-plan.md の「未解決事項」に対応する:
//!
//! - `elst`(edit list)の存在 — サンプルを削った後にタイムラインが整合する
//!   とは限らない
//! - `stsd` の複数エントリ — `sample_description_index` を1固定にしている
//! - オープン GOP — 「パケット数 == E - S」規則が成立しない
//! - 音声トラックが2本以上、または映像/音声以外のトラック(字幕など)がある
//! - 映像トラックが0本または2本以上
//!
//! `stco`/`co64` のどちらであるか自体はここでは判定しない(読み込みは両方通す。
//! 出力側の `co64` 対応は別issue)。

use mp4_atom::{Codec, Moov};

use crate::dtvi::Dtvi;

/// 対応していない入力構成を検出したときに返すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedInput {
    /// なぜ処理を中止したか。
    pub reason: String,
    /// どうすればよいか(わかる場合のみ)。
    pub suggestion: Option<String>,
}

impl UnsupportedInput {
    fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            suggestion: None,
        }
    }

    fn with_suggestion(reason: impl Into<String>, suggestion: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
            suggestion: Some(suggestion.into()),
        }
    }
}

impl std::fmt::Display for UnsupportedInput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "error: {}", self.reason)?;
        if let Some(suggestion) = &self.suggestion {
            write!(f, "\n       {suggestion}")?;
        }
        Ok(())
    }
}

impl std::error::Error for UnsupportedInput {}

/// `moov`(と、あれば `.dtvi`)を検査し、対応していない構成があればエラーを返す。
///
/// `.dtvi` は `dtvi: None` を許すシグネチャになっているが、オープン GOP は
/// `.dtvi` のフレーム表からしか判定できない(moov の `stss`/`ctts` だけでは
/// フレーム間の参照構造が分からない)。そのため `.dtvi` が渡されなかった場合は
/// 「チェックをスキップして警告」ではなく「`.dtvi` を要求してエラーにする」を
/// 選んでいる。analyze パイプラインでは chapter_exe が解析時に必ず `.dtvi` を
/// 生成するため(docs/pipeline.md)、実運用でこの分岐に入ることは想定していない。
pub fn check_supported(moov: &Moov, dtvi: Option<&Dtvi>) -> Result<(), UnsupportedInput> {
    check_track_counts(moov)?;
    check_no_edit_list(moov)?;
    check_single_stsd_entry(moov)?;
    check_closed_gop(dtvi)?;
    Ok(())
}

/// トラックの `stsd` の先頭エントリから `Codec` を取り出す。
fn track_codec(trak: &mp4_atom::Trak) -> Option<&Codec> {
    trak.mdia.minf.stbl.stsd.codecs.first()
}

/// 映像トラックがちょうど1本、音声トラックが1本、それ以外(字幕など)のトラックが
/// 0本であることを確認する。
fn check_track_counts(moov: &Moov) -> Result<(), UnsupportedInput> {
    let mut video_count = 0usize;
    let mut audio_count = 0usize;
    let mut other_count = 0usize;

    for trak in &moov.trak {
        match track_codec(trak) {
            Some(Codec::Avc1(_)) => video_count += 1,
            Some(Codec::Opus(_)) => audio_count += 1,
            _ => other_count += 1,
        }
    }

    if video_count != 1 {
        return Err(UnsupportedInput::new(format!(
            "映像トラックが {video_count} 本あります。この構成は未検証のため処理を中止しました\
             (対応するのは映像トラック1本のみです)。"
        )));
    }

    if audio_count >= 2 || other_count > 0 {
        return Err(UnsupportedInput::new(format!(
            "音声トラックが {audio_count} 本、映像/音声以外のトラック(字幕など)が {other_count} 本\
             あります。この構成は未検証のため処理を中止しました\
             (対応するのは映像1本+音声1本のみです)。"
        )));
    }

    Ok(())
}

/// `elst`(edit list)が存在しないことを確認する。
fn check_no_edit_list(moov: &Moov) -> Result<(), UnsupportedInput> {
    let has_elst = moov
        .trak
        .iter()
        .any(|trak| trak.edts.as_ref().is_some_and(|edts| edts.elst.is_some()));

    if has_elst {
        return Err(UnsupportedInput::with_suggestion(
            "入力に edit list (elst) があります。この構成は未検証のため処理を中止しました。",
            "ffmpeg で edit list を除去してから再試行してください(動作確認済み):\n         \
             ffmpeg -i IN.mp4 -c copy -use_editlist 0 -movflags +faststart OUT.mp4",
        ));
    }

    Ok(())
}

/// どのトラックの `stsd` も1エントリであることを確認する。
fn check_single_stsd_entry(moov: &Moov) -> Result<(), UnsupportedInput> {
    let has_multiple_entries = moov
        .trak
        .iter()
        .any(|trak| trak.mdia.minf.stbl.stsd.codecs.len() > 1);

    if has_multiple_entries {
        return Err(UnsupportedInput::new(
            "stsd に複数のサンプルエントリを持つトラックがあります。この構成は未検証のため\
             処理を中止しました(sample_description_index を1固定にしているため)。",
        ));
    }

    Ok(())
}

/// オープン GOP でないことを `.dtvi` から確認する。
///
/// `.dtvi` が無い場合はチェックできないため、警告してスキップするのではなく
/// エラーにする(理由は [`check_supported`] のドキュメントを参照)。
fn check_closed_gop(dtvi: Option<&Dtvi>) -> Result<(), UnsupportedInput> {
    let dtvi = dtvi.ok_or_else(|| {
        UnsupportedInput::with_suggestion(
            "オープン GOP かどうかを判定するための .dtvi がありません。この構成は未検証のため\
             処理を中止しました。",
            "analyze で `.dtvi`(dtvindex build の出力)を生成してから再試行してください。",
        )
    })?;

    let has_open_gop_frame = dtvi.leading_frame_count() > 0
        || dtvi.frames.iter().any(|frame| frame.requires_earlier_rap());

    if has_open_gop_frame {
        return Err(UnsupportedInput::new(
            "入力はオープン GOP です。「S の同期サンプルからデコード順に E - S パケット取る」\
             規則が成立しないため処理を中止しました。",
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtvi::{DtviFrame, FLAG_LEADING_SAMPLE, FLAG_REQUIRES_EARLIER_RAP};
    use crate::order::{DecodeIdx, DisplayIdx};
    use mp4_atom::{Audio, Avc1, Dops, Edts, Elst, ElstEntry, Mdia, Minf, Opus, Stbl, Stsd, Trak};
    use std::collections::HashMap;
    use std::path::Path;

    const FIXTURE: &str = "tests/fixtures/sample.mp4";

    /// `stsd` に指定したコーデックだけを持つ、それ以外はデフォルトの `Trak`。
    ///
    /// `Trak::default()` を作ってからフィールドを代入すると
    /// `clippy::field_reassign_with_default` に引っかかるため、構造体更新構文で
    /// ネストした `Default` をそのまま使う。
    fn trak_with_codecs(codecs: Vec<Codec>) -> Trak {
        Trak {
            mdia: Mdia {
                minf: Minf {
                    stbl: Stbl {
                        stsd: Stsd { codecs },
                        ..Default::default()
                    },
                    ..Default::default()
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    fn video_trak() -> Trak {
        trak_with_codecs(vec![Codec::Avc1(Avc1::default())])
    }

    fn audio_trak() -> Trak {
        trak_with_codecs(vec![Codec::Opus(Opus {
            audio: Audio {
                data_reference_index: 1,
                channel_count: 2,
                sample_size: 16,
                sample_rate: 48_000u16.into(),
            },
            dops: Dops {
                output_channel_count: 2,
                pre_skip: 0,
                input_sample_rate: 48_000,
                output_gain: 0,
            },
            btrt: None,
        })])
    }

    /// 対象素材と同じ構成(映像1本 + 音声1本、edit list なし、stsd 1エントリ)の
    /// 合成 `Moov`。
    fn valid_moov() -> Moov {
        Moov {
            trak: vec![video_trak(), audio_trak()],
            ..Default::default()
        }
    }

    /// クローズド GOP(先行提示サンプルも早期 RAP 要求もない)な合成 `.dtvi`。
    fn closed_gop_dtvi() -> Dtvi {
        Dtvi {
            format_version: 1,
            header: HashMap::new(),
            frames: vec![DtviFrame {
                frame_number: DisplayIdx(0),
                sample_number: DecodeIdx(0),
                random_access_sample: DecodeIdx(0),
                file_offset: 0,
                pts: 0,
                dts: 0,
                duration: 1001,
                flags: 0,
            }],
        }
    }

    /// 実ファイルの `tests/fixtures/sample.mp4` があればそれを読んで使い、
    /// 無ければ合成 `Moov` にフォールバックする(生成は `tests/fixtures/gen.sh`)。
    fn valid_moov_from_fixture_or_synthetic() -> Moov {
        if Path::new(FIXTURE).exists() {
            crate::mp4io::read::read_moov(FIXTURE).expect("フィクスチャの moov を読めること")
        } else {
            valid_moov()
        }
    }

    #[test]
    fn accepts_normal_input() {
        let moov = valid_moov_from_fixture_or_synthetic();
        let dtvi = closed_gop_dtvi();
        assert!(check_supported(&moov, Some(&dtvi)).is_ok());
    }

    #[test]
    fn rejects_edit_list() {
        let mut moov = valid_moov();
        moov.trak[0].edts = Some(Edts {
            elst: Some(Elst {
                entries: vec![ElstEntry {
                    segment_duration: 1000,
                    media_time: Some(0),
                    media_rate: 1.into(),
                }],
            }),
        });

        let err = check_supported(&moov, Some(&closed_gop_dtvi())).unwrap_err();
        assert!(err.reason.contains("edit list"));
        assert!(err.suggestion.is_some());
    }

    #[test]
    fn rejects_multiple_stsd_entries() {
        let mut moov = valid_moov();
        moov.trak[0]
            .mdia
            .minf
            .stbl
            .stsd
            .codecs
            .push(Codec::Avc1(Avc1::default()));

        let err = check_supported(&moov, Some(&closed_gop_dtvi())).unwrap_err();
        assert!(err.reason.contains("stsd"));
    }

    #[test]
    fn rejects_open_gop_via_leading_sample_flag() {
        let moov = valid_moov();
        let mut dtvi = closed_gop_dtvi();
        dtvi.frames[0].flags |= FLAG_LEADING_SAMPLE;

        let err = check_supported(&moov, Some(&dtvi)).unwrap_err();
        assert!(err.reason.contains("オープン GOP"));
    }

    #[test]
    fn rejects_open_gop_via_requires_earlier_rap_flag() {
        let moov = valid_moov();
        let mut dtvi = closed_gop_dtvi();
        dtvi.frames[0].flags |= FLAG_REQUIRES_EARLIER_RAP;

        let err = check_supported(&moov, Some(&dtvi)).unwrap_err();
        assert!(err.reason.contains("オープン GOP"));
    }

    #[test]
    fn rejects_missing_dtvi() {
        let moov = valid_moov();
        let err = check_supported(&moov, None).unwrap_err();
        assert!(err.reason.contains(".dtvi"));
    }

    #[test]
    fn rejects_multiple_audio_tracks() {
        let mut moov = valid_moov();
        moov.trak.push(audio_trak());

        let err = check_supported(&moov, Some(&closed_gop_dtvi())).unwrap_err();
        assert!(err.reason.contains("音声トラック"));
    }

    #[test]
    fn rejects_subtitle_like_track() {
        let mut moov = valid_moov();
        // 映像でも音声でもないトラック(字幕などを想定した「その他」トラック)。
        moov.trak.push(Trak::default());

        let err = check_supported(&moov, Some(&closed_gop_dtvi())).unwrap_err();
        assert!(err.reason.contains("音声トラック"));
    }

    #[test]
    fn rejects_zero_video_tracks() {
        let mut moov = valid_moov();
        moov.trak.remove(0);

        let err = check_supported(&moov, Some(&closed_gop_dtvi())).unwrap_err();
        assert!(err.reason.contains("映像トラック"));
    }

    #[test]
    fn rejects_multiple_video_tracks() {
        let mut moov = valid_moov();
        moov.trak.push(video_trak());

        let err = check_supported(&moov, Some(&closed_gop_dtvi())).unwrap_err();
        assert!(err.reason.contains("映像トラック"));
    }

    #[test]
    fn display_formats_reason_and_suggestion() {
        let err = UnsupportedInput::with_suggestion("reason line", "suggestion line");
        let text = err.to_string();
        assert_eq!(text, "error: reason line\n       suggestion line");
    }

    #[test]
    fn display_formats_reason_only() {
        let err = UnsupportedInput::new("reason line");
        assert_eq!(err.to_string(), "error: reason line");
    }
}
