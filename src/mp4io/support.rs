//! 未対応の入力構成を早期に検出する。
//!
//! CLAUDE.md の方針: 未検証・未実装の構成は、静かに間違った出力を出すより
//! 明示的に落とす。ここでチェックする構成はどれも「エラーは出ないが結果が
//! 壊れる」可能性がある未検証パスであり、`--force` のような続行フラグは
//! 用意しない(誤った出力を作れる経路を残さないため)。
//!
//! 検出する5条件は docs/architecture.md の「未対応の入力」に対応する:
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
//!
//! ## `elst` と複数 `stsd` エントリを明示エラーのまま残す理由(#41 で調査済み)
//!
//! どちらも「対応する実装を書く」より「明示エラーを維持する」方針に決めた。
//! 根拠は docs/architecture.md の「未対応の入力」に書いてあるが、要点だけ:
//!
//! - `elst`: `ffmpeg`(既定設定、`-use_editlist 0` を付けない)で実際に elst 付き
//!   mp4 を作り、`check_supported` をバイパスして `write_mp4` に通す実験をした。
//!   結果、clone された `elst` の `segment_duration` が新しいトラック長(カット後、
//!   元の約2/3)を超えたまま出力され、`media_time` が新しい先頭の正当なフレーム
//!   を数フレーム分スキップした。「エラーは出ないが結果が壊れる」の実例であり、
//!   対応(削除して正規化 / 再計算)よりも明示エラー継続の方が安全と判断した。
//!   代わりに既存の回避策(ffmpeg での事前除去)を提示している
//!   ([`check_no_edit_list`] のエラーメッセージ)。この回避策はCRC32でパケットが
//!   ビット一致することを確認済み。
//! - `stsd` 複数エントリ: 対象素材(配信系トランスコード、エンコード設定固定)では
//!   発生しないと判断した。仮に発生した場合の正しい対応(サンプルごとに
//!   `sample_description_index` を保持する)は `mp4io/write.rs` の `stsc` 構築
//!   ロジックの変更を要し、このモジュール(未対応構成を早期に落とす役割)の
//!   スコープを超えるため、実装は別issue送りとし、ここでは明示エラーを維持する。

use mp4_atom::{Codec, Moov};

use crate::dtvi::Dtvi;
use crate::mp4io::read::is_audio_codec;

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
            Some(codec) if is_audio_codec(codec) => audio_count += 1,
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
///
/// #41 で調査済み: `elst` を `moov.clone()` で引き継いだまま `write_mp4` に通すと、
/// `segment_duration` が新しいトラック長を超えたまま残り、`media_time` が新しい
/// 先頭の正当なフレームを数フレーム分スキップする(実機で確認)。削除して先頭を
/// 0 に正規化する対応も、サンプル削除に合わせて再計算する対応も、対象素材
/// (elst を持たない配信系トランスコード)には需要がないため見送り、明示エラーを
/// 維持する。詳細は docs/architecture.md の「未対応の入力」。
fn check_no_edit_list(moov: &Moov) -> Result<(), UnsupportedInput> {
    let has_elst = moov
        .trak
        .iter()
        .any(|trak| trak.edts.as_ref().is_some_and(|edts| edts.elst.is_some()));

    if has_elst {
        return Err(UnsupportedInput::with_suggestion(
            "入力に edit list (elst) があります。この構成は対応していないため処理を中止しました\
             (#41: サンプル削除後に edit list のタイムラインが不整合になることを確認済み)。",
            "ffmpeg で edit list を除去してから再試行してください(動作確認済み):\n         \
             ffmpeg -i IN.mp4 -c copy -use_editlist 0 -movflags +faststart OUT.mp4",
        ));
    }

    Ok(())
}

/// どのトラックの `stsd` も1エントリであることを確認する。
///
/// #41 で調査済み: 対象素材(配信系トランスコード、エンコード設定固定)では複数
/// エントリは発生しないと判断した。仮に発生した場合の正しい対応(サンプルごとに
/// `sample_description_index` を保持する)は `mp4io/write.rs` の `stsc` 構築
/// ロジックの変更を要し、このファイルの役割(早期の明示エラー化)を超えるため、
/// 実装は別issue送りとし、ここでは明示エラーを維持する。ffmpeg の無劣化 remux
/// では複数エントリの原因(実体のパラメータ差異)自体を解消できないため、elst の
/// ような事前除去の回避策は提示できない。
fn check_single_stsd_entry(moov: &Moov) -> Result<(), UnsupportedInput> {
    let has_multiple_entries = moov
        .trak
        .iter()
        .any(|trak| trak.mdia.minf.stbl.stsd.codecs.len() > 1);

    if has_multiple_entries {
        return Err(UnsupportedInput::new(
            "stsd に複数のサンプルエントリを持つトラックがあります。この構成は対応していない\
             ため処理を中止しました(#41: sample_description_index を1固定にしているため。\
             対応にはサンプルごとの index 保持が必要で、write.rs の変更を要するため見送り)。",
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
    use mp4_atom::esds::{DecoderConfig, DecoderSpecific, EsDescriptor, SLConfig};
    use mp4_atom::{
        Audio, Avc1, Dops, Edts, Elst, ElstEntry, Esds, Mdia, Minf, Mp4a, Opus, Stbl, Stsd, Trak,
    };
    use std::collections::HashMap;
    use std::path::Path;

    // cwd 非依存にする（`external::tests` がプロセスの cwd を一時的に変えるため）。
    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.mp4");

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

    fn audio_header() -> Audio {
        Audio {
            data_reference_index: 1,
            channel_count: 2,
            sample_size: 16,
            sample_rate: 48_000u16.into(),
        }
    }

    fn audio_trak() -> Trak {
        trak_with_codecs(vec![Codec::Opus(Opus {
            audio: audio_header(),
            dops: Dops {
                output_channel_count: 2,
                pre_skip: 0,
                input_sample_rate: 48_000,
                output_gain: 0,
            },
            btrt: None,
        })])
    }

    fn aac_trak() -> Trak {
        trak_with_codecs(vec![Codec::Mp4a(Mp4a {
            audio: audio_header(),
            esds: Esds {
                es_desc: EsDescriptor {
                    es_id: 0,
                    dec_config: DecoderConfig {
                        object_type_indication: 0x40,
                        stream_type: 5,
                        up_stream: 0,
                        buffer_size_db: 0u32.try_into().unwrap(),
                        max_bitrate: 0,
                        avg_bitrate: 0,
                        dec_specific: DecoderSpecific {
                            profile: 2,
                            freq_index: 3,
                            chan_conf: 2,
                        },
                    },
                    sl_config: SLConfig {},
                },
            },
            btrt: None,
            taic: None,
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
    fn accepts_aac_as_single_audio_track() {
        let moov = Moov {
            trak: vec![video_trak(), aac_trak()],
            ..Default::default()
        };
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
