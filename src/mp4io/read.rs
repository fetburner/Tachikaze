//! mp4 の `moov` を取り出し、映像/音声トラックを識別する読み込み側。
//!
//! `mdat` は読み飛ばすため、ファイル全体をメモリに載せずに `moov` だけを取得できる
//! （検証済みコード: docs/mp4-atom.md「トップレベルから moov を取り出す」）。

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

use mp4_atom::{Atom, Codec, Header, Moov, ReadAtom, ReadFrom, Stbl, StszSamples, Trak};

/// トラックごとに異なりうる時間の基準単位。
///
/// 映像と音声で timescale は別々なので、トラックごとに保持する
/// （前提: 対象素材は H.264 + 音声（Opus / AAC など）で、両者の timescale は一致しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackInfo {
    pub timescale: u32,
}

/// mp4 ファイルパスから `moov` アトムを取り出す。
///
/// トップレベルのアトムを順に読み、`moov` に到達するまで他のアトム（特に巨大な
/// `mdat`）はシークして読み飛ばす。`Header::size` が `None`（サイズ 0 = ファイル
/// 末尾まで）のアトムは、その時点で `moov` が見つかっていないことを意味するため
/// エラーとして扱う。
pub fn read_moov<P: AsRef<Path>>(path: P) -> std::result::Result<Moov, anyhow::Error> {
    let mut r = BufReader::new(File::open(path)?);

    loop {
        let header = Header::read_from(&mut r)?;

        if header.kind == Moov::KIND {
            return Ok(Moov::read_atom(&header, &mut r)?);
        }

        match header.size {
            Some(size) => {
                r.seek(SeekFrom::Current(size as i64))?;
            }
            None => {
                // サイズ 0 は「ファイル末尾まで」を意味する。moov に到達する前に
                // このようなアトム（通常は末尾の mdat）に当たった場合、moov は
                // このファイルに存在しない。
                anyhow::bail!(
                    "moov atom not found before size-to-eof atom '{}'",
                    header.kind
                );
            }
        }
    }
}

/// トラックの `stsd` に入っている先頭のサンプルエントリから `Codec` を取り出す。
///
/// 対象素材の `stsd` は 1 トラックにつき 1 エントリのみを想定している。
fn track_codec(trak: &Trak) -> Option<&Codec> {
    trak.mdia.minf.stbl.stsd.codecs.first()
}

/// `Codec` が音声コーデックかどうかを判定する（音声トラック識別の唯一の基準）。
///
/// カット処理は「ソース上の DTS から最近傍パケットを引き当ててビットコピーする」
/// だけでコーデックに依存しない（docs/lossless-cut.md「音声の扱い」）。そのため
/// 特定コーデックに限定せず、`mp4-atom` 0.14 が `Codec` として認識する音声系すべてを
/// 音声トラックとして受け入れる。対応一覧は docs/mp4-atom.md の音声 Codec 表。
///
/// `Codec` は `#[non_exhaustive]` なので、未知の variant や映像・字幕・`Unknown` は
/// `_ => false` で音声に数えない。read.rs / support.rs / commands.rs で音声判定が
/// 二重定義にならないよう、判定はこの関数 1 か所に集約する。
pub fn is_audio_codec(codec: &Codec) -> bool {
    matches!(
        codec,
        // 圧縮音声
        Codec::Opus(_)
            | Codec::Mp4a(_)
            | Codec::Flac(_)
            | Codec::Ac3(_)
            | Codec::Eac3(_)
            | Codec::Samr(_)
            // 非圧縮 / QuickTime 系 PCM
            | Codec::Ipcm(_)
            | Codec::Fpcm(_)
            | Codec::Sowt(_)
            | Codec::Twos(_)
            | Codec::Lpcm(_)
            | Codec::In24(_)
            | Codec::In32(_)
            | Codec::Fl32(_)
            | Codec::Fl64(_)
            | Codec::S16l(_)
    )
}

/// トラックの `mdhd` から timescale を取り出す。
fn track_info(trak: &Trak) -> TrackInfo {
    TrackInfo {
        timescale: trak.mdia.mdhd.timescale,
    }
}

/// `moov` から映像トラック（`Codec::Avc1`）を 1 本だけ見つける。
///
/// 複数本存在する場合は最初に見つかったものを返す（対象素材は映像 1 本を想定）。
pub fn find_video_track(moov: &Moov) -> Option<(&Trak, TrackInfo)> {
    moov.trak.iter().find_map(|trak| match track_codec(trak) {
        Some(Codec::Avc1(_)) => Some((trak, track_info(trak))),
        _ => None,
    })
}

