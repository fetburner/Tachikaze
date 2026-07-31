//! `prepare` サブコマンド: elst 除去と字幕抽出を1か所に集約する。
//!
//! `cut` は elst 付き入力と字幕トラック付き入力を明示エラーで拒否する
//! (`mp4io::support::check_no_edit_list` / `check_track_counts`、#41 の調査結果。
//! この方針は変えない)。回避策の前処理はこれまで `scripts/tachikaze-cmcut` の
//! `prepare_input`(bash + python3)にあったが、字幕は抽出されずに `-map` 省略の
//! 既定選択で1本引き継がれ、結局 `check_track_counts` に弾かれていた。`auto`
//! (#62、未着手)とラッパーが別々に前処理を持つと elst の扱いと字幕の抽出元が
//! 必ずずれるため、同じコードをここ1か所に置く。
//!
//! # なぜ「除去」ではなく「除去して字幕はここで抽出」なのか
//!
//! `use_editlist` は muxer 専用オプション(`ffmpeg -h muxer=mp4` で `E..........`)。
//! demuxer は既定で edit list を適用して読む(`ignore_editlist=false`)。つまり
//! elst 除去とは「読みで畳み込んで、書かない」ことであり、strip 後のファイルの
//! 時間軸は元ファイルとずれる可能性がある(ずれの量は別issue #60が実測する。
//! ここでは触れるだけに留める)。字幕を元ファイルから別に抽出すると、そのずれが
//! 字幕にだけ乗ってしまう。だから elst 除去と字幕抽出は**必ず同じ ffmpeg 呼び出し**
//! で行い、同じ時間軸のずれ(あれば)を両方が等しく受ける。
//!
//! # 罠: strip 後、自己検証5は「元ファイル」を見誤る
//!
//! `cut` の自己検証5(docs/architecture.md「自己検証」節)は「元ファイルと出力で
//! 映像pts−音声ptsが保たれているか」を見る。`prepare` が作った
//! `input_prepared.mp4` を `cut` の入力として渡すと、この検証は
//! **strip 後のファイルを「元ファイル」として** 比較する。elst 除去そのものが
//! 音声・映像のタイムスタンプを変えていた場合(#60 が実測するまで未検証)、
//! 検証は「strip 後ファイルと最終出力の一致」しか見ておらず、「真の元ファイルと
//! 最終出力の一致」は見ていない。つまりこの検証は
//! **strip によるタイムスタンプのずれを検出できない穴**を持つ。elst 除去の
//! CRC32一致([`mp4io::support`] のドキュメント、docs/architecture.md「未対応の
//! 入力」)もペイロード(パケットの中身)しか見ておらず、タイムスタンプが保たれる
//! 根拠にはならない。
//!
//! # 出力先
//!
//! 入力の隣には何も作らない。すべて [`workdir::prepared_input_path`] /
//! [`workdir::subs_path`] が指す、入力ごとの XDG キャッシュディレクトリ
//! (`workdir` モジュールの doc comment参照)に書く。800MB 級の
//! `input_prepared.mp4` が残ることになるが、自動削除はしない
//! (`analyze` の中間ファイルと同じ「消えても再生成できるキャッシュ」という
//! 位置づけ。`cut --dtvi` が `analyze` のキャッシュを自動的に見つけるのと同じ
//! 発想で、将来 `cut` 側からもこのキャッシュを再利用できる余地を残す)。
//!
//! # 再実行時の扱い
//!
//! `prepare` を同じ入力に対して再実行すると、前処理が必要な限り
//! **毎回 ffmpeg を再実行して上書きする**(既存の `input_prepared.mp4` の
//! 有無やタイムスタンプを見て「作り直すかどうか」を判断することはしない)。
//! 理由: `analyze` の中間ファイル(`work_dir` のdoc comment)も同じ方針で、
//! `dtvindex` / `chapter_exe` / `join_logo_scp` の出力を毎回上書きしている。
//! 入力ファイルが同じパスのまま更新されている可能性を否定できない以上、
//! 「キャッシュが最新かどうか」を判定するより「常に作り直す」ほうが安全
//! (誤って古い前処理済みファイルを使い続けるほうが、CM カットの結果が
//! 静かに壊れるという CLAUDE.md の最優先事項に反する)。800MB 級のコピーが
//! 毎回発生するトレードオフは許容する。ただし「入力ごとに1回 `prepare` を
//! 呼ぶ」以上の頻度(例えば将来の `auto` が cut のたびに毎回呼ぶ設計)には
//! しないこと。
//!
//! # 字幕抽出の形式
//!
//! 字幕トラックのコーデックは `mp4-atom` の `Codec` から判定する
//! ([`SubtitleFormat`])。`Tx3g`(mov_text)/`Wvtt`(WebVTT)はプレーンテキスト
//! 主体の字幕形式なので SRT に変換する。それ以外(`Codec::Unknown` 等。
//! 代表例としてARIB字幕がffmpegに解釈された場合の私的トラックを想定)は
//! 色・位置などのスタイル情報を落とさないよう ASS に変換する。
//! **未検証**: 実際にどの Codec が来るか、ffmpeg がデコードできるかどうかは
//! 確認していない(手元にARIB字幕付きmp4の実フィクスチャが無いため)。
//! デコードに失敗した場合は ffmpeg 自体がエラー終了し、
//! [`external::run`] がコマンドラインと stderr 末尾を含むエラーを返す。
//!
//! 元がARIB字幕由来の`.ass`サイドカーとして別に存在する場合、mp4内の
//! `mov_text` から抽出したものより情報量が多いことがある。`--subs PATH`
//! (`cli::Commands::Prepare::subs`)で外部ファイルを優先的に使えるように
//! しており、この場合 mp4 内蔵の字幕トラックの抽出(ffmpeg呼び出し)は行わない
//! (ただし elst 除去や、mp4 内蔵字幕トラック自体の除去は引き続き行う)。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use mp4_atom::{Codec, Moov};

