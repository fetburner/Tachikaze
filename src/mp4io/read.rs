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
/// （前提: 対象素材は H.264 + Opus で、両者の timescale は一致しない）。
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

/// `moov` から音声トラック（`Codec::Opus`）を 1 本だけ見つける。
///
/// 複数本存在する場合は最初に見つかったものを返す（対象素材は音声 1 本を想定）。
pub fn find_audio_track(moov: &Moov) -> Option<(&Trak, TrackInfo)> {
    moov.trak.iter().find_map(|trak| match track_codec(trak) {
        Some(Codec::Opus(_)) => Some((trak, track_info(trak))),
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
pub fn samples(stbl: &Stbl) -> Vec<SampleInfo> {
    let sizes: Vec<u32> = match &stbl.stsz.samples {
        StszSamples::Identical { count, size } => vec![*size; *count as usize],
        StszSamples::Different { sizes } => sizes.clone(),
    };
    let total = sizes.len();

    let chunk_offsets: Vec<u64> = match (&stbl.stco, &stbl.co64) {
        (Some(stco), _) => stco.entries.iter().map(|&o| o as u64).collect(),
        (_, Some(co64)) => co64.entries.clone(),
        _ => vec![],
    };

    let mut durations = Vec::with_capacity(total);
    for entry in &stbl.stts.entries {
        for _ in 0..entry.sample_count {
            durations.push(entry.sample_delta);
        }
    }

    let mut cts_offsets = vec![0i64; total];
    if let Some(ctts) = &stbl.ctts {
        let mut i = 0usize;
        for entry in &ctts.entries {
            for _ in 0..entry.sample_count {
                if i < cts_offsets.len() {
                    cts_offsets[i] = entry.sample_offset;
                    i += 1;
                }
            }
        }
    }

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
                    duration: durations.get(sample_index).copied().unwrap_or(0),
                    cts_offset: cts_offsets[sample_index],
                    is_sync: all_sync || sync_samples.contains(&sample_number),
                });

                offset += size as u64;
                sample_index += 1;
            }
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// フィクスチャ: H.264 (Avc1) + Opus, GOP 120, 30000/1001fps の mp4。
    /// `tests/fixtures/gen.sh`（issue #15）で生成する。無ければスキップする。
    const FIXTURE: &str = "tests/fixtures/sample.mp4";

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

    // --- #26: サンプル表の復元 ---

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
        let ours = samples(&video_trak.mdia.minf.stbl);
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

        let ours = samples(&audio_trak.mdia.minf.stbl);
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

        let got = samples(&stbl);

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

        let got = samples(&stbl);

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

        let start = Instant::now();
        let got = samples(&stbl);
        let elapsed = start.elapsed();

        assert_eq!(got.len(), N);
        assert!(
            elapsed.as_millis() < BUDGET_MS,
            "O(n) 実装なら 10 万サンプルは高速なはず(実測 {elapsed:?})。O(n^2) に戻っていないか確認すること。"
        );
    }
}