/// `moov` から音声トラック（[`is_audio_codec`] が真になる Codec）を 1 本だけ見つける。
///
/// 複数本存在する場合は最初に見つかったものを返す（対象素材は音声 1 本を想定。本数の
/// 検証は `support::check_track_counts` が担い、2 本以上は明示エラーにする）。
pub fn find_audio_track(moov: &Moov) -> Option<(&Trak, TrackInfo)> {
    moov.trak.iter().find_map(|trak| match track_codec(trak) {
        Some(codec) if is_audio_codec(codec) => Some((trak, track_info(trak))),
        _ => None,
    })
}

/// `stbl` から復元した 1 サンプルの情報。
///
/// `duration` と `cts_offset` はどちらも整数で意味が異なるため、タプルではなく
/// フィールド名を持つ構造体にしている（取り違えても型では気付けない値を分離する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SampleInfo {
    /// ファイル先頭からのバイトオフセット。
    pub file_offset: u64,
    /// サンプルのバイト数。
    pub size: u32,
    /// `stts` から得られる、デコード順での表示時間（timescale 単位）。
    pub duration: u32,
    /// `ctts` から得られる、デコード順から表示順への補正（timescale 単位、負値あり）。
    pub cts_offset: i64,
    /// 同期サンプル（IDR 等）かどうか。`stss` が無いトラック（音声など）は全て true。
    pub is_sync: bool,
}

/// `stbl`（サンプルテーブル）から、デコード順のサンプル一覧を復元する。
///
/// `stsz`/`stsc`/`stco`(or `co64`)/`stts`/`ctts`/`stss` を 1 パスずつ読み、各サンプルの
/// `(file_offset, size, duration, cts_offset, is_sync)` を組み立てる
/// （検証済みコード: docs/mp4-atom.md「サンプル表の復元」）。
///
/// `stsc` を展開する際、チャンク先頭のオフセットに前サンプルのサイズを積算していく
/// ことで、各サンプルのオフセットを O(n) で求める（内側ループで毎回積算し直す
/// O(n^2) 実装を避けている）。
///
/// # 入力の検証
///
/// サンプル表の `u32` は信頼しない。確保・展開ループの**前**に次を確認し、
/// 失敗時はパニックではなく `Err` を返す（破損／悪意ある MP4 で OOM しないため）。
///
/// - `stsz` の件数・サイズ（および `count * size` / サイズ総和）が `file_len` 以下
/// - `stts` / `ctts` の `sample_count` 合計が `stsz` の総数と一致（一致後だけ展開）
///
/// `file_len` は入力ファイルの実バイト長（それ以上に厳しい検証済み mdat 範囲でも可）。
/// マジックなメモリ上限は持ち込まず、mdat に収まらない表を定義上あり得ないとして弾く。
///
/// `stsc` の `first_chunk` 検証（wrap / 静かな誤オフセット防止）は本関数の件数・サイズ
/// 検証とは別関心として後続で扱う。
pub fn samples(stbl: &Stbl, file_len: u64) -> anyhow::Result<Vec<SampleInfo>> {
    let sizes = validate_and_collect_sizes(&stbl.stsz.samples, file_len)?;
    let total = sizes.len();

    let chunk_offsets: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (Some(stco), _) => stco.entries.iter().map(|&o| o as u64).collect(),
        (_, Some(co64)) => co64.entries.clone(),
        _ => vec![],
    };

    let durations = expand_stts(&stbl.stts.entries, total)?;
    let cts_offsets = expand_ctts(stbl.ctts.as_ref(), total)?;

    // stss が無いトラック（音声など）は全サンプルが同期扱い。
    // stss は 1 始まりのサンプル番号を持つ。
    let all_sync = stbl.stss.is_none();
    let sync_samples: HashSet<u32> = stbl
        .stss
        .as_ref()
        .map(|stss| stss.entries.iter().copied().collect())
        .unwrap_or_default();

    // stsc をチャンクごとに展開しながら、チャンク先頭オフセットに直前サンプルの
    // サイズを積算してオフセットを求める。内側ループが無いため O(n)。
    let mut out = Vec::with_capacity(total);
    let mut sample_index = 0usize;
    let stsc_entries = &stbl.stsc.entries;

    for (i, entry) in stsc_entries.iter().enumerate() {
        let last_chunk = match stsc_entries.get(i + 1) {
            Some(next) => next.first_chunk - 1,
            None => chunk_offsets.len() as u32,
        };

        for chunk in entry.first_chunk..=last_chunk {
            let chunk_index = chunk as usize - 1;
            let mut offset = chunk_offsets.get(chunk_index).copied().unwrap_or(0);

            for _ in 0..entry.samples_per_chunk {
                if sample_index >= total {
                    break;
                }

                let size = sizes[sample_index];
                let sample_number = sample_index as u32 + 1;
                out.push(SampleInfo {
                    file_offset: offset,
                    size,
                    duration: durations[sample_index],
                    cts_offset: cts_offsets[sample_index],
                    is_sync: all_sync || sync_samples.contains(&sample_number),
                });

                offset += u64::from(size);
                sample_index += 1;
            }
        }
    }

    Ok(out)
}

