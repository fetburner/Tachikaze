// cut パイプライン(#32以降)から消費されるまで未使用。配線されたら外す。
#![allow(dead_code)]

//! `ftyp` → `mdat` → `moov` の順でロスレスな mp4 を書き出す。
//!
//! `stsd` には一切触れない。`moov` を `clone()` し、トラックごとのサンプル表
//! (`stsz`/`stts`/`ctts`/`stss`/`stsc`/`stco`)だけを keep リストに合わせて
//! 差し替える。コーデック固有の知識は不要で、サンプルはただのバイト列として
//! 元ファイルから読み出して `mdat` に詰め直すだけでよい
//! (検証済みコード: docs/mp4-atom.md「書き出し」)。
//!
//! レイアウトは `ftyp` → `mdat` → `moov` の順。`moov` を最後に置くことで、
//! `mdat` に書き終えた時点でオフセットが確定してから `stco` を書ける
//! (サイズの先読みという鶏と卵を回避する)。

use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::Path;

use mp4_atom::{
    Co64, Ctts, CttsEntry, Encode, Ftyp, Moov, Stco, Stsc, StscEntry, Stss, Stsz, StszSamples,
    Stts, SttsEntry,
};

use crate::mp4io::read::{samples, SampleInfo};
use crate::order::DecodeIdx;

/// `mdat` アトムのヘッダ(サイズ4バイト + kind4バイト)の長さ。
const MDAT_HEADER_LEN: u64 = 8;

/// `mdat` を書き終えたあとに `moov` を書けるように、`ftyp` の長さと
/// `mdat` 本体の合計バイト数から `mdat` 本体の開始オフセットを求める。
///
/// `stco` は 32bit オフセットしか持てない(`co64` は今回のスコープ外)ため、
/// `mdat` 本体の最後の1バイトのオフセットが `u32::MAX` を超えるならここで
/// エラーにする(黙って壊れた `stco` を書かない)。
fn mdat_body_start(ftyp_len: u64, mdat_body_len: u64) -> anyhow::Result<u64> {
    let start = ftyp_len
        .checked_add(MDAT_HEADER_LEN)
        .ok_or_else(|| anyhow::anyhow!("ftyp のサイズ計算がオーバーフローした"))?;

    let last_offset = start
        .checked_add(mdat_body_len)
        .ok_or_else(|| anyhow::anyhow!("mdat のサイズ計算がオーバーフローした"))?;

    anyhow::ensure!(
        last_offset <= u32::MAX as u64,
        "mdat が大きすぎて stco (32bit オフセット) で表現できません\
         (mdat 本体 {mdat_body_len} バイト、末尾オフセット {last_offset} > u32::MAX)。\
         co64 対応は #10 送りのため、この入力は処理できません。"
    );

    Ok(start)
}

/// トラックごとの keep リスト(出力に含める順の `DecodeIdx`)から実際の
/// `SampleInfo` を引く。範囲外の `DecodeIdx` はエラーにする。
fn resolve_kept_samples(
    track_index: usize,
    keep: &[DecodeIdx],
    all_samples: &[SampleInfo],
) -> anyhow::Result<Vec<SampleInfo>> {
    keep.iter()
        .map(|idx| {
            all_samples.get(idx.0 as usize).copied().ok_or_else(|| {
                anyhow::anyhow!(
                    "track {track_index}: DecodeIdx({}) がサンプル数({})の範囲外です",
                    idx.0,
                    all_samples.len()
                )
            })
        })
        .collect()
}

