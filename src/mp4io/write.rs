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

/// `mdat` アトムの通常ヘッダ(サイズ4バイト + kind4バイト)の長さ。
const MDAT_HEADER_LEN: u64 = 8;

/// `mdat` アトムの largesize ヘッダの長さ。
///
/// ISO BMFF のボックスヘッダは通常「サイズ4バイト + 種別4バイト」だが、
/// サイズが32bitに収まらない場合は「サイズ=1(4バイト) + 種別4バイト +
/// 64bit の実サイズ(8バイト)」という形式が使える(先頭の `size` フィールドに
/// `1` を書くと続く8バイトが実際のサイズだという意味になる)。
/// `mp4_atom` の `Header::encode` も同じ規則で largesize を選択しており
/// (`src/header.rs`)、ここでの判定はそれと整合させてある。
const MDAT_LARGESIZE_HEADER_LEN: u64 = 16;

/// `mdat` アトムのレイアウト計画。
///
/// `write_mp4` 本体から純粋関数として切り出してあるのは、実際に4GB超の
/// ファイルを作らずに `co64`/largesize の分岐を unit test できるようにする
/// ため(docs/mp4-atom.md「本実装で直すべき点」3番)。
#[derive(Debug)]
struct MdatLayout {
    /// `mdat` ヘッダの長さ(8 または 16)。
    header_len: u64,
    /// largesize 形式のヘッダを使うかどうか。
    use_largesize: bool,
    /// `mdat` 本体の開始オフセット(= `ftyp_len + header_len`)。
    body_start: u64,
    /// `stco`(32bit オフセット)ではなく `co64`(64bit オフセット)を
    /// 使うべきかどうか。
    use_co64: bool,
}

/// `ftyp` の長さと `mdat` 本体の合計バイト数から `mdat` のレイアウトを
/// 決める純粋関数。
///
/// `use_largesize` は「`mdat` の通常ヘッダ込みの合計サイズ(`mdat_body_len + 8`)
/// が `u32::MAX` に収まるか」で決める。`use_co64` は「`mdat` の終端オフセット
/// (`body_start + mdat_body_len`)が `u32::MAX` を超えるか」で決める。
/// 個々のチャンクオフセットではなく `mdat` 全体の終端で判定することで、
/// 「一部のトラックだけ `co64` になる」というちぐはぐな状態を避ける。
fn plan_mdat_layout(ftyp_len: u64, mdat_body_len: u64) -> anyhow::Result<MdatLayout> {
    let normal_total = mdat_body_len
        .checked_add(MDAT_HEADER_LEN)
        .ok_or_else(|| anyhow::anyhow!("mdat のサイズ計算がオーバーフローした"))?;
    let use_largesize = normal_total > u32::MAX as u64;
    let header_len = if use_largesize {
        MDAT_LARGESIZE_HEADER_LEN
    } else {
        MDAT_HEADER_LEN
    };

    let body_start = ftyp_len
        .checked_add(header_len)
        .ok_or_else(|| anyhow::anyhow!("ftyp のサイズ計算がオーバーフローした"))?;

    let mdat_end = body_start
        .checked_add(mdat_body_len)
        .ok_or_else(|| anyhow::anyhow!("mdat のサイズ計算がオーバーフローした"))?;
    let use_co64 = mdat_end > u32::MAX as u64;

    Ok(MdatLayout {
        header_len,
        use_largesize,
        body_start,
        use_co64,
    })
}

/// `plan_mdat_layout` の結果に従って `mdat` のヘッダバイトを書く。
///
/// `impl Write` にしてあるのは unit test で `Vec<u8>` に対しても呼べるように
/// するため。本番では `BufWriter<File>` に対して呼ぶ。
fn write_mdat_header(
    out: &mut impl Write,
    layout: &MdatLayout,
    mdat_body_len: u64,
) -> anyhow::Result<()> {
    if layout.use_largesize {
        // size フィールドに 1 を書くと「続く8バイトが実際のサイズ」の意味になる。
        out.write_all(&1u32.to_be_bytes())?;
        out.write_all(b"mdat")?;
        let largesize = mdat_body_len
            .checked_add(layout.header_len)
            .ok_or_else(|| anyhow::anyhow!("mdat の largesize 計算がオーバーフローした"))?;
        out.write_all(&largesize.to_be_bytes())?;
    } else {
        let total = mdat_body_len
            .checked_add(layout.header_len)
            .ok_or_else(|| anyhow::anyhow!("mdat のサイズ計算がオーバーフローした"))?;
        let total: u32 = total
            .try_into()
            .map_err(|_| anyhow::anyhow!("mdat の総サイズが u32::MAX を超えています"))?;
        out.write_all(&total.to_be_bytes())?;
        out.write_all(b"mdat")?;
    }
    Ok(())
}