/// `stsz` をファイル長で束縛し、確保前に件数・サイズを検証する。
fn validate_and_collect_sizes(samples: &StszSamples, file_len: u64) -> anyhow::Result<Vec<u32>> {
    match samples {
        StszSamples::Identical { count, size } => {
            // size=0 でも巨大 count を許さない（件数自体をファイル長以下に束縛）。
            if u64::from(*count) > file_len {
                anyhow::bail!(
                    "stsz sample count ({count}) が入力ファイル長 ({file_len}) を超えています"
                );
            }
            if *count > 0 {
                if u64::from(*size) > file_len {
                    anyhow::bail!(
                        "stsz sample size ({size}) が入力ファイル長 ({file_len}) を超えています"
                    );
                }
                let total_bytes =
                    u64::from(*count)
                        .checked_mul(u64::from(*size))
                        .ok_or_else(|| {
                            anyhow::anyhow!(
                                "stsz count ({count}) * size ({size}) が u64 を overflow します"
                            )
                        })?;
                if total_bytes > file_len {
                    anyhow::bail!(
                        "stsz の合計サイズ ({total_bytes}) が入力ファイル長 ({file_len}) を超えています"
                    );
                }
            }
            let count_usize = usize::try_from(*count).map_err(|_| {
                anyhow::anyhow!("stsz sample count ({count}) が usize に収まりません")
            })?;
            Ok(vec![*size; count_usize])
        }
        StszSamples::Different { sizes } => {
            let count = sizes.len() as u64;
            if count > file_len {
                anyhow::bail!(
                    "stsz sample count ({count}) が入力ファイル長 ({file_len}) を超えています"
                );
            }
            let mut total_bytes: u64 = 0;
            for (i, &size) in sizes.iter().enumerate() {
                if u64::from(size) > file_len {
                    anyhow::bail!(
                        "stsz sample[{i}] size ({size}) が入力ファイル長 ({file_len}) を超えています"
                    );
                }
                total_bytes = total_bytes.checked_add(u64::from(size)).ok_or_else(|| {
                    anyhow::anyhow!("stsz のサイズ総和が u64 を overflow します（sample {i}）")
                })?;
            }
            if total_bytes > file_len {
                anyhow::bail!(
                    "stsz の合計サイズ ({total_bytes}) が入力ファイル長 ({file_len}) を超えています"
                );
            }
            Ok(sizes.clone())
        }
    }
}

/// `stts` の `sample_count` 合計が `total` と一致することを確認してから展開する。
fn expand_stts(entries: &[mp4_atom::SttsEntry], total: usize) -> anyhow::Result<Vec<u32>> {
    let sum = sum_sample_counts(entries.iter().map(|e| e.sample_count), "stts")?;
    if sum != total as u64 {
        anyhow::bail!(
            "stts の sample_count 合計 ({sum}) が stsz のサンプル数 ({total}) と一致しません"
        );
    }
    let mut durations = Vec::with_capacity(total);
    for entry in entries {
        for _ in 0..entry.sample_count {
            durations.push(entry.sample_delta);
        }
    }
    Ok(durations)
}

/// `ctts` の `sample_count` 合計が `total` と一致することを確認してから展開する。
/// `ctts` が無い場合は全 0。
fn expand_ctts(ctts: Option<&mp4_atom::Ctts>, total: usize) -> anyhow::Result<Vec<i64>> {
    let mut cts_offsets = vec![0i64; total];
    let Some(ctts) = ctts else {
        return Ok(cts_offsets);
    };
    let sum = sum_sample_counts(ctts.entries.iter().map(|e| e.sample_count), "ctts")?;
    if sum != total as u64 {
        anyhow::bail!(
            "ctts の sample_count 合計 ({sum}) が stsz のサンプル数 ({total}) と一致しません"
        );
    }
    let mut i = 0usize;
    for entry in &ctts.entries {
        for _ in 0..entry.sample_count {
            cts_offsets[i] = entry.sample_offset;
            i += 1;
        }
    }
    Ok(cts_offsets)
}