use crate::mp4io::read::is_audio_codec;
use crate::{external, tools, workdir};

/// 字幕トラックのコーデックから判定した、抽出に使う字幕形式。
///
/// `mp4_atom::Codec` は `#[non_exhaustive]` であり、判定は
/// [`subtitle_format_in`] 1か所に集約する。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubtitleFormat {
    /// `Tx3g`(mov_text)。
    Tx3g,
    /// `Wvtt`(WebVTT)。
    Wvtt,
    /// 上記以外(代表例: ARIB字幕をffmpegが解釈した場合の私的トラック)。
    Unknown,
}

impl SubtitleFormat {
    /// サイドカーファイルの拡張子(`.` なし)。
    fn extension(self) -> &'static str {
        match self {
            SubtitleFormat::Tx3g | SubtitleFormat::Wvtt => "srt",
            SubtitleFormat::Unknown => "ass",
        }
    }

    /// ffmpeg の `-c:s` に渡すエンコーダ名。
    fn ffmpeg_encoder(self) -> &'static str {
        match self {
            SubtitleFormat::Tx3g | SubtitleFormat::Wvtt => "srt",
            SubtitleFormat::Unknown => "ass",
        }
    }

    /// 人間向けの表示名(ログ用)。
    fn label(self) -> &'static str {
        match self {
            SubtitleFormat::Tx3g => "Tx3g(mov_text)",
            SubtitleFormat::Wvtt => "Wvtt(WebVTT)",
            SubtitleFormat::Unknown => "Unknown(未知の字幕コーデック)",
        }
    }
}

/// `moov` から読み取った、前処理の要否判定に必要な情報。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MoovInspection {
    /// `elst`(edit list)を持つトラックがあるか。
    pub has_edit_list: bool,
    /// 字幕トラックがあれば、そのコーデックから判定した抽出形式。
    pub subtitle: Option<SubtitleFormat>,
}

/// `elst`(edit list)を持つトラックがあるかを判定する。
///
/// `mp4io::support::check_no_edit_list` と同じ判定条件(`trak.edts.elst`の有無)
/// だが、あちらは「エラーにする」ためのチェック、こちらは「除去が必要かどうか」
/// の判定なので別関数として持つ(役割が違う: support.rs は明示エラー化に特化した
/// モジュールで、prepare 固有の判断をそこに混ぜたくない)。
fn has_edit_list(moov: &Moov) -> bool {
    moov.trak
        .iter()
        .any(|trak| trak.edts.as_ref().is_some_and(|edts| edts.elst.is_some()))
}