/// `kept` の `duration` 列をランレングス圧縮して `stts` のエントリ列にする。
///
/// 1サンプル1エントリだと10万サンプルで `stts` が800KBになる
/// (docs/mp4-atom.md「本実装で直すべき点」)。固定フレームレートの映像・音声では
/// 同じ `duration` が連続するため、連続区間をまとめれば大幅に小さくなる。
fn run_length_encode_stts(kept: &[SampleInfo]) -> Vec<SttsEntry> {
    let mut entries: Vec<SttsEntry> = Vec::new();
    for s in kept {
        match entries.last_mut() {
            Some(last) if last.sample_delta == s.duration => {
                last.sample_count += 1;
            }
            _ => entries.push(SttsEntry {
                sample_count: 1,
                sample_delta: s.duration,
            }),
        }
    }
    entries
}

/// `kept` の `cts_offset` 列をランレングス圧縮して `ctts` のエントリ列にする。
///
/// `stts` と同じ理由で圧縮する。`sample_offset` は負値も取り得るが、
/// グループ判定は単純な等値比較でよい。
fn run_length_encode_ctts(kept: &[SampleInfo]) -> Vec<CttsEntry> {
    let mut entries: Vec<CttsEntry> = Vec::new();
    for s in kept {
        match entries.last_mut() {
            Some(last) if last.sample_offset == s.cts_offset => {
                last.sample_count += 1;
            }
            _ => entries.push(CttsEntry {
                sample_count: 1,
                sample_offset: s.cts_offset,
            }),
        }
    }
    entries
}

/// トラックを1秒(そのトラックの `timescale`)程度の単位でチャンクに分割した
/// ときの、1チャンク分の情報。
///
/// `start_time_secs` はトラックをまたいで開始時刻順にマージするための
/// キーで、映像・音声の `timescale` が異なっていても比較できるように
/// 秒単位に正規化してある。
struct Chunk {
    /// `moov.trak` の何番目のトラックか。
    track_index: usize,
    /// そのトラックの `kept` 内でのチャンク先頭サンプルのインデックス。
    start_sample: usize,
    /// チャンクに含まれるサンプル数。
    sample_count: u32,
    /// チャンク先頭の表示時刻(秒)。クロストラックの時刻順ソートに使う。
    start_time_secs: f64,
}

/// 1トラックの `kept` サンプル列を、`duration` の累積が `timescale`
/// (≒1秒)を超えるたびに切ってチャンク列にする。
///
/// docs/mp4-atom.md「本実装で直すべき点」: トラックごとに1チャンクだと
/// `mdat` が「映像全部→音声全部」の順になりプレイヤが大きくシークするため、
/// 1秒程度でチャンクを切ってトラック間でインターリーブする土台になる。
/// 最後に閾値未満で余ったサンプルは1つの短いチャンクにまとめる。
fn chunk_track(kept: &[SampleInfo], timescale: u32, track_index: usize) -> Vec<Chunk> {
    // timescale が 0 の異常な入力では閾値判定ができないため、
    // 全体を1チャンクとして扱う(サンプルを取りこぼさないための保険)。
    let threshold = if timescale == 0 {
        u64::MAX
    } else {
        timescale as u64
    };

    let mut chunks = Vec::new();
    let mut start_sample = 0usize;
    let mut cumulative_before_chunk = 0u64;
    let mut cumulative_in_chunk = 0u64;

    for (i, s) in kept.iter().enumerate() {
        cumulative_in_chunk += s.duration as u64;
        let is_last_sample = i + 1 == kept.len();

        if cumulative_in_chunk >= threshold || is_last_sample {
            chunks.push(Chunk {
                track_index,
                start_sample,
                sample_count: (i - start_sample + 1) as u32,
                start_time_secs: cumulative_before_chunk as f64 / threshold as f64,
            });
            cumulative_before_chunk += cumulative_in_chunk;
            cumulative_in_chunk = 0;
            start_sample = i + 1;
        }
    }

    chunks
}