/// mp4 を再エンコードせずに書き出す。
///
/// `keep_per_track` は `moov.trak` と同じ順序・同じ長さの `Vec` で、各要素は
/// そのトラックについて出力に含めるサンプルを `DecodeIdx`(デコード順)で
/// 出力順に並べたもの。空の `Vec` を渡すとそのトラックは出力から除外される
/// (例えば音声を含めない出力を作りたい場合)。少なくとも1トラックは
/// 空でない keep リストを持つ必要がある。
pub fn write_mp4<P: AsRef<Path>>(
    input_path: P,
    output_path: P,
    moov: &Moov,
    keep_per_track: &[Vec<DecodeIdx>],
) -> anyhow::Result<()> {
    anyhow::ensure!(
        keep_per_track.len() == moov.trak.len(),
        "keep_per_track の長さ({})が moov.trak の本数({})と一致しません",
        keep_per_track.len(),
        moov.trak.len()
    );

    // 元ファイルのトラックごとのサンプル表(デコード順)を復元し、keep リストが
    // 指す実際のサンプルを解決しておく。
    let kept_samples: Vec<Vec<SampleInfo>> = moov
        .trak
        .iter()
        .zip(keep_per_track.iter())
        .enumerate()
        .map(|(ti, (trak, keep))| {
            let all = samples(&trak.mdia.minf.stbl);
            resolve_kept_samples(ti, keep, &all)
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    // 空でない keep リストを持つトラックだけを出力に含める
    // (「音声トラックがない出力も有効な mp4 になる」ための仕組み)。
    let included: Vec<usize> = (0..moov.trak.len())
        .filter(|&ti| !kept_samples[ti].is_empty())
        .collect();
    anyhow::ensure!(
        !included.is_empty(),
        "出力に含まれるトラックが1本もありません(すべての keep リストが空です)"
    );

    // 1. ftyp を組み立てる(サイズを先に知るため、まずメモリ上でエンコードする)。
    let ftyp = Ftyp {
        major_brand: b"isom".into(),
        minor_version: 512,
        compatible_brands: vec![
            b"isom".into(),
            b"iso2".into(),
            b"avc1".into(),
            b"mp41".into(),
        ],
    };
    let mut ftyp_buf = Vec::new();
    ftyp.encode(&mut ftyp_buf)?;

    let mdat_body_len: u64 = included
        .iter()
        .map(|&ti| kept_samples[ti].iter().map(|s| s.size as u64).sum::<u64>())
        .sum();
    let mdat_body_start = mdat_body_start(ftyp_buf.len() as u64, mdat_body_len)?;

    let output_path = output_path.as_ref();
    let mut out = BufWriter::new(File::create(output_path)?);
    out.write_all(&ftyp_buf)?;

    // 2. mdat: ヘッダを先に書き、続けて keep リストの順に元ファイルから
    // サンプルを読んで追記する。元ファイル全体はメモリに載せず、サンプル単位で
    // seek + read する。
    let mdat_total_len = mdat_body_len + MDAT_HEADER_LEN;
    let mdat_total_len_u32: u32 = mdat_total_len
        .try_into()
        .map_err(|_| anyhow::anyhow!("mdat の総サイズが u32::MAX を超えています"))?;
    out.write_all(&mdat_total_len_u32.to_be_bytes())?;
    out.write_all(b"mdat")?;

    let mut input = BufReader::new(File::open(input_path)?);
    let mut new_offsets: Vec<Vec<u64>> = vec![Vec::new(); moov.trak.len()];
    let mut cursor = mdat_body_start;
    let mut copy_buf: Vec<u8> = Vec::new();

    for &ti in &included {
        for s in &kept_samples[ti] {
            new_offsets[ti].push(cursor);

            input.seek(SeekFrom::Start(s.file_offset))?;
            copy_buf.resize(s.size as usize, 0);
            input.read_exact(&mut copy_buf)?;
            out.write_all(&copy_buf)?;

            cursor += s.size as u64;
        }
    }
    debug_assert_eq!(
        cursor,
        mdat_body_start + mdat_body_len,
        "書き出したバイト数が事前計算と一致すること"
    );

    // 3. moov を clone してトラックごとにサンプルテーブルだけ差し替える。
    let movie_timescale = moov.mvhd.timescale;
    let mut nmoov = moov.clone();

    let mut new_trak = Vec::with_capacity(included.len());
    let mut max_movie_duration = 0u64;

    for &ti in &included {
        let mut trak = moov.trak[ti].clone();
        let kept = &kept_samples[ti];
        let offs = &new_offsets[ti];
        let original_stbl = &moov.trak[ti].mdia.minf.stbl;

        let stbl = &mut trak.mdia.minf.stbl;

        stbl.stsz = Stsz {
            samples: StszSamples::Different {
                sizes: kept.iter().map(|s| s.size).collect(),
            },
        };
        stbl.stts = Stts {
            entries: kept
                .iter()
                .map(|s| SttsEntry {
                    sample_count: 1,
                    sample_delta: s.duration,
                })
                .collect(),
        };
        stbl.ctts = original_stbl.ctts.as_ref().map(|_| Ctts {
            entries: kept
                .iter()
                .map(|s| CttsEntry {
                    sample_count: 1,
                    sample_offset: s.cts_offset,
                })
                .collect(),
        });
        // stss は1始まりの「出力側」サンプル番号。
        stbl.stss = original_stbl.stss.as_ref().map(|_| Stss {
            entries: kept
                .iter()
                .enumerate()
                .filter(|&(_, s)| s.is_sync)
                .map(|(n, _)| n as u32 + 1)
                .collect(),
        });
        stbl.stsc = Stsc {
            entries: vec![StscEntry {
                first_chunk: 1,
                samples_per_chunk: kept.len() as u32,
                sample_description_index: 1,
            }],
        };
        // このバージョンではトラックごとに1チャンクとする前提のため、
        // チャンクの先頭オフセットは offs[0] だけでよい。
        let first_offset = offs[0];
        anyhow::ensure!(
            first_offset <= u32::MAX as u64,
            "track {ti}: stco オフセットが u32::MAX を超えています(mdat_body_start の計算で\
             既に検出されているはずなので、ここに来るのは内部バグです)"
        );
        stbl.stco = Some(Stco {
            entries: vec![first_offset as u32],
        });
        stbl.co64 = None::<Co64>;

        // duration を更新する。
        let track_duration: u64 = kept.iter().map(|s| s.duration as u64).sum();
        trak.mdia.mdhd.duration = track_duration;

        let track_timescale = trak.mdia.mdhd.timescale;
        trak.tkhd.duration = if track_timescale == 0 {
            0
        } else {
            track_duration * movie_timescale as u64 / track_timescale as u64
        };

        max_movie_duration = max_movie_duration.max(trak.tkhd.duration);
        new_trak.push(trak);
    }

    nmoov.trak = new_trak;
    nmoov.mvhd.duration = max_movie_duration;

    let mut moov_buf = Vec::new();
    nmoov.encode(&mut moov_buf)?;
    out.write_all(&moov_buf)?;

    out.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- mdat_body_start: オフセット計算のロジック単体テスト ---
    // (完了条件: 「mdat が4GBを超える場合に明示的なエラーになる」を、
    // 実際に4GBのファイルを作らずに検証する)

    #[test]
    fn mdat_body_start_computes_normal_case() {
        // ftyp 32バイト + mdat ヘッダ8バイト = 40。
        let start = mdat_body_start(32, 1000).expect("通常サイズは成功すること");
        assert_eq!(start, 40);
    }

    #[test]
    fn mdat_body_start_errors_when_exceeding_u32_max() {
        // mdat 本体の末尾オフセットが u32::MAX を超えるケース。
        let err = mdat_body_start(32, u32::MAX as u64).unwrap_err();
        assert!(err.to_string().contains("stco"));
    }

    #[test]
    fn mdat_body_start_errors_on_overflow() {
        let err = mdat_body_start(u64::MAX, 1).unwrap_err();
        assert!(err.to_string().contains("オーバーフロー"));
    }

    #[test]
    fn mdat_body_start_accepts_boundary_just_under_u32_max() {
        // start(40) + body <= u32::MAX ぎりぎりのケースは成功する。
        let body_len = u32::MAX as u64 - 40;
        let start = mdat_body_start(32, body_len).expect("境界ちょうどは成功すること");
        assert_eq!(start + body_len, u32::MAX as u64);
    }

    // --- write_mp4: フィクスチャを使った統合テスト ---

    const FIXTURE: &str = "tests/fixtures/sample.mp4";

    fn skip_if_fixture_missing() -> bool {
        if Path::new(FIXTURE).exists() {
            return false;
        }
        eprintln!(
            "{FIXTURE} が無いためスキップします。`tests/fixtures/gen.sh` を実行してください。"
        );
        true
    }

    fn skip_if_missing(bin: &str) -> bool {
        match std::process::Command::new(bin).arg("-version").output() {
            Ok(output) if output.status.success() => false,
            _ => {
                eprintln!("{bin} が無いためスキップします。");
                true
            }
        }
    }

    /// ffprobe でデコードできるパケット数を数える(`-count_packets` は使わず
    /// 実際にパケット行を数える。読み取り専用ツールでの検証)。
    fn ffprobe_packet_count(path: &Path, stream_selector: &str) -> u32 {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                stream_selector,
                "-show_entries",
                "packet=pts",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("ffprobe を起動できること");
        assert!(
            output.status.success(),
            "ffprobe が失敗した: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("ffprobe の出力が utf-8 であること")
            .lines()
            .filter(|line| !line.is_empty())
            .count() as u32
    }

    fn decodes_cleanly(path: &Path) -> bool {
        let output = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-i"])
            .arg(path)
            .args(["-f", "null", "-"])
            .output()
            .expect("ffmpeg を起動できること");
        if !output.status.success() || !output.stderr.is_empty() {
            eprintln!(
                "ffmpeg decode failed: status={:?} stderr={}",
                output.status,
                String::from_utf8_lossy(&output.stderr)
            );
            return false;
        }
        true
    }

    #[test]
    fn write_mp4_roundtrips_video_and_audio() {
        if skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe") {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let (video_trak, _) =
            crate::mp4io::read::find_video_track(&moov).expect("映像トラックがあること");
        let (audio_trak, _) =
            crate::mp4io::read::find_audio_track(&moov).expect("音声トラックがあること");

        let video_samples = samples(&video_trak.mdia.minf.stbl);
        let audio_samples = samples(&audio_trak.mdia.minf.stbl);

        // 全サンプルを keep する(このテストの主眼はカット処理の正しさではなく
        // 書き出しパイプラインが有効な mp4 を生成できること)。
        let keep_per_track: Vec<Vec<DecodeIdx>> = moov
            .trak
            .iter()
            .map(|trak| {
                let n = samples(&trak.mdia.minf.stbl).len();
                (0..n as u32).map(DecodeIdx).collect()
            })
            .collect();

        let tmp_dir =
            std::env::temp_dir().join(format!("tachikaze-write-mp4-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_full.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        assert!(
            decodes_cleanly(&out_path),
            "ffmpeg でエラーなくデコードできること"
        );

        let got_video_packets = ffprobe_packet_count(&out_path, "v:0");
        let got_audio_packets = ffprobe_packet_count(&out_path, "a:0");
        assert_eq!(got_video_packets as usize, video_samples.len());
        assert_eq!(got_audio_packets as usize, audio_samples.len());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn write_mp4_video_only_output_is_valid() {
        if skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe") {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let (video_trak, _) =
            crate::mp4io::read::find_video_track(&moov).expect("映像トラックがあること");
        let video_samples = samples(&video_trak.mdia.minf.stbl);

        // 音声トラックの keep リストを空にして、音声を含めない出力を作る
        // (--video-only 相当)。
        let keep_per_track: Vec<Vec<DecodeIdx>> = moov
            .trak
            .iter()
            .map(|trak| {
                let codec_is_video = matches!(
                    trak.mdia.minf.stbl.stsd.codecs.first(),
                    Some(mp4_atom::Codec::Avc1(_))
                );
                if codec_is_video {
                    let n = samples(&trak.mdia.minf.stbl).len();
                    (0..n as u32).map(DecodeIdx).collect()
                } else {
                    Vec::new()
                }
            })
            .collect();

        let tmp_dir = std::env::temp_dir().join(format!(
            "tachikaze-write-mp4-test-video-only-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_video_only.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        assert!(
            decodes_cleanly(&out_path),
            "ffmpeg でエラーなくデコードできること"
        );

        let got_video_packets = ffprobe_packet_count(&out_path, "v:0");
        assert_eq!(got_video_packets as usize, video_samples.len());

        // 音声トラックが無いことを ffprobe で確認する。
        let probe = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_type",
                "-of",
                "csv=p=0",
            ])
            .arg(&out_path)
            .output()
            .expect("ffprobe を起動できること");
        let stream_types = String::from_utf8_lossy(&probe.stdout);
        assert!(
            !stream_types.contains("audio"),
            "音声トラックが含まれていないこと: {stream_types}"
        );
        assert!(
            stream_types.contains("video"),
            "映像トラックは残っていること: {stream_types}"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn write_mp4_rejects_out_of_range_decode_idx() {
        if skip_if_fixture_missing() {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let keep_per_track: Vec<Vec<DecodeIdx>> = moov
            .trak
            .iter()
            .map(|_| vec![DecodeIdx(u32::MAX)])
            .collect();

        let tmp_dir = std::env::temp_dir().join(format!(
            "tachikaze-write-mp4-test-oor-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_oor.mp4");

        let err =
            write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track).unwrap_err();
        assert!(err.to_string().contains("範囲外"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn write_mp4_rejects_all_empty_keep_lists() {
        if skip_if_fixture_missing() {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let keep_per_track: Vec<Vec<DecodeIdx>> = moov.trak.iter().map(|_| Vec::new()).collect();

        let tmp_dir = std::env::temp_dir().join(format!(
            "tachikaze-write-mp4-test-empty-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_empty.mp4");

        let err =
            write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track).unwrap_err();
        assert!(err.to_string().contains("1本もありません"));

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// `ffprobe -show_data_hash CRC32` でパケット単位のハッシュを取得する。
    ///
    /// CLAUDE.md の罠2: 無劣化の検証に md5 を使わない
    /// (`h264_mp4toannexb` が IDR ごとに SPS/PPS を再挿入しバイト数一致でも
    /// ハッシュがずれるため)。ここでは mp4 コンテナのパケットをそのまま
    /// 比較するので、この罠には該当しない(そもそも再エンコードしていない)。
    fn ffprobe_packet_hashes(path: &Path, stream_selector: &str) -> Vec<String> {
        let output = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-select_streams",
                stream_selector,
                "-show_entries",
                "packet=size,data_hash",
                "-show_data_hash",
                "CRC32",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .expect("ffprobe を起動できること");
        assert!(
            output.status.success(),
            "ffprobe が失敗した: {:?}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout)
            .expect("ffprobe の出力が utf-8 であること")
            .lines()
            .filter(|line| !line.is_empty())
            .map(str::to_string)
            .collect()
    }

    #[test]
    fn write_mp4_output_is_bit_identical_to_input_packets() {
        if skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe") {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let keep_per_track: Vec<Vec<DecodeIdx>> = moov
            .trak
            .iter()
            .map(|trak| {
                let n = samples(&trak.mdia.minf.stbl).len();
                (0..n as u32).map(DecodeIdx).collect()
            })
            .collect();

        let tmp_dir = std::env::temp_dir().join(format!(
            "tachikaze-write-mp4-test-crc32-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_crc32.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        for selector in ["v:0", "a:0"] {
            let want = ffprobe_packet_hashes(Path::new(FIXTURE), selector);
            let got = ffprobe_packet_hashes(&out_path, selector);
            assert_eq!(
                got, want,
                "{selector} のパケットが CRC32 単位で完全一致すること(無劣化の検証)"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