/// トラックの `stsd` の先頭エントリから `Codec` を取り出す。
fn track_codec(trak: &mp4_atom::Trak) -> Option<&Codec> {
    trak.mdia.minf.stbl.stsd.codecs.first()
}

/// 映像でも音声でもない最初のトラックを字幕トラックとみなし、その `Codec` から
/// [`SubtitleFormat`] を判定する。
///
/// 対象素材は映像1本+音声1本+(あれば)字幕1本を想定しており、字幕が2本以上ある
/// 構成は元々 `check_track_counts` が拒否する対象なので、ここでは最初の1本だけを
/// 見れば十分(`mp4io::read::find_video_track` / `find_audio_track` と同じ
/// 「最初の1本を返す」設計)。
fn subtitle_format_in(moov: &Moov) -> Option<SubtitleFormat> {
    moov.trak.iter().find_map(|trak| {
        let codec = track_codec(trak)?;
        match codec {
            Codec::Avc1(_) => None,
            codec if is_audio_codec(codec) => None,
            Codec::Tx3g(_) => Some(SubtitleFormat::Tx3g),
            Codec::Wvtt(_) => Some(SubtitleFormat::Wvtt),
            _ => Some(SubtitleFormat::Unknown),
        }
    })
}

/// `moov` を検査し、elst の有無と字幕トラックの形式を判定する。
pub fn inspect_moov(moov: &Moov) -> MoovInspection {
    MoovInspection {
        has_edit_list: has_edit_list(moov),
        subtitle: subtitle_format_in(moov),
    }
}

/// `prepare` の実行結果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PrepareOutcome {
    /// `cut` にそのまま渡せる映像+音声のパス。前処理が不要なら入力そのもの、
    /// 必要なら [`workdir::prepared_input_path`] が指すキャッシュ内のファイル。
    pub media_path: PathBuf,
    /// 字幕サイドカー(mp4 内蔵の字幕トラックから抽出、または `--subs` で
    /// 指定された外部ファイル)。どちらも無ければ `None`。
    pub subtitle_path: Option<PathBuf>,
    /// elst 除去や字幕トラック除去のために ffmpeg を実際に実行したか。
    pub ran_ffmpeg: bool,
    /// 入力に elst があったか(ログ・呼び出し元の判断用)。
    pub had_edit_list: bool,
}

/// パスを `&str` として取り出す。UTF-8 でないパスは非対応として扱う
/// (`analyze.rs` の同名関数と同じ役割。モジュールごとに小さく持つ方針は
/// `analyze.rs` に合わせている)。
fn require_utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow::anyhow!("パスが UTF-8 として扱えません: {}", path.display()))
}