fn sum_sample_counts(counts: impl Iterator<Item = u32>, table: &str) -> anyhow::Result<u64> {
    let mut sum: u64 = 0;
    for count in counts {
        sum = sum.checked_add(u64::from(count)).ok_or_else(|| {
            anyhow::anyhow!("{table} の sample_count 合計が u64 を overflow します")
        })?;
    }
    Ok(sum)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// フィクスチャ: H.264 (Avc1) + Opus, GOP 120, 30000/1001fps の mp4。
    /// `tests/fixtures/gen.sh` で生成する。無ければスキップする。
    // cwd 非依存にする（`external::tests` がプロセスの cwd を一時的に変えるため）。
    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.mp4");

    fn skip_if_fixture_missing() -> bool {
        if std::path::Path::new(FIXTURE).exists() {
            return false;
        }
        eprintln!(
            "{FIXTURE} が無いためスキップします。`tests/fixtures/gen.sh` を実行してください。"
        );
        true
    }

    #[test]
    fn finds_video_and_audio_tracks_with_distinct_timescales() {
        if skip_if_fixture_missing() {
            return;
        }
        let moov = read_moov(FIXTURE).expect("moov を読めること");

        let (_video_trak, video_info) =
            find_video_track(&moov).expect("映像トラックが 1 本見つかること");
        let (_audio_trak, audio_info) =
            find_audio_track(&moov).expect("音声トラックが 1 本見つかること");

        assert!(video_info.timescale > 0);
        assert!(audio_info.timescale > 0);
        // 映像と音声で timescale は異なる(前提: CLAUDE.md)。
        assert_ne!(video_info.timescale, audio_info.timescale);
    }

    #[test]
    fn codec_kinds_match_expected_material() {
        if skip_if_fixture_missing() {
            return;
        }
        let moov = read_moov(FIXTURE).expect("moov を読めること");

        let (video_trak, _) = find_video_track(&moov).expect("映像トラックが見つかること");
        let (audio_trak, _) = find_audio_track(&moov).expect("音声トラックが見つかること");

        assert!(matches!(track_codec(video_trak), Some(Codec::Avc1(_))));
        assert!(matches!(track_codec(audio_trak), Some(Codec::Opus(_))));
    }

    #[test]
    fn find_video_track_returns_none_on_empty_moov() {
        let moov = Moov::default();
        assert!(find_video_track(&moov).is_none());
        assert!(find_audio_track(&moov).is_none());
    }

    // --- 音声 Codec 判定の一般化 ---

    /// サンプルエントリ共通の `Audio` ヘッダ（値は判定に影響しないダミー）。
    fn dummy_audio() -> mp4_atom::Audio {
        mp4_atom::Audio {
            data_reference_index: 1,
            channel_count: 2,
            sample_size: 16,
            sample_rate: 48_000u16.into(),
        }
    }

    /// PCM 系サンプルエントリ（`Ipcm` 等）は全て同じ形なのでマクロで量産する。
    macro_rules! pcm {
        ($ty:ident) => {
            mp4_atom::$ty {
                audio: dummy_audio(),
                pcmc: None,
                chnl: None,
                btrt: None,
            }
        };
    }

    /// `mp4-atom` 0.14 が認識する音声系 Codec を 1 つずつ構築して返す。
    ///
    /// 対象一覧（圧縮 6 種 + 非圧縮/QT 系 10 種）と 1:1 で対応させ、
    /// 一覧の増減にテストが追従するようにしている。
    fn all_audio_codecs() -> Vec<Codec> {
        use mp4_atom::esds::{DecoderConfig, DecoderSpecific, EsDescriptor, SLConfig};
        use mp4_atom::*;

        let opus = Codec::Opus(Opus {
            audio: dummy_audio(),
            dops: Dops {
                output_channel_count: 2,
                pre_skip: 0,
                input_sample_rate: 48_000,
                output_gain: 0,
            },
            btrt: None,
        });

        let mp4a = Codec::Mp4a(Mp4a {
            audio: dummy_audio(),
            esds: Esds {
                es_desc: EsDescriptor {
                    es_id: 0,
                    dec_config: DecoderConfig {
                        object_type_indication: 0x40, // AAC
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
        });

        let flac = Codec::Flac(Flac {
            audio: dummy_audio(),
            dfla: Dfla { blocks: Vec::new() },
        });

        let ac3 = Codec::Ac3(Ac3 {
            audio: dummy_audio(),
            dac3: Ac3SpecificBox {
                fscod: 0,
                bsid: 8,
                bsmod: 0,
                acmod: 2,
                lfeon: false,
                bit_rate_code: 0,
            },
        });

        let eac3 = Codec::Eac3(Eac3 {
            audio: dummy_audio(),
            dec3: Ec3SpecificBox {
                data_rate: 0,
                substreams: Vec::new(),
            },
        });

        let samr = Codec::Samr(Samr {
            amrsampleentry: AmrSampleEntry {
                data_reference_index: 1,
                timescale: 8000,
            },
            damr: Damr {
                vendor: b"erat".into(),
                decoder_version: 0,
                mode_set: 0,
                mode_change_period: 0,
                frames_per_sample: 1,
            },
        });

        vec![
            opus,
            mp4a,
            flac,
            ac3,
            eac3,
            samr,
            Codec::Ipcm(pcm!(Ipcm)),
            Codec::Fpcm(pcm!(Fpcm)),
            Codec::Sowt(pcm!(Sowt)),
            Codec::Twos(pcm!(Twos)),
            Codec::Lpcm(pcm!(Lpcm)),
            Codec::In24(pcm!(In24)),
            Codec::In32(pcm!(In32)),
            Codec::Fl32(pcm!(Fl32)),
            Codec::Fl64(pcm!(Fl64)),
            Codec::S16l(pcm!(S16l)),
        ]
    }

    #[test]
    fn all_audio_codecs_are_recognized_as_audio() {
        let codecs = all_audio_codecs();
        // 対象一覧（圧縮 6 + 非圧縮/QT 10）と本数が一致していること。
        assert_eq!(codecs.len(), 16);
        for codec in &codecs {
            assert!(is_audio_codec(codec), "{codec:?} は音声と判定されるべき");
        }
    }

    #[test]
    fn video_subtitle_and_unknown_are_not_audio() {
        use mp4_atom::{Avc1, FourCC, Tx3g};
        assert!(!is_audio_codec(&Codec::Avc1(Avc1::default())));
        assert!(!is_audio_codec(&Codec::Tx3g(Tx3g::default())));
        assert!(!is_audio_codec(&Codec::Unknown(FourCC::new(b"xxxx"))));
    }

    #[test]
    fn find_audio_track_matches_aac_track() {
        use mp4_atom::{Avc1, Mdia, Minf, Stsd};

        let trak_with_codec = |codec: Codec| Trak {
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
        };

        // 音声を AAC(Mp4a) にした moov でも音声トラックが 1 本認識されること。
        let aac = all_audio_codecs()
            .into_iter()
            .find(|c| matches!(c, Codec::Mp4a(_)))
            .unwrap();
        let moov = Moov {
            trak: vec![
                trak_with_codec(Codec::Avc1(Avc1::default())),
                trak_with_codec(aac),
            ],
            ..Default::default()
        };

        assert!(find_video_track(&moov).is_some());
        assert!(find_audio_track(&moov).is_some());
    }

    // --- サンプル表の復元 ---

    /// 合成 stbl 用。オフセット・サイズが収まる十分大きな「ファイル長」。
    const SYNTH_FILE_LEN: u64 = 1_000_000;

    fn file_len(path: &str) -> u64 {
        std::fs::metadata(path)
            .unwrap_or_else(|e| panic!("{path} の metadata 取得に失敗: {e}"))
            .len()
    }

    fn skip_if_ffprobe_missing() -> bool {
        match std::process::Command::new("ffprobe")
            .arg("-version")
            .output()
        {
            Ok(output) if output.status.success() => false,
            _ => {
                eprintln!("ffprobe が無いためスキップします。");
                true
            }
        }
    }

    /// ffprobe が報告する 1 パケットの情報（比較対象の一部だけを持つ）。
    struct FfprobePacket {
        pos: u64,
        size: u32,
        is_sync: bool,
    }

    /// `ffprobe -show_entries packet=size,pos,flags` の出力をパースする。
    ///
    /// 出力はストリームのデコード順（ファイルに格納されている順）そのものなので、
    /// こちらで復元したサンプル一覧とインデックスをそのまま突き合わせられる。
    fn ffprobe_packets(stream_selector: &str) -> Vec<FfprobePacket> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                stream_selector,
                "-show_entries",
                "packet=size,pos,flags",
                "-of",
                "compact=p=0",
                FIXTURE,
            ])
            .output()
            .expect("ffprobe を起動できること");
        assert!(
            output.status.success(),
            "ffprobe が失敗した: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );

        let stdout = String::from_utf8(output.stdout).expect("ffprobe の出力が utf-8 であること");
        stdout
            .lines()
            .filter(|line| !line.is_empty())
            .map(|line| {
                let mut pos = None;
                let mut size = None;
                let mut flags = None;
                for field in line.split('|') {
                    if let Some((key, value)) = field.split_once('=') {
                        match key {
                            "pos" => {
                                pos = Some(value.parse::<u64>().expect("pos が数値であること"))
                            }
                            "size" => {
                                size = Some(value.parse::<u32>().expect("size が数値であること"))
                            }
                            "flags" => flags = Some(value.to_string()),
                            _ => {}
                        }
                    }
                }
                FfprobePacket {
                    pos: pos.expect("pos フィールドがあること"),
                    size: size.expect("size フィールドがあること"),
                    is_sync: flags.expect("flags フィールドがあること").starts_with('K'),
                }
            })
            .collect()
    }

    #[test]
    fn video_samples_match_ffprobe() {
        if skip_if_fixture_missing() || skip_if_ffprobe_missing() {
            return;
        }
        let moov = read_moov(FIXTURE).expect("moov を読めること");
        let (video_trak, _) = find_video_track(&moov).expect("映像トラックが見つかること");
        let ours = samples(&video_trak.mdia.minf.stbl, file_len(FIXTURE)).expect("samples");
        let expected = ffprobe_packets("v:0");

        assert_eq!(ours.len(), expected.len(), "サンプル数が一致すること");
        let our_sync_count = ours.iter().filter(|s| s.is_sync).count();
        let expected_sync_count = expected.iter().filter(|p| p.is_sync).count();
        assert_eq!(
            our_sync_count, expected_sync_count,
            "同期サンプル数が一致すること"
        );

        for (i, (got, want)) in ours.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got.file_offset, want.pos, "sample {i} の file_offset");
            assert_eq!(got.size, want.size, "sample {i} の size");
            assert_eq!(got.is_sync, want.is_sync, "sample {i} の is_sync");
        }
    }

    #[test]
    fn audio_samples_match_ffprobe_and_are_all_sync() {
        if skip_if_fixture_missing() || skip_if_ffprobe_missing() {
            return;
        }
        let moov = read_moov(FIXTURE).expect("moov を読めること");
        let (audio_trak, _) = find_audio_track(&moov).expect("音声トラックが見つかること");
        // 音声(Opus)の stsz は可変サイズ(StszSamples::Different)である前提。
        assert!(matches!(
            audio_trak.mdia.minf.stbl.stsz.samples,
            StszSamples::Different { .. }
        ));

        let ours = samples(&audio_trak.mdia.minf.stbl, file_len(FIXTURE)).expect("samples");
        let expected = ffprobe_packets("a:0");

        assert_eq!(ours.len(), expected.len(), "サンプル数が一致すること");
        assert!(
            ours.iter().all(|s| s.is_sync),
            "stss が無い音声トラックは全サンプルが同期扱いであること"
        );

        for (i, (got, want)) in ours.iter().zip(expected.iter()).enumerate() {
            assert_eq!(got.file_offset, want.pos, "sample {i} の file_offset");
            assert_eq!(got.size, want.size, "sample {i} の size");
        }
    }

    #[test]
    fn samples_with_identical_stsz_sizes() {
        use mp4_atom::{Stco, StscEntry, SttsEntry};

        // stsz が StszSamples::Identical(全サンプル同一サイズ)のケース。
        // チャンク1に3サンプル、チャンク2に2サンプル。
        let mut stbl = Stbl::default();
        stbl.stsz.samples = StszSamples::Identical {
            count: 5,
            size: 100,
        };
        stbl.stco = Some(Stco {
            entries: vec![1000, 2000],
        });
        stbl.stsc.entries = vec![
            StscEntry {
                first_chunk: 1,
                samples_per_chunk: 3,
                sample_description_index: 1,
            },
            StscEntry {
                first_chunk: 2,
                samples_per_chunk: 2,
                sample_description_index: 1,
            },
        ];
        stbl.stts.entries = vec![SttsEntry {
            sample_count: 5,
            sample_delta: 1000,
        }];

        let got = samples(&stbl, SYNTH_FILE_LEN).expect("正常な表は成功すること");

        let expected_offsets = [1000u64, 1100, 1200, 2000, 2100];
        assert_eq!(got.len(), 5);
        for (i, (info, &offset)) in got.iter().zip(expected_offsets.iter()).enumerate() {
            assert_eq!(info.file_offset, offset, "sample {i} の file_offset");
            assert_eq!(info.size, 100, "sample {i} の size");
            assert_eq!(info.duration, 1000, "sample {i} の duration");
            assert_eq!(info.cts_offset, 0, "sample {i} の cts_offset");
            assert!(
                info.is_sync,
                "stss が無いので全サンプルが同期扱いであること"
            );
        }
    }

    #[test]
    fn samples_apply_ctts_and_stss_correctly() {
        use mp4_atom::{Ctts, CttsEntry, Stco, StscEntry, Stss, SttsEntry};

        // duration と cts_offset を取り違えていないか、stss の1始まりを正しく
        // 扱えているか、cts_offset の負値を扱えているかを同時に検証する。
        let mut stbl = Stbl::default();
        stbl.stsz.samples = StszSamples::Different {
            sizes: vec![10, 20, 30, 40],
        };
        stbl.stco = Some(Stco { entries: vec![0] });
        stbl.stsc.entries = vec![StscEntry {
            first_chunk: 1,
            samples_per_chunk: 4,
            sample_description_index: 1,
        }];
        stbl.stts.entries = vec![SttsEntry {
            sample_count: 4,
            sample_delta: 512,
        }];
        stbl.ctts = Some(Ctts {
            entries: vec![
                CttsEntry {
                    sample_count: 2,
                    sample_offset: 1024,
                },
                CttsEntry {
                    sample_count: 2,
                    sample_offset: -256,
                },
            ],
        });
        // stss は1始まりのサンプル番号。サンプル1と3が同期サンプル。
        stbl.stss = Some(Stss {
            entries: vec![1, 3],
        });

        let got = samples(&stbl, SYNTH_FILE_LEN).expect("正常な表は成功すること");

        assert_eq!(got.len(), 4);
        let expected_offsets = [0u64, 10, 30, 60];
        let expected_cts = [1024i64, 1024, -256, -256];
        let expected_sync = [true, false, true, false];
        for i in 0..4 {
            assert_eq!(got[i].file_offset, expected_offsets[i], "sample {i}");
            assert_eq!(got[i].duration, 512, "sample {i} の duration");
            assert_eq!(
                got[i].cts_offset, expected_cts[i],
                "sample {i} の cts_offset"
            );
            assert_eq!(
                got[i].is_sync, expected_sync[i],
                "sample {i} の is_sync (stss は1始まり)"
            );
        }
    }

    #[test]
    fn samples_scale_linearly_for_one_hundred_thousand_samples() {
        use mp4_atom::{Stco, StscEntry, SttsEntry};
        use std::time::Instant;

        // O(n^2) に戻ると数百ms〜秒単位になり、この閾値を超えて失敗するはず。
        const N: usize = 100_000;
        const BUDGET_MS: u128 = 500;

        let mut stbl = Stbl::default();
        stbl.stsz.samples = StszSamples::Identical {
            count: N as u32,
            size: 100,
        };
        // 音声のように1チャンク1サンプルの極端なケース(=チャンク数も10万)にする。
        stbl.stco = Some(Stco {
            entries: (0..N as u32).map(|i| i * 100).collect(),
        });
        stbl.stsc.entries = vec![StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
            sample_description_index: 1,
        }];
        stbl.stts.entries = vec![SttsEntry {
            sample_count: N as u32,
            sample_delta: 1,
        }];

        // N * 100 バイトが収まるファイル長。
        let file_len = (N as u64) * 100;
        let start = Instant::now();
        let got = samples(&stbl, file_len).expect("正常な表は成功すること");
        let elapsed = start.elapsed();

        assert_eq!(got.len(), N);
        assert!(
            elapsed.as_millis() < BUDGET_MS,
            "O(n) 実装なら 10 万サンプルは高速なはず(実測 {elapsed:?})。O(n^2) に戻っていないか確認すること。"
        );
    }

    // --- サンプル表の検証（E17: 過大確保・整数 wrap を拒否） ---

    /// 最小限の正常 stbl（1 チャンク・1 サンプル）をベースに壊す。
    fn minimal_valid_stbl() -> Stbl {
        use mp4_atom::{Stco, StscEntry, SttsEntry};
        let mut stbl = Stbl::default();
        stbl.stsz.samples = StszSamples::Identical { count: 1, size: 10 };
        stbl.stco = Some(Stco { entries: vec![0] });
        stbl.stsc.entries = vec![StscEntry {
            first_chunk: 1,
            samples_per_chunk: 1,
            sample_description_index: 1,
        }];
        stbl.stts.entries = vec![SttsEntry {
            sample_count: 1,
            sample_delta: 1,
        }];
        stbl
    }

    #[test]
    fn samples_rejects_huge_identical_stsz_count_before_alloc() {
        let mut stbl = minimal_valid_stbl();
        stbl.stsz.samples = StszSamples::Identical {
            count: u32::MAX,
            size: 0, // size=0 でも巨大 count は拒否
        };
        stbl.stts.entries[0].sample_count = u32::MAX;
        let err = samples(&stbl, 100).expect_err("巨大 count はエラー");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stsz sample count"),
            "確保前の件数エラーであること: {msg}"
        );
    }

    #[test]
    fn samples_rejects_huge_identical_stsz_size() {
        let mut stbl = minimal_valid_stbl();
        stbl.stsz.samples = StszSamples::Identical {
            count: 1,
            size: u32::MAX,
        };
        let err = samples(&stbl, 100).expect_err("巨大 size はエラー");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stsz sample size"),
            "サイズ検証エラーであること: {msg}"
        );
    }

    #[test]
    fn samples_rejects_identical_stsz_count_times_size_overflow() {
        let mut stbl = minimal_valid_stbl();
        // count * size が u64 を overflow（両方が大きな値）
        stbl.stsz.samples = StszSamples::Identical {
            count: u32::MAX,
            size: u32::MAX,
        };
        // file_len を大きくして件数チェックをすり抜けさせ、乗算 overflow を狙う
        let err = samples(&stbl, u64::from(u32::MAX)).expect_err("count*size overflow");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("overflow") || msg.contains("超えて"),
            "乗算 overflow または合計超過であること: {msg}"
        );
    }

    #[test]
    fn samples_rejects_identical_stsz_total_exceeding_file_len() {
        let mut stbl = minimal_valid_stbl();
        stbl.stsz.samples = StszSamples::Identical {
            count: 10,
            size: 20,
        };
        stbl.stts.entries[0].sample_count = 10;
        stbl.stsc.entries[0].samples_per_chunk = 10;
        // 10 * 20 = 200 > 100
        let err = samples(&stbl, 100).expect_err("合計サイズ超過");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("合計サイズ") || msg.contains("stsz"),
            "合計サイズ検証であること: {msg}"
        );
    }

    #[test]
    fn samples_rejects_different_stsz_size_sum_overflow_and_mismatch() {
        // 個別 size が file_len 超
        let mut stbl = minimal_valid_stbl();
        stbl.stsz.samples = StszSamples::Different {
            sizes: vec![50, 60],
        };
        stbl.stts.entries[0].sample_count = 2;
        stbl.stsc.entries[0].samples_per_chunk = 2;
        let err = samples(&stbl, 55).expect_err("個別 size 超過");
        assert!(
            format!("{err:#}").contains("size"),
            "size 超過であること: {err:#}"
        );

        // 総和が file_len 超（個別は file_len 以下）
        stbl.stsz.samples = StszSamples::Different {
            sizes: vec![40, 40],
        };
        let err = samples(&stbl, 70).expect_err("総和超過");
        assert!(
            format!("{err:#}").contains("合計サイズ"),
            "総和超過であること: {err:#}"
        );
    }

    #[test]
    fn samples_rejects_stts_sample_count_mismatch_and_huge_before_expand() {
        let mut stbl = minimal_valid_stbl();
        // stsz は 1 なのに stts が巨大 → 展開前に不一致で落ちる（OOM しない）
        stbl.stts.entries[0].sample_count = u32::MAX;
        let err = samples(&stbl, SYNTH_FILE_LEN).expect_err("stts 不一致");
        assert!(
            format!("{err:#}").contains("stts"),
            "stts 検証であること: {err:#}"
        );

        // 合計が overflow しうる複数エントリ
        use mp4_atom::SttsEntry;
        stbl.stts.entries = vec![
            SttsEntry {
                sample_count: u32::MAX,
                sample_delta: 1,
            },
            SttsEntry {
                sample_count: u32::MAX,
                sample_delta: 1,
            },
        ];
        let err = samples(&stbl, SYNTH_FILE_LEN).expect_err("stts sum overflow or mismatch");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("stts") || msg.contains("overflow"),
            "stts 合計の検証であること: {msg}"
        );
    }

    #[test]
    fn samples_rejects_ctts_sample_count_mismatch() {
        use mp4_atom::{Ctts, CttsEntry};
        let mut stbl = minimal_valid_stbl();
        stbl.ctts = Some(Ctts {
            entries: vec![CttsEntry {
                sample_count: u32::MAX,
                sample_offset: 0,
            }],
        });
        let err = samples(&stbl, SYNTH_FILE_LEN).expect_err("ctts 不一致");
        assert!(
            format!("{err:#}").contains("ctts"),
            "ctts 検証であること: {err:#}"
        );
    }
}