/// チャンクごとのサンプル数列から `stsc` のエントリ列をランレングス圧縮で
/// 組み立てる。
///
/// `StscEntry` は「このエントリの `first_chunk` から次のエントリの
/// `first_chunk` の前まで、`samples_per_chunk` 個ずつ」を表すため、値が
/// 変わった時だけ新しいエントリを追加すれば自然に RLE になる。
fn build_stsc_entries(chunk_sample_counts: &[u32]) -> Vec<StscEntry> {
    let mut entries: Vec<StscEntry> = Vec::new();
    for (i, &count) in chunk_sample_counts.iter().enumerate() {
        let chunk_number = (i + 1) as u32; // 1始まり
        match entries.last() {
            Some(last) if last.samples_per_chunk == count => {}
            _ => entries.push(StscEntry {
                first_chunk: chunk_number,
                samples_per_chunk: count,
                sample_description_index: 1,
            }),
        }
    }
    entries
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
    let mdat_layout = plan_mdat_layout(ftyp_buf.len() as u64, mdat_body_len)?;
    let mdat_body_start = mdat_layout.body_start;

    let output_path = output_path.as_ref();
    let mut out = BufWriter::new(File::create(output_path)?);
    out.write_all(&ftyp_buf)?;

    // 2. mdat: ヘッダを先に書き、続けて keep リストの順に元ファイルから
    // サンプルを読んで追記する。元ファイル全体はメモリに載せず、サンプル単位で
    // seek + read する。4GB を超える場合は largesize 形式のヘッダになる
    // (plan_mdat_layout 参照)。
    write_mdat_header(&mut out, &mdat_layout, mdat_body_len)?;

    // トラックごとに1秒程度でチャンクに分割し、開始時刻でグローバルに
    // マージする。これがそのまま「映像→音声→映像→…」という mdat 上の
    // 書き出し順になる(同一トラック内ではチャンクは元々時刻順なので、
    // 複数のソート済み列を1列にマージする操作になる)。
    let mut all_chunks: Vec<Chunk> = Vec::new();
    for &ti in &included {
        let timescale = moov.trak[ti].mdia.mdhd.timescale;
        all_chunks.extend(chunk_track(&kept_samples[ti], timescale, ti));
    }
    all_chunks.sort_by(|a, b| a.start_time_secs.total_cmp(&b.start_time_secs));

    let mut input = BufReader::new(File::open(input_path)?);
    let mut new_offsets: Vec<Vec<u64>> = vec![Vec::new(); moov.trak.len()];
    let mut chunk_sample_counts: Vec<Vec<u32>> = vec![Vec::new(); moov.trak.len()];
    let mut cursor = mdat_body_start;
    let mut copy_buf: Vec<u8> = Vec::new();

    for chunk in &all_chunks {
        let ti = chunk.track_index;
        // 同一トラック内ではチャンクはマージ前の時刻順が保たれるため、
        // マージ順に追記していけばそのまま正しいチャンク順の
        // オフセット列/サンプル数列になる。
        new_offsets[ti].push(cursor);
        chunk_sample_counts[ti].push(chunk.sample_count);

        let end_sample = chunk.start_sample + chunk.sample_count as usize;
        for s in &kept_samples[ti][chunk.start_sample..end_sample] {
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
            entries: run_length_encode_stts(kept),
        };
        stbl.ctts = original_stbl.ctts.as_ref().map(|_| Ctts {
            entries: run_length_encode_ctts(kept),
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
            entries: build_stsc_entries(&chunk_sample_counts[ti]),
        };
        // 各チャンクの先頭オフセット列そのものが stco/co64 になる。
        // stco(32bit) で表現できるかどうかは個々のオフセットではなく
        // mdat 全体の終端オフセットで判定済み(plan_mdat_layout.use_co64)。
        // 一部のトラックだけ co64 になる、というちぐはぐな状態を避けるため。
        if mdat_layout.use_co64 {
            stbl.stco = None;
            stbl.co64 = Some(Co64 {
                entries: offs.clone(),
            });
        } else {
            stbl.stco = Some(Stco {
                entries: offs.iter().map(|&o| o as u32).collect(),
            });
            stbl.co64 = None;
        }

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

    // --- plan_mdat_layout / write_mdat_header: オフセット計算のロジック単体テスト ---
    // (完了条件: 「4GB を超える出力に対応する」を、実際に4GBのファイルを
    // 作らずに co64/largesize の分岐込みで検証する)

    #[test]
    fn plan_mdat_layout_computes_normal_case() {
        // ftyp 32バイト + mdat ヘッダ8バイト = 40。
        let layout = plan_mdat_layout(32, 1000).expect("通常サイズは成功すること");
        assert_eq!(layout.header_len, MDAT_HEADER_LEN);
        assert!(!layout.use_largesize);
        assert_eq!(layout.body_start, 40);
        assert!(!layout.use_co64);
    }

    #[test]
    fn plan_mdat_layout_errors_on_overflow() {
        let err = plan_mdat_layout(u64::MAX, 1).unwrap_err();
        assert!(err.to_string().contains("オーバーフロー"));
    }

    #[test]
    fn plan_mdat_layout_accepts_boundary_just_under_u32_max() {
        // body_start(40) + body <= u32::MAX ぎりぎりのケースは stco のまま成功する。
        let body_len = u32::MAX as u64 - 40;
        let layout = plan_mdat_layout(32, body_len).expect("境界ちょうどは成功すること");
        assert!(!layout.use_largesize);
        assert!(!layout.use_co64);
        assert_eq!(layout.body_start + body_len, u32::MAX as u64);
    }

    #[test]
    fn plan_mdat_layout_uses_largesize_and_co64_when_mdat_body_exceeds_4gb() {
        // mdat 本体だけで(通常ヘッダ込みで)u32::MAX を超える、実際に4GB超の
        // 出力になるケース。largesize ヘッダ(16バイト)になり、当然 mdat の
        // 終端オフセットも u32::MAX を超えるので co64 になる。
        let body_len = u32::MAX as u64 + 1_000_000;
        let layout = plan_mdat_layout(32, body_len).expect("largesize で成功すること");
        assert_eq!(layout.header_len, MDAT_LARGESIZE_HEADER_LEN);
        assert!(layout.use_largesize);
        assert_eq!(layout.body_start, 32 + MDAT_LARGESIZE_HEADER_LEN);
        assert!(layout.use_co64);
    }

    #[test]
    fn plan_mdat_layout_uses_co64_without_largesize_when_only_end_offset_exceeds_u32_max() {
        // use_largesize と use_co64 は別々の閾値で決まることを確認する。
        // mdat 本体自体は小さくても(largesize は不要)、ftyp が異常に
        // 大きければ mdat の終端オフセットは u32::MAX を超えうる。
        let body_len = 1000u64;
        let huge_ftyp_len = u32::MAX as u64;
        let layout =
            plan_mdat_layout(huge_ftyp_len, body_len).expect("成功すること(co64 で救える)");
        assert!(
            !layout.use_largesize,
            "mdat 本体自体は小さいので largesize は不要"
        );
        assert!(
            layout.use_co64,
            "mdat の終端オフセットが u32::MAX を超えるので co64 が必要"
        );
    }

    #[test]
    fn write_mdat_header_writes_normal_8_byte_header() {
        let layout = plan_mdat_layout(32, 1000).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_mdat_header(&mut buf, &layout, 1000).expect("書き込みが成功すること");

        // 手動でパースする: size==1 でなければ通常ヘッダ(size4 + kind4)。
        assert_eq!(buf.len(), 8);
        let size = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        assert_ne!(size, 1, "largesize のプレースホルダではないこと");
        assert_eq!(size, 1008, "size フィールドは本体+ヘッダの合計であること");
        assert_eq!(&buf[4..8], b"mdat");
    }

    #[test]
    fn write_mdat_header_writes_largesize_16_byte_header_when_body_exceeds_4gb() {
        let body_len = u32::MAX as u64 + 1_000_000;
        let layout = plan_mdat_layout(32, body_len).unwrap();
        let mut buf: Vec<u8> = Vec::new();
        write_mdat_header(&mut buf, &layout, body_len).expect("書き込みが成功すること");

        // 手動でパースする: size==1 なら続く8バイトが実際のサイズ。
        assert_eq!(buf.len(), 16);
        let placeholder = u32::from_be_bytes(buf[0..4].try_into().unwrap());
        assert_eq!(placeholder, 1, "largesize のプレースホルダであること");
        assert_eq!(&buf[4..8], b"mdat");
        let largesize = u64::from_be_bytes(buf[8..16].try_into().unwrap());
        assert_eq!(
            largesize,
            body_len + MDAT_LARGESIZE_HEADER_LEN,
            "largesize フィールドは本体+16バイトヘッダの合計であること"
        );
    }

    // --- Co64: mp4_atom のエンコード/デコードでエントリが保持されることの確認 ---
    // (完了条件: 実ファイルで4GB超のケースを作れないため、mp4_atom の
    // 型を直接 encode/decode してエントリが保持されることを確認する)

    #[test]
    fn co64_roundtrips_via_encode_decode() {
        use mp4_atom::Decode;

        let co64 = Co64 {
            // u32::MAX を超えるオフセットを含めて、64bit 値が欠けずに
            // 保持されることを確認する。
            entries: vec![0, 1_000, u32::MAX as u64 + 12_345, 5_000_000_000],
        };
        let mut buf = Vec::new();
        co64.encode(&mut buf).expect("encode が成功すること");

        let mut slice = buf.as_slice();
        let decoded = Co64::decode(&mut slice).expect("decode が成功すること");
        assert_eq!(decoded, co64, "co64 のエントリが完全に保持されること");
    }

    // --- write_mp4: フィクスチャを使った統合テスト ---

    // cwd 非依存にする（`external::tests` がプロセスの cwd を一時的に変えるため）。
    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.mp4");

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

    // --- run_length_encode_stts / run_length_encode_ctts: ランレングス圧縮 ---

    /// テスト用に `SampleInfo` を作る。`file_offset`/`size`/`is_sync` は
    /// このテストの主眼(duration/cts_offset の圧縮)に関係しないので固定値でよい。
    fn sample_info(duration: u32, cts_offset: i64) -> SampleInfo {
        SampleInfo {
            file_offset: 0,
            size: 0,
            duration,
            cts_offset,
            is_sync: false,
        }
    }

    /// 圧縮済みの `stts` エントリ列を展開して、サンプルごとの `duration` 列に戻す。
    fn expand_stts(entries: &[SttsEntry]) -> Vec<u32> {
        entries
            .iter()
            .flat_map(|e| std::iter::repeat_n(e.sample_delta, e.sample_count as usize))
            .collect()
    }

    /// 圧縮済みの `ctts` エントリ列を展開して、サンプルごとの `cts_offset` 列に戻す。
    fn expand_ctts(entries: &[CttsEntry]) -> Vec<i64> {
        entries
            .iter()
            .flat_map(|e| std::iter::repeat_n(e.sample_offset, e.sample_count as usize))
            .collect()
    }

    #[test]
    fn run_length_encode_stts_expands_back_to_original_durations() {
        // 固定フレームレート区間(3001が10個)のあとに1個だけ違う値、
        // その後また同じ値が続く、という典型的な構成。
        let durations = [3001, 3001, 3001, 3001, 3001, 3002, 3001, 3001, 3001, 3001];
        let kept: Vec<SampleInfo> = durations.iter().map(|&d| sample_info(d, 0)).collect();

        let encoded = run_length_encode_stts(&kept);

        // 3区間(3001が5個、3002が1個、3001が4個)にまとまっているはず。
        assert_eq!(encoded.len(), 3);

        let expanded = expand_stts(&encoded);
        assert_eq!(expanded, durations.to_vec());
    }

    #[test]
    fn run_length_encode_stts_handles_all_same_duration() {
        let kept: Vec<SampleInfo> = (0..50).map(|_| sample_info(3003, 0)).collect();
        let encoded = run_length_encode_stts(&kept);

        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0].sample_count, 50);
        assert_eq!(encoded[0].sample_delta, 3003);

        let expanded = expand_stts(&encoded);
        assert_eq!(expanded, vec![3003; 50]);
    }

    #[test]
    fn run_length_encode_stts_handles_empty_input() {
        let kept: Vec<SampleInfo> = Vec::new();
        let encoded = run_length_encode_stts(&kept);
        assert!(encoded.is_empty());
    }

    #[test]
    fn run_length_encode_ctts_expands_back_to_original_offsets_including_negative() {
        // 罠: sample_offset は i64 で負値も含む。B フレームの並べ替えで
        // 表示順がデコード順より前に来るケースがあるため、負値を必ずテストする。
        let offsets: Vec<i64> = vec![0, 0, 0, -1024, -1024, 512, 512, 512, -3, 0, 0];
        let kept: Vec<SampleInfo> = offsets.iter().map(|&o| sample_info(3001, o)).collect();

        let encoded = run_length_encode_ctts(&kept);

        // 連続する値ごとにまとまっているので、5区間になるはず
        // (0x3, -1024x2, 512x3, -3x1, 0x2)。
        assert_eq!(encoded.len(), 5);

        let expanded = expand_ctts(&encoded);
        assert_eq!(expanded, offsets);
    }

    #[test]
    fn run_length_encode_ctts_handles_all_same_offset() {
        let kept: Vec<SampleInfo> = (0..50).map(|_| sample_info(3003, -7)).collect();
        let encoded = run_length_encode_ctts(&kept);

        assert_eq!(encoded.len(), 1);
        assert_eq!(encoded[0].sample_count, 50);
        assert_eq!(encoded[0].sample_offset, -7);

        let expanded = expand_ctts(&encoded);
        assert_eq!(expanded, vec![-7; 50]);
    }

    #[test]
    fn run_length_encode_stts_compresses_hundred_thousand_constant_fps_samples() {
        // 55分素材相当: 30fps なら約60fps基準のtimescaleで3001刻み、
        // 約10万サンプル。すべて同じ duration の合成データ。
        const N: usize = 100_000;
        let kept: Vec<SampleInfo> = (0..N).map(|_| sample_info(3001, 0)).collect();

        let encoded = run_length_encode_stts(&kept);

        assert!(
            encoded.len() <= 50,
            "固定フレームレートの10万サンプルなら数十エントリ以下になるはず(実際: {})",
            encoded.len()
        );
        assert_eq!(
            encoded.len(),
            1,
            "全サンプル同じ duration なら1エントリになるはず"
        );

        let expanded = expand_stts(&encoded);
        assert_eq!(expanded.len(), N);
        assert!(expanded.iter().all(|&d| d == 3001));
    }

    #[test]
    fn run_length_encode_ctts_compresses_hundred_thousand_samples_with_few_distinct_offsets() {
        // 55分素材相当の10万サンプルで、cts_offset は数種類の値が
        // ある程度長い連続区間を作りながら繰り返す、という現実的な合成データ
        // (B フレームの並べ替えパターンが周期的に繰り返される想定)。
        const N: usize = 100_000;
        let pattern: [i64; 4] = [0, 3001, -3001, 0];
        let mut kept: Vec<SampleInfo> = Vec::with_capacity(N);
        let mut i = 0;
        while kept.len() < N {
            let offset = pattern[i % pattern.len()];
            // 各値を1000サンプル連続させる(GOP相当のまとまりを模す)。
            for _ in 0..1000 {
                if kept.len() >= N {
                    break;
                }
                kept.push(sample_info(3001, offset));
            }
            i += 1;
        }

        let encoded = run_length_encode_ctts(&kept);

        assert!(
            encoded.len() <= 200,
            "1000サンプルごとにしか値が変わらないなら数百エントリ以下になるはず(実際: {})",
            encoded.len()
        );

        let expanded = expand_ctts(&encoded);
        let original: Vec<i64> = kept.iter().map(|s| s.cts_offset).collect();
        assert_eq!(expanded, original);
    }

    /// フィクスチャで実際に `write_mp4` を呼び、圧縮後の `moov` サイズが
    /// サンプル数に対して小さいことを確認する。完了条件は「圧縮前の実装との
    /// 比較」ではなく「エントリ数がサンプル数より大幅に少ない」こと。
    #[test]
    fn write_mp4_compresses_stts_ctts_entry_count_far_below_sample_count() {
        if skip_if_fixture_missing() {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let (video_trak, _) =
            crate::mp4io::read::find_video_track(&moov).expect("映像トラックがあること");
        let video_samples = samples(&video_trak.mdia.minf.stbl);

        let keep_per_track: Vec<Vec<DecodeIdx>> = moov
            .trak
            .iter()
            .map(|trak| {
                let n = samples(&trak.mdia.minf.stbl).len();
                (0..n as u32).map(DecodeIdx).collect()
            })
            .collect();

        let tmp_dir = std::env::temp_dir().join(format!(
            "tachikaze-write-mp4-test-moov-size-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_moov_size.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        let out_moov = crate::mp4io::read::read_moov(&out_path).expect("出力の moov を読めること");
        let (out_video_trak, _) =
            crate::mp4io::read::find_video_track(&out_moov).expect("映像トラックがあること");
        let out_stbl = &out_video_trak.mdia.minf.stbl;

        assert!(
            !video_samples.is_empty(),
            "フィクスチャに映像サンプルがあること(テストの前提)"
        );
        assert!(
            out_stbl.stts.entries.len() < video_samples.len(),
            "stts のエントリ数({})がサンプル数({})より大幅に少ないこと",
            out_stbl.stts.entries.len(),
            video_samples.len()
        );
        if let Some(ctts) = &out_stbl.ctts {
            assert!(
                ctts.entries.len() < video_samples.len(),
                "ctts のエントリ数({})がサンプル数({})より大幅に少ないこと",
                ctts.entries.len(),
                video_samples.len()
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // --- チャンクのインターリーブ ---

    /// 指定パスの指定範囲を読んでバイト列を返す(往復テスト用の生読み出し)。
    fn read_bytes_at(path: &Path, offset: u64, size: u32) -> Vec<u8> {
        let mut f = File::open(path).expect("開けること");
        f.seek(SeekFrom::Start(offset)).expect("seek できること");
        let mut buf = vec![0u8; size as usize];
        f.read_exact(&mut buf).expect("read_exact できること");
        buf
    }

    /// 出力の `stsc`/`stco` から再構成した `file_offset`/`size` が、実際に
    /// `mdat` へ書き込んだ内容と一致することを確認する(往復テスト)。
    ///
    /// 完了条件: 「各サンプルの file_offset が stsc/stco から正しく
    /// 再構成できることを、読み込み器(samples())で自分の出力を
    /// 読み直して確認する」。ここでは keep リストが全サンプルかつ元の順序
    /// なので、出力側で読み直したサンプル `i` のバイト列が入力ファイル側の
    /// サンプル `i` のバイト列とまったく同じであることまで検証する
    /// (offset がズレていればここで内容不一致として検出できる)。
    #[test]
    fn write_mp4_chunk_offsets_roundtrip_via_read_samples() {
        if skip_if_fixture_missing() || skip_if_missing("ffmpeg") || skip_if_missing("ffprobe") {
            return;
        }

        let moov = crate::mp4io::read::read_moov(FIXTURE).expect("moov を読めること");
        let (video_trak, _) =
            crate::mp4io::read::find_video_track(&moov).expect("映像トラックがあること");
        let (audio_trak, _) =
            crate::mp4io::read::find_audio_track(&moov).expect("音声トラックがあること");
        let orig_video_samples = samples(&video_trak.mdia.minf.stbl);
        let orig_audio_samples = samples(&audio_trak.mdia.minf.stbl);

        let keep_per_track: Vec<Vec<DecodeIdx>> = moov
            .trak
            .iter()
            .map(|trak| {
                let n = samples(&trak.mdia.minf.stbl).len();
                (0..n as u32).map(DecodeIdx).collect()
            })
            .collect();

        let tmp_dir = std::env::temp_dir().join(format!(
            "tachikaze-write-mp4-test-chunk-roundtrip-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_chunk_roundtrip.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        let out_moov = crate::mp4io::read::read_moov(&out_path).expect("出力の moov を読めること");
        let (out_video_trak, _) =
            crate::mp4io::read::find_video_track(&out_moov).expect("映像トラックがあること");
        let (out_audio_trak, _) =
            crate::mp4io::read::find_audio_track(&out_moov).expect("音声トラックがあること");
        let out_video_samples = samples(&out_video_trak.mdia.minf.stbl);
        let out_audio_samples = samples(&out_audio_trak.mdia.minf.stbl);

        assert_eq!(out_video_samples.len(), orig_video_samples.len());
        assert_eq!(out_audio_samples.len(), orig_audio_samples.len());

        for (i, (orig, got)) in orig_video_samples
            .iter()
            .zip(out_video_samples.iter())
            .enumerate()
        {
            assert_eq!(got.size, orig.size, "video sample {i}: size が一致すること");
            let want_bytes = read_bytes_at(Path::new(FIXTURE), orig.file_offset, orig.size);
            let got_bytes = read_bytes_at(&out_path, got.file_offset, got.size);
            assert_eq!(
                got_bytes, want_bytes,
                "video sample {i}: stsc/stco から再構成した file_offset の内容が\
                 実際に書き込んだ内容と一致すること"
            );
        }

        for (i, (orig, got)) in orig_audio_samples
            .iter()
            .zip(out_audio_samples.iter())
            .enumerate()
        {
            assert_eq!(got.size, orig.size, "audio sample {i}: size が一致すること");
            let want_bytes = read_bytes_at(Path::new(FIXTURE), orig.file_offset, orig.size);
            let got_bytes = read_bytes_at(&out_path, got.file_offset, got.size);
            assert_eq!(
                got_bytes, want_bytes,
                "audio sample {i}: stsc/stco から再構成した file_offset の内容が\
                 実際に書き込んだ内容と一致すること"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// `mdat` 内で映像と音声のチャンクが時刻順に交互になっていることを、
    /// 出力を読み直した `stco`(チャンク先頭オフセット列)から確認する。
    ///
    /// 完了条件: 「映像だけがまとまって先に来る→その後音声だけ、のような
    /// 偏りがないこと」。ここでは全チャンクをオフセット順に並べたときの
    /// 最長の同一トラック連続長(run)が全体のチャンク数よりずっと小さい
    /// ことを確認する(完全に分離していれば run の長さ = チャンク数になる)。
    #[test]
    fn write_mp4_interleaves_video_and_audio_chunks_in_mdat() {
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
            "tachikaze-write-mp4-test-interleave-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_interleave.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        let out_moov = crate::mp4io::read::read_moov(&out_path).expect("出力の moov を読めること");
        let (out_video_trak, _) =
            crate::mp4io::read::find_video_track(&out_moov).expect("映像トラックがあること");
        let (out_audio_trak, _) =
            crate::mp4io::read::find_audio_track(&out_moov).expect("音声トラックがあること");

        let video_offsets: Vec<u32> = out_video_trak
            .mdia
            .minf
            .stbl
            .stco
            .as_ref()
            .expect("映像トラックに stco があること")
            .entries
            .clone();
        let audio_offsets: Vec<u32> = out_audio_trak
            .mdia
            .minf
            .stbl
            .stco
            .as_ref()
            .expect("音声トラックに stco があること")
            .entries
            .clone();

        assert!(
            video_offsets.len() > 1,
            "映像トラックが複数チャンクに分割されていること(実際: {})",
            video_offsets.len()
        );
        assert!(
            audio_offsets.len() > 1,
            "音声トラックが複数チャンクに分割されていること(実際: {})",
            audio_offsets.len()
        );

        // 時刻(≒オフセット順、mdat には時刻順に書いているのでオフセット順が
        // そのまま時刻順になる)でマージし、トラック種別のタグ付きシーケンスを作る。
        let mut tagged: Vec<(u32, &str)> = Vec::new();
        tagged.extend(video_offsets.iter().map(|&o| (o, "video")));
        tagged.extend(audio_offsets.iter().map(|&o| (o, "audio")));
        tagged.sort_by_key(|&(o, _)| o);

        let total_chunks = tagged.len();

        // 最長の同一トラック連続長を求める。
        let mut longest_run = 1usize;
        let mut current_run = 1usize;
        for i in 1..tagged.len() {
            if tagged[i].1 == tagged[i - 1].1 {
                current_run += 1;
                longest_run = longest_run.max(current_run);
            } else {
                current_run = 1;
            }
        }

        assert!(
            longest_run < total_chunks / 2,
            "映像/音声チャンクがインターリーブされていること\
             (最長の同一トラック連続長 {longest_run} が全チャンク数 {total_chunks} の半分未満であるはず)"
        );

        // 最初の数チャンクの出現順にも両トラックが混在していることを確認する。
        let head_len = tagged.len().min(10);
        let head_tags: std::collections::HashSet<&str> =
            tagged[..head_len].iter().map(|&(_, t)| t).collect();
        assert_eq!(
            head_tags.len(),
            2,
            "先頭 {head_len} チャンクに映像と音声の両方が混在していること(実際: {:?})",
            &tagged[..head_len]
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// チャンク数が「出力の秒数」程度になっていることを確認する
    /// (1秒程度でインターリーブする、という設計の直接確認)。
    #[test]
    fn write_mp4_chunk_count_is_close_to_output_duration_in_seconds() {
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
            "tachikaze-write-mp4-test-chunk-count-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
        let out_path = tmp_dir.join("out_chunk_count.mp4");

        write_mp4(FIXTURE, out_path.to_str().unwrap(), &moov, &keep_per_track)
            .expect("write_mp4 が成功すること");

        let out_moov = crate::mp4io::read::read_moov(&out_path).expect("出力の moov を読めること");
        let (out_video_trak, _) =
            crate::mp4io::read::find_video_track(&out_moov).expect("映像トラックがあること");
        let (out_audio_trak, _) =
            crate::mp4io::read::find_audio_track(&out_moov).expect("音声トラックがあること");

        let video_duration_secs =
            out_video_trak.mdia.mdhd.duration as f64 / out_video_trak.mdia.mdhd.timescale as f64;
        let audio_duration_secs =
            out_audio_trak.mdia.mdhd.duration as f64 / out_audio_trak.mdia.mdhd.timescale as f64;

        let video_chunk_count = out_video_trak
            .mdia
            .minf
            .stbl
            .stco
            .as_ref()
            .expect("映像トラックに stco があること")
            .entries
            .len();
        let audio_chunk_count = out_audio_trak
            .mdia
            .minf
            .stbl
            .stco
            .as_ref()
            .expect("音声トラックに stco があること")
            .entries
            .len();

        // フィクスチャは20秒程度(tests/fixtures/gen.sh)。「1秒程度」という
        // 目安に対して大きく外れていないことだけを確認する(半分〜2倍)。
        for (name, duration_secs, chunk_count) in [
            ("video", video_duration_secs, video_chunk_count),
            ("audio", audio_duration_secs, audio_chunk_count),
        ] {
            let lower = (duration_secs * 0.5).floor() as usize;
            let upper = (duration_secs * 2.0).ceil() as usize;
            assert!(
                chunk_count >= lower.max(1) && chunk_count <= upper,
                "{name}: チャンク数({chunk_count})が出力の秒数({duration_secs:.1}s)の\
                 半分〜2倍の範囲内であるはず"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