/// `prepare` 本体。
///
/// - `input`: 入力 mp4。
/// - `tool_dir`: `--tool-dir`(ffmpeg の探索に使う)。
/// - `external_subs`: `--subs PATH`。指定時は mp4 内蔵の字幕トラックを
///   抽出せず、このパスをそのまま [`PrepareOutcome::subtitle_path`] にする。
///
/// 前処理(elst 除去・字幕トラック除去・字幕抽出)が何も要らない場合は ffmpeg を
/// 一切呼ばず、`input` をそのまま `media_path` として返す(無駄な800MB級の
/// コピーを作らない)。
pub fn run(
    input: &Path,
    tool_dir: Option<&Path>,
    external_subs: Option<&Path>,
) -> Result<PrepareOutcome> {
    let moov = crate::mp4io::read::read_moov(input)
        .with_context(|| format!("入力 mp4 の読み込みに失敗しました: {}", input.display()))?;
    let inspection = inspect_moov(&moov);

    let needs_strip = inspection.has_edit_list || inspection.subtitle.is_some();

    if !needs_strip {
        eprintln!(
            "[prepare] 前処理不要: edit list も字幕トラックもありません。入力をそのまま使えます: {}",
            input.display()
        );
        return Ok(PrepareOutcome {
            media_path: input.to_path_buf(),
            subtitle_path: external_subs.map(Path::to_path_buf),
            ran_ffmpeg: false,
            had_edit_list: false,
        });
    }

    // ffmpeg はここまで来て初めて必要になる(elst 除去または字幕トラック除去が
    // 要るとわかった時点)。見つからない場合は、何のために必要かと手動で行う
    // 場合のコマンド例を添えて停止する。
    let ffmpeg_path = tools::resolve_tool(tool_dir, tools::FFMPEG).with_context(|| {
        format!(
            "prepare: 入力に edit list または字幕トラックがあり、除去に ffmpeg が必要です。\n\
             手動で行う場合の例(字幕が無い場合):\n  \
             ffmpeg -i {} -map 0:v:0 -map 0:a:0 -c copy -use_editlist 0 -movflags +faststart <OUT>.mp4",
            input.display()
        )
    })?;

    let prepared_path = workdir::prepared_input_path(input)
        .with_context(|| format!("キャッシュパスの解決に失敗しました: {}", input.display()))?;
    let cache_dir = prepared_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("キャッシュパスに親ディレクトリがありません"))?
        .to_path_buf();
    fs::create_dir_all(&cache_dir).with_context(|| {
        format!(
            "キャッシュディレクトリの作成に失敗しました: {}",
            cache_dir.display()
        )
    })?;

    if inspection.has_edit_list {
        eprintln!(
            "[prepare] edit list (elst) を検出しました。除去して書き出します: {}",
            prepared_path.display()
        );
    }

    let mut args: Vec<String> = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-y".into(),
        "-i".into(),
        require_utf8(input)?.into(),
        "-map".into(),
        "0:v:0".into(),
        "-map".into(),
        "0:a:0".into(),
        "-c".into(),
        "copy".into(),
        "-use_editlist".into(),
        "0".into(),
        "-movflags".into(),
        "+faststart".into(),
        require_utf8(&prepared_path)?.into(),
    ];

    let subtitle_path = match (inspection.subtitle, external_subs) {
        (Some(format), Some(external)) => {
            eprintln!(
                "[prepare] 字幕トラック({})を検出しましたが、--subs で指定された外部ファイルを\
                 優先します(mp4 内蔵の字幕トラックは破棄): {}",
                format.label(),
                external.display()
            );
            Some(external.to_path_buf())
        }
        (Some(format), None) => {
            let subs_out = workdir::subs_path(input, format.extension()).with_context(|| {
                format!(
                    "字幕サイドカーのキャッシュパスの解決に失敗しました: {}",
                    input.display()
                )
            })?;
            eprintln!(
                "[prepare] 字幕トラック({})を検出しました。抽出します: {}",
                format.label(),
                subs_out.display()
            );
            args.extend([
                "-map".into(),
                "0:s:0".into(),
                "-c:s".into(),
                format.ffmpeg_encoder().into(),
                require_utf8(&subs_out)?.into(),
            ]);
            Some(subs_out)
        }
        (None, Some(external)) => {
            eprintln!(
                "[prepare] 字幕トラックはありませんが、--subs で指定された外部ファイルを使います: {}",
                external.display()
            );
            Some(external.to_path_buf())
        }
        (None, None) => None,
    };

    let args_ref: Vec<&str> = args.iter().map(String::as_str).collect();
    external::run(require_utf8(&ffmpeg_path)?, &args_ref, &cache_dir)?;

    eprintln!(
        "[prepare] 前処理済みファイルはキャッシュに残ります(自動削除しません): {}",
        prepared_path.display()
    );

    Ok(PrepareOutcome {
        media_path: prepared_path,
        subtitle_path,
        ran_ffmpeg: true,
        had_edit_list: inspection.has_edit_list,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use mp4_atom::{
        Avc1, Dops, Edts, Elst, ElstEntry, Mdia, Minf, Opus, PlainText, Stbl, Stsd, Trak, Tx3g,
        VttC, Wvtt,
    };

    fn trak_with_codec(codec: Codec) -> Trak {
        Trak {
            mdia: Mdia {
                minf: Minf {
                    stbl: Stbl {
                        stsd: Stsd {
                            codecs: vec![codec],
                        },
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
        trak_with_codec(Codec::Avc1(Avc1::default()))
    }

    fn audio_trak() -> Trak {
        trak_with_codec(Codec::Opus(Opus {
            audio: mp4_atom::Audio {
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
        }))
    }

    #[test]
    fn inspect_moov_reports_no_edit_list_and_no_subtitle_for_plain_input() {
        let moov = Moov {
            trak: vec![video_trak(), audio_trak()],
            ..Default::default()
        };
        let inspection = inspect_moov(&moov);
        assert!(!inspection.has_edit_list);
        assert_eq!(inspection.subtitle, None);
    }

    #[test]
    fn inspect_moov_detects_edit_list() {
        let mut moov = Moov {
            trak: vec![video_trak(), audio_trak()],
            ..Default::default()
        };
        moov.trak[0].edts = Some(Edts {
            elst: Some(Elst {
                entries: vec![ElstEntry {
                    segment_duration: 1000,
                    media_time: Some(0),
                    media_rate: 1.into(),
                }],
            }),
        });

        let inspection = inspect_moov(&moov);
        assert!(inspection.has_edit_list);
    }

    #[test]
    fn inspect_moov_detects_tx3g_subtitle() {
        let moov = Moov {
            trak: vec![
                video_trak(),
                audio_trak(),
                trak_with_codec(Codec::Tx3g(Tx3g::default())),
            ],
            ..Default::default()
        };
        let inspection = inspect_moov(&moov);
        assert_eq!(inspection.subtitle, Some(SubtitleFormat::Tx3g));
    }

    #[test]
    fn inspect_moov_detects_wvtt_subtitle() {
        let wvtt = Wvtt {
            plaintext: PlainText {
                data_reference_index: 1,
            },
            config: VttC {
                config: "WEBVTT\n".into(),
            },
            label: None,
            btrt: None,
        };
        let moov = Moov {
            trak: vec![
                video_trak(),
                audio_trak(),
                trak_with_codec(Codec::Wvtt(wvtt)),
            ],
            ..Default::default()
        };
        let inspection = inspect_moov(&moov);
        assert_eq!(inspection.subtitle, Some(SubtitleFormat::Wvtt));
    }

    #[test]
    fn inspect_moov_treats_other_non_av_track_as_unknown_subtitle() {
        // `Trak::default()` は `stsd.codecs` が空で `track_codec` が `None` を
        // 返すため対象外になる(コーデック情報が無いトラックは判定しようがない)。
        // ここでは「映像でも音声でも Tx3g/Wvtt でもない実在のコーデック」を持つ
        // トラックとして、`Codec::Unknown` (代表例: TTML の `stpp`) を使う。
        let moov = Moov {
            trak: vec![
                video_trak(),
                audio_trak(),
                trak_with_codec(Codec::Unknown(mp4_atom::FourCC::new(b"stpp"))),
            ],
            ..Default::default()
        };
        let inspection = inspect_moov(&moov);
        assert_eq!(inspection.subtitle, Some(SubtitleFormat::Unknown));
    }

    #[test]
    fn subtitle_format_extension_and_encoder_match() {
        assert_eq!(SubtitleFormat::Tx3g.extension(), "srt");
        assert_eq!(SubtitleFormat::Wvtt.extension(), "srt");
        assert_eq!(SubtitleFormat::Unknown.extension(), "ass");
        assert_eq!(SubtitleFormat::Tx3g.ffmpeg_encoder(), "srt");
        assert_eq!(SubtitleFormat::Unknown.ffmpeg_encoder(), "ass");
    }

    /// `run()` 自体の単体テストは `mp4-atom` での判定ロジック
    /// (`inspect_moov`)を主眼にしている。「前処理不要なら入力をそのまま返す」
    /// 「elst / 字幕を検出したら ffmpeg を呼ぶ」という分岐は実際の mp4 ファイルと
    /// ffmpeg が要る(合成 `Moov` では `read_moov` を通せない)ため、
    /// `tests/prepare_e2e.rs` の統合テストで確認する。ここでは
    /// 存在しない/mp4 として読めない入力を渡した場合に `read_moov` のエラーが
    /// そのまま伝播することだけを確認する。
    #[test]
    fn run_propagates_read_moov_error_for_invalid_input() {
        let dir = std::env::temp_dir().join(format!(
            "tachikaze-prepare-test-invalid-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let input = dir.join("IN.mp4");
        fs::write(&input, b"not a real mp4").unwrap();

        let err = run(&input, None, None).expect_err("mp4 として読めない入力はエラーになるはず");
        assert!(err
            .to_string()
            .contains("入力 mp4 の読み込みに失敗しました"));

        fs::remove_dir_all(&dir).ok();
    }
}
