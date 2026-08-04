//! cut パイプラインの最終段: 区間ごとの assert と、失敗時の出力破棄。
//!
//! これまでの issue で作られた部品（`mp4io::read` / `mp4io::order_map` / `plan` /
//! `audio` / `mp4io::write`）を実際に組み合わせて呼び出し、書き出した結果を検証し、
//! 1つでも問題があれば出力を破棄する唯一のモジュール（[`cut_and_verify`]）。
//!
//! ## 検証の骨格
//!
//! 1. `write_mp4` で **一時ファイル** に書き出す（本番の `output_path` には直接書かない）。
//! 2. 一時ファイルを `read_moov` で読み直し、区間ごとの assert（検査1〜3）と
//!    `.dtvi` との突き合わせ（検査4）を行う。
//! 3. 音声を含む場合、区間ごとに出力の先頭音声パケットが元ファイルの正しい位置の
//!    パケットと一致することを確認し（検査5）、A/V ずれの最大値をログ出力する
//!    （検査6）。
//! 4. 1つでも失敗すれば一時ファイルを削除してエラーを返す。すべて通れば
//!    `std::fs::rename` で `output_path` へ移動する。
//!
//! ffprobe によるパケット単位 CRC32 の一致確認（`--verify` 指定時、CLAUDE.md の罠2）は
//! [`verify_with_ffprobe`] / [`cut_verify_and_ffprobe_check`] が担当する。
//! `cut_and_verify` が `output_path` へ rename した**後**に、ffprobe を使って元ファイルと
//! 出力ファイルのパケットを突き合わせる。映像は `video_keep`（元ファイルのデコード順
//! `DecodeIdx` 列、出力に含める順そのもの）から期待される CRC32 列を組み立てて1対1で
//! 比較し（`-ss` によるタイムスタンプベースの抽出より単純で確実）、音声は集合比較 +
//! dts 単調増加の assert で補う。不一致なら `output_path` を削除する。
//!
//! ## 設計判断1: 検査4は「出力」ではなく「元ファイル」を `.dtvi` と突き合わせる
//!
//! `.dtvi`（dtvindex の出力）は元ファイルのサンプル配置を索引したものであり、カット後の
//! 出力ファイルとは対応しない。出力はサンプルの部分集合を（区間ごとに同期サンプルから
//! 連続する範囲で）抜き出して連結したものなので、`.dtvi` が記録している
//! `frame_number` / `sample_number` / `random_access_sample` の値は出力ファイル上の
//! インデックスとは一致しない。そのため「出力ファイル自体を `.dtvi` と突き合わせる」
//! 方法は存在しない。
//!
//! 代わりに、cut のこの最終段でも「元ファイルの `SampleInfo` から自前導出した
//! `DisplayDecodeMap` が `.dtvi` の全行と一致すること」
//! （[`crate::mp4io::order_map::verify_against_dtvi`]）をもう一度確認する
//! （[`verify_dtvi_consistency`]）。既存の検証と論理的には重複するが、cut パイプライン
//! 全体を1箇所で検証する「最後の関門」として、表示順とデコード順の混同（CLAUDE.md の
//! 罠3、唯一の重大バグ源）を実行時にもう一度確認する意味がある。
//!
//! ## 設計判断2: `.dtvi` は既定で必須
//!
//! 検査4（`.dtvi` との突き合わせ）が、表示順とデコード順の混同という唯一の重大バグ源
//! （CLAUDE.md の罠3）に対する実効的な防御である。`.dtvi` が無いと検査4を一切実行でき
//! ず、混同があってもエラーを出さずに間違った位置で切られたファイルが出力されてしまう
//! （CLAUDE.md「静かに壊れる4つの罠」の通り、混同は例外を飛ばさない）。そのため
//! [`cut_and_verify`] は `dtvi: None` を早期エラーにする。エラーメッセージには
//! `tachikaze analyze` を先に実行するよう促す趣旨の文言を含める。`.dtvi` 無しでの実行を
//! 許容するオプション（例: `--skip-dtvi-check`）は本 issue のスコープ外とし、将来
//! 必要になった時点で明示的なフラグとして追加する。

use std::collections::HashSet;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::Context;
use mp4_atom::Moov;

use crate::audio::{self, AudioSegment, AvSyncReport};
use crate::dtvi::Dtvi;
use crate::mp4io::order_map::{verify_against_dtvi, DisplayDecodeMap};
use crate::mp4io::read::{find_audio_track, find_video_track, read_moov, samples, SampleInfo};
use crate::mp4io::write::write_mp4;
use crate::order::{DecodeIdx, OrderMap};
use crate::plan::SnappedRange;

/// 検査5（音声の位置）と検査6（音声ドリフトのログ出力）に共通で必要な入力をまとめた
/// もの。
///
/// 生のタプルのままだと `cut_and_verify` の引数が読みにくくなるため、意味のある名前を
/// 持つ小さな構造体として切り出す（issue が許容している調整）。
#[derive(Clone, Copy)]
pub struct AudioDiffInputs<'a> {
    /// 出力に並べる順の各映像区間の再生時間（映像トラックの timescale 単位）。
    pub video_segment_durations: &'a [u64],
    /// 各映像区間の**ソース上の絶対 DTS**（映像トラックの timescale 単位。合成時刻/
    /// PTS ではない。`src/commands.rs::segment_video_source_starts` 参照）。
    /// `video_segment_durations` と同じ長さ・同じ順序。実際に `cut` が音声区間選択に
    /// 使った値そのものであり、検査6（音声ドリフトのログ）で使う。
    ///
    /// 検査5（音声の位置）は、この値をそのまま信用すると「値自体が誤っていた場合に
    /// 検出できない」（`select_audio_segments` に渡す値と検証に使う値が同じ計算経路
    /// から来ることになるため）。そのため検査5は、元ファイルの映像 `SampleInfo` から
    /// 独立に DTS を計算し直した値（[`verify_written_output`] 内の
    /// `independent_video_segment_source_starts`）と突き合わせる。出力タイムライン上の
    /// 累積時間ではないことにも注意（docs/lossless-cut.md「実際に起きた誤り」参照）。
    pub video_segment_source_starts: &'a [u64],
    /// 映像トラックの timescale。
    pub video_timescale: u32,
    /// 音声トラックの全サンプル（デコード順 == 表示順）。
    pub audio_samples: &'a [SampleInfo],
    /// 音声トラックの timescale。
    pub audio_timescale: u32,
}

/// [`cut_and_verify`] が全ての検査を通過したときの結果レポート。
#[derive(Debug, Clone)]
pub struct VerifyReport {
    /// 出力に書き出された映像パケットの総数。
    pub video_packet_count: usize,
    /// 出力に書き出された映像の保持区間数。
    pub video_range_count: usize,
    /// 音声トラックを含む出力の場合の A/V 同期レポート（`--video-only` なら `None`）。
    pub av_sync: Option<AvSyncReport>,
}

/// cut パイプラインの最終段。`write_mp4` で一時ファイルに書き出し、区間ごとの検査
/// （1〜5）と音声ドリフトのログ出力（6）を行う。1つでも検査に失敗すれば一時ファイルを
/// 削除してエラーを返す。すべて通れば `output_path` へ rename する。
///
/// `dtvi` は既定で必須（`None` はエラー）。理由は本ファイル冒頭のdoc commentを参照。
///
/// `keep_per_track`（`write_mp4` への引数）は `video_track_index` / `audio_track_index`
/// から `moov.trak` と同じ順序で組み立てる（音声が無ければ空リスト）。
#[allow(clippy::too_many_arguments)]
pub fn cut_and_verify(
    input_path: &Path,
    output_path: &Path,
    moov: &Moov,
    video_track_index: usize,
    audio_track_index: Option<usize>,
    snapped_video_ranges: &[SnappedRange],
    video_keep: &[DecodeIdx],
    audio_segments: Option<&[AudioSegment]>,
    video_order: &OrderMap,
    dtvi: Option<&Dtvi>,
    audio_samples_for_diff: Option<AudioDiffInputs<'_>>,
) -> anyhow::Result<VerifyReport> {
    let dtvi = dtvi.ok_or_else(|| {
        anyhow::anyhow!(
            "cut_and_verify には .dtvi が必須です（検査4「.dtvi との突き合わせ」が \
             表示順/デコード順の混同を検出する唯一の実効的な防御のため）。\
             先に `tachikaze analyze` を実行して .dtvi を生成してください。"
        )
    })?;

    anyhow::ensure!(
        video_track_index < moov.trak.len(),
        "video_track_index({}) が moov.trak の本数({})の範囲外です",
        video_track_index,
        moov.trak.len()
    );
    if let Some(ai) = audio_track_index {
        anyhow::ensure!(
            ai < moov.trak.len(),
            "audio_track_index({}) が moov.trak の本数({})の範囲外です",
            ai,
            moov.trak.len()
        );
        anyhow::ensure!(
            ai != video_track_index,
            "audio_track_index と video_track_index が同じトラック({ai})を指しています"
        );
    }
    anyhow::ensure!(
        audio_track_index.is_some() == audio_segments.is_some(),
        "audio_track_index と audio_segments の有無が一致しません（両方 Some か両方 \
         None である必要があります）"
    );

    let keep_per_track = build_keep_per_track(
        moov,
        video_track_index,
        audio_track_index,
        video_keep,
        audio_segments,
    );

    let tmp_path = temp_output_path(output_path);
    // 前回の失敗などで同名の一時ファイルが残っていれば先に片付けておく。
    let _ = std::fs::remove_file(&tmp_path);

    if let Err(err) = write_mp4(input_path, tmp_path.as_path(), moov, &keep_per_track) {
        let _ = std::fs::remove_file(&tmp_path);
        return Err(err);
    }

    match verify_written_output(
        &tmp_path,
        input_path,
        moov,
        snapped_video_ranges,
        video_order,
        dtvi,
        audio_segments,
        audio_samples_for_diff,
    ) {
        Ok(report) => {
            std::fs::rename(&tmp_path, output_path).with_context(|| {
                format!(
                    "一時ファイル({})から出力先({})への rename に失敗しました",
                    tmp_path.display(),
                    output_path.display()
                )
            })?;
            Ok(report)
        }
        Err(err) => {
            // 1つでも検査に失敗したら出力を破棄する（この関数の中心的な責務）。
            let _ = std::fs::remove_file(&tmp_path);
            Err(err)
        }
    }
}

/// `moov.trak` と同じ順序・同じ長さの keep リストを組み立てる。
fn build_keep_per_track(
    moov: &Moov,
    video_track_index: usize,
    audio_track_index: Option<usize>,
    video_keep: &[DecodeIdx],
    audio_segments: Option<&[AudioSegment]>,
) -> Vec<Vec<DecodeIdx>> {
    let mut keep_per_track: Vec<Vec<DecodeIdx>> = vec![Vec::new(); moov.trak.len()];
    keep_per_track[video_track_index] = video_keep.to_vec();

    if let (Some(ai), Some(segments)) = (audio_track_index, audio_segments) {
        keep_per_track[ai] = segments
            .iter()
            .flat_map(|seg| (seg.start.0..seg.end.0).map(DecodeIdx))
            .collect();
    }

    keep_per_track
}

/// `output_path` と同じディレクトリに、一意なサフィックスを付けた一時ファイルパスを作る。
fn temp_output_path(output_path: &Path) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut os = output_path.as_os_str().to_os_string();
    os.push(format!(".verify-tmp-{}-{nonce}", std::process::id()));
    PathBuf::from(os)
}

/// 書き出した一時ファイルを読み直し、検査1〜6を行う。
#[allow(clippy::too_many_arguments)]
fn verify_written_output(
    tmp_path: &Path,
    input_path: &Path,
    original_moov: &Moov,
    snapped_video_ranges: &[SnappedRange],
    video_order: &OrderMap,
    dtvi: &Dtvi,
    audio_segments: Option<&[AudioSegment]>,
    audio_samples_for_diff: Option<AudioDiffInputs<'_>>,
) -> anyhow::Result<VerifyReport> {
    let output_moov = read_moov(tmp_path).with_context(|| {
        format!(
            "検証のため出力ファイル({})の読み直しに失敗しました",
            tmp_path.display()
        )
    })?;

    let (output_video_trak, _) = find_video_track(&output_moov)
        .ok_or_else(|| anyhow::anyhow!("出力ファイルに映像トラックが見つかりません"))?;
    let output_video_samples = samples(&output_video_trak.mdia.minf.stbl);

    // 検査1・3: 区間ごとのパケット数と、各区間先頭が同期サンプルであること。
    verify_video_ranges(&output_video_samples, snapped_video_ranges)?;

    // 検査2: 出力全体で表示順に欠落がないこと。
    verify_display_order_is_contiguous(&output_video_samples)?;

    // 検査4: 元ファイルの自前導出が .dtvi と一致すること（設計判断1参照）。
    verify_dtvi_consistency(original_moov, video_order, dtvi)?;

    // 検査5: 音声が正しい位置から取れていること（--video-only ならスキップ）。
    // 出力ファイルの音声トラックの SampleInfo は、音声を含む場合だけここで読む
    // （`output_moov` は既に読み込み済みなので `read_moov` をもう一度呼ぶ必要はない）。
    let output_audio_samples = match audio_segments {
        Some(_) => {
            let (output_audio_trak, _) = find_audio_track(&output_moov).ok_or_else(|| {
                anyhow::anyhow!(
                    "検査5(音声の位置)に失敗: 出力ファイルに音声トラックが見つかりません"
                )
            })?;
            Some(samples(&output_audio_trak.mdia.minf.stbl))
        }
        None => None,
    };
    // 検査5の期待位置は `audio_samples_for_diff.video_segment_source_starts`
    // （実際に `select_audio_segments` へ渡された値）をそのまま信用しない。その値自体が
    // 合成時刻と DTS を取り違えるような系統的な誤りを持っていた場合、同じ値で
    // 再計算しても必ず一致してしまい検出できない（過去に実際にこれが起きた。
    // docs/lossless-cut.md「実際に起きた誤り」参照）。そのため元ファイルの映像
    // `SampleInfo` から独立に DTS を計算し直す。
    let independent_source_starts = match audio_segments {
        Some(_) => {
            let (original_video_trak, _) = find_video_track(original_moov).ok_or_else(|| {
                anyhow::anyhow!("検査5(音声の位置)に失敗: 元ファイルに映像トラックが見つかりません")
            })?;
            let original_video_samples = samples(&original_video_trak.mdia.minf.stbl);
            Some(independent_video_segment_source_starts(
                snapped_video_ranges,
                video_order,
                &original_video_samples,
            )?)
        }
        None => None,
    };
    verify_audio_segment_positions(
        input_path,
        tmp_path,
        audio_segments,
        output_audio_samples.as_deref(),
        audio_samples_for_diff,
        independent_source_starts.as_deref(),
    )?;

    // 検査6: 音声の累積誤差の最大値をログ出力する（--video-only ならスキップ）。
    let av_sync = log_audio_drift(audio_segments, audio_samples_for_diff)?;

    Ok(VerifyReport {
        video_packet_count: output_video_samples.len(),
        video_range_count: snapped_video_ranges.len(),
        av_sync,
    })
}

/// 検査1（パケット数）・検査3（先頭が同期サンプル）。
///
/// `snapped` は半開区間 `[start.snapped, end.snapped)` の列。出力の映像サンプル列を
/// 各区間の長さ（`E - S`）で順に区切り、区切ったサンプル数の合計が出力の総サンプル数と
/// 一致すること、および各区切りの先頭サンプルが同期サンプルであることを確認する。
fn verify_video_ranges(
    output_samples: &[SampleInfo],
    snapped: &[SnappedRange],
) -> anyhow::Result<()> {
    let expected_counts: Vec<u32> = snapped
        .iter()
        .map(|r| r.end.snapped - r.start.snapped)
        .collect();
    let expected_total: u64 = expected_counts.iter().map(|&c| u64::from(c)).sum();

    anyhow::ensure!(
        output_samples.len() as u64 == expected_total,
        "検査1(パケット数)に失敗: 出力の映像サンプル総数({}) が全区間の合計 sum(E-S) \
         ({expected_total}) と一致しません",
        output_samples.len()
    );

    let mut cursor = 0usize;
    for (range_idx, (&count, range)) in expected_counts.iter().zip(snapped.iter()).enumerate() {
        let count = count as usize;
        anyhow::ensure!(
            count > 0,
            "検査1(パケット数)に失敗: 区間{}(表示順 {}..{}) の想定パケット数が0です",
            range_idx + 1,
            range.start.snapped.0,
            range.end.snapped.0
        );

        let group = &output_samples[cursor..cursor + count];
        anyhow::ensure!(
            group[0].is_sync,
            "検査3(先頭が同期サンプル)に失敗: 区間{}(表示順 {}..{}, 出力デコード順 {}..{}) \
             の先頭サンプルが同期サンプルではありません",
            range_idx + 1,
            range.start.snapped.0,
            range.end.snapped.0,
            cursor,
            cursor + count
        );

        cursor += count;
    }
    debug_assert_eq!(cursor, output_samples.len());

    Ok(())
}

/// 検査2（表示順に欠落がない）。
///
/// 出力の映像サンプル列全体で [`DisplayDecodeMap::build`] を呼び、エラーにならない
/// （合成時刻の同値が無い）ことを確認する。これが成功すれば、`build` の実装（合成時刻で
/// ソートしたものを表示順として `0..len()` を割り当てる）により表示順が `0..len()` を
/// 過不足なくカバーすることは構造的に保証されるが、念のため `order.to_display` で得た
/// 値の集合が `0..len()` と一致することも確認する。
fn verify_display_order_is_contiguous(output_samples: &[SampleInfo]) -> anyhow::Result<()> {
    let map = DisplayDecodeMap::build(output_samples).context(
        "検査2(表示順に欠落がない)に失敗: 出力全体で合成時刻(dts+cts_offset)が複数の \
         デコード順サンプルで同値です",
    )?;

    let len = output_samples.len();
    let mut seen = vec![false; len];
    for i in 0..len as u32 {
        let display = map.order.to_display(DecodeIdx(i)).ok_or_else(|| {
            anyhow::anyhow!(
                "検査2(表示順に欠落がない)に失敗: デコード順インデックス{i}に対応する \
                 表示順が見つかりません"
            )
        })?;
        let display_idx = display.0 as usize;
        anyhow::ensure!(
            display_idx < len,
            "検査2(表示順に欠落がない)に失敗: 表示順インデックス{display_idx}が \
             サンプル総数{len}の範囲外です"
        );
        anyhow::ensure!(
            !seen[display_idx],
            "検査2(表示順に欠落がない)に失敗: 表示順インデックス{display_idx}が \
             複数のデコード順サンプルに割り当てられています"
        );
        seen[display_idx] = true;
    }
    anyhow::ensure!(
        seen.iter().all(|&s| s),
        "検査2(表示順に欠落がない)に失敗: 表示順が 0..{len} を過不足なく覆っていません"
    );

    Ok(())
}

/// 検査4（`.dtvi` との突き合わせ）。設計判断1参照: 出力ではなく元ファイルを検証する。
fn verify_dtvi_consistency(
    original_moov: &Moov,
    video_order: &OrderMap,
    dtvi: &Dtvi,
) -> anyhow::Result<()> {
    let (orig_video_trak, _) = find_video_track(original_moov).ok_or_else(|| {
        anyhow::anyhow!("検査4(.dtvi突き合わせ)に失敗: 元ファイルに映像トラックが見つかりません")
    })?;
    let orig_video_samples = samples(&orig_video_trak.mdia.minf.stbl);

    // video_order が今まさに検証しようとしている元ファイルの映像トラックから作られた
    // ものであることの安価な整合性チェック(対応数の一致)。古い/別ファイル由来の
    // OrderMap が渡された場合の取り違えを早期に検出する。
    anyhow::ensure!(
        video_order.len() == orig_video_samples.len(),
        "検査4(.dtvi突き合わせ)に失敗: video_order の対応数({}) が元ファイルの映像サンプル \
         数({})と一致しません(古い/別ファイル由来の OrderMap が渡された可能性があります)",
        video_order.len(),
        orig_video_samples.len()
    );

    let orig_map = DisplayDecodeMap::build(&orig_video_samples).context(
        "検査4(.dtvi突き合わせ)に失敗: 元ファイルの映像サンプルから表示順を再導出できません \
         でした(合成時刻の同値)",
    )?;

    verify_against_dtvi(&orig_map, dtvi)
        .context("検査4(.dtvi突き合わせ)に失敗: .dtvi と元ファイルの自前導出が一致しません")
}

/// スナップ済み区間ごとに、その区間の**ソース上の絶対 DTS**を、元ファイルの映像
/// `SampleInfo` から独立に計算する。
///
/// `src/commands.rs::segment_video_source_starts` と同じ式（`mp4io::order_map::
/// decode_timestamp`、`dts(0) = 0` のデコード順 duration 累積）を使うが、呼び出しは
/// 完全に別経路（`cut` パイプラインが実際に使った値である
/// `AudioDiffInputs::video_segment_source_starts` は一切参照しない）。
///
/// 検査5（[`verify_audio_segment_positions`]）が `AudioDiffInputs::
/// video_segment_source_starts` だけを信用してしまうと、その値自体に系統的な誤り
/// （合成時刻と DTS の取り違えなど）があった場合、同じ値で「期待位置」を再計算しても
/// 必ず一致してしまい検出できない（過去に実際にこれが起きていた。
/// docs/lossless-cut.md「実際に起きた誤り」参照）。この関数が返す値を「本当の意味で
/// 独立な」正解として検査5で使うことで、その取り違えを検出できるようにする。
fn independent_video_segment_source_starts(
    snapped_video_ranges: &[SnappedRange],
    video_order: &OrderMap,
    original_video_samples: &[SampleInfo],
) -> anyhow::Result<Vec<u64>> {
    snapped_video_ranges
        .iter()
        .map(|range| {
            let decode = video_order.to_decode(range.start.snapped).ok_or_else(|| {
                anyhow::anyhow!(
                    "検査5(音声の位置)に失敗: 表示順インデックス{}に対応するデコード順 \
                     インデックスが見つかりません",
                    range.start.snapped.0
                )
            })?;
            crate::mp4io::order_map::decode_timestamp(original_video_samples, decode).ok_or_else(
                || {
                    anyhow::anyhow!(
                        "検査5(音声の位置)に失敗: デコード順インデックス{}が映像サンプルの \
                         範囲外です",
                        decode.0
                    )
                },
            )
        })
        .collect()
}

/// `audio_samples[..idx]` の duration 累積（音声トラックの timescale 単位、
/// `audio::select_audio_segments` の `cumulative` と同じ定義）。
fn cumulative_audio_duration(audio_samples: &[SampleInfo], idx: usize) -> u64 {
    audio_samples[..idx.min(audio_samples.len())]
        .iter()
        .map(|s| s.duration as u64)
        .sum()
}

/// 検査5（音声が正しい位置から取れていること）。
///
/// 区間ごとに次の2点を確認する:
///
/// 1. **出力の先頭音声パケットのソース上の時刻**と `dts(区間先頭サンプル)`
///    （`independent_source_starts[k]`）の差が、半パケット + 1パケット長以内である
///    こと（音声パケット長は一定でないため緩い上限。先頭のプライミングパケットが
///    平均よりかなり長いことがあるのを考慮している）。
/// 2. **出力の先頭音声パケットのバイト列**が、**元ファイルの期待位置（DTS 基準で
///    独立に再計算した位置）に対応する音声パケット**のバイト列と一致すること。
///
/// # なぜこの検査が要るか
///
/// 検査6（音声ドリフトのログ、長さの一致）も `--verify` 指定時のffprobeパケット
/// 集合比較も、どちらも**中身が別の位置の音声でも通ってしまう**
/// （docs/lossless-cut.md「実際に起きた誤り」節）。過去に `select_audio_segments` が
/// 出力タイムライン上の累積時間を起点にしてしまい、出力の音声が常にソースの先頭から
/// 詰められるバグがあったが、上記2つの検査はどちらもこれを素通りさせた。
///
/// # 「期待位置」は `audio_segments` にも `diff_inputs.video_segment_source_starts`
/// にも頼らない
///
/// `audio_segments` の値自体が誤っていた場合にそれを検出できるよう、期待位置は
/// `audio_segments` を直接読むのではなく [`audio::select_audio_segments`] と同じ
/// アルゴリズムで独立に再計算する。**このとき目標時刻には
/// `diff_inputs.video_segment_source_starts` ではなく `independent_source_starts`
/// （呼び出し元が元ファイルの映像から独立に計算し直した DTS）を使う。** もし前者を
/// 使うと、`select_audio_segments` に実際に渡した値と検証に使う値が同じ計算経路
/// （`src/commands.rs::segment_video_source_starts`）から来ることになり、その経路
/// 自体に系統的な誤りがあっても検証が常に一致してしまう（過去に実際にこれが起きて
/// いた）。`independent_source_starts` は元ファイルの映像 `SampleInfo` から
/// [`independent_video_segment_source_starts`] で計算し直した、完全に別経路の値。
///
/// # バイト比較の方法
///
/// `output_audio_samples`（出力ファイルの音声 `SampleInfo` 列）と
/// `diff_inputs.audio_samples`（元ファイルの音声 `SampleInfo` 列）はどちらも
/// `file_offset` / `size` を持つため、ファイル全体を読み込まず `seek` した範囲
/// だけを読んで比較する（[`read_byte_range`]）。
///
/// `--video-only` 相当（4引数がすべて `None`）ならスキップする。一部だけ `None` の
/// 場合は呼び出し側の取り違えとみなしエラーにする。
fn verify_audio_segment_positions(
    input_path: &Path,
    output_path: &Path,
    audio_segments: Option<&[AudioSegment]>,
    output_audio_samples: Option<&[SampleInfo]>,
    diff_inputs: Option<AudioDiffInputs<'_>>,
    independent_source_starts: Option<&[u64]>,
) -> anyhow::Result<()> {
    let (audio_segments, output_audio_samples, diff_inputs, independent_source_starts) = match (
        audio_segments,
        output_audio_samples,
        diff_inputs,
        independent_source_starts,
    ) {
        (None, None, None, None) => return Ok(()),
        (Some(segments), Some(output_samples), Some(inputs), Some(starts)) => {
            (segments, output_samples, inputs, starts)
        }
        _ => {
            return Err(anyhow::anyhow!(
                "audio_segments / output_audio_samples / audio_samples_for_diff / \
                     independent_source_starts は すべて Some かすべて None である必要が \
                     あります(音声ありの出力なら すべて指定し、--video-only 相当ならすべて \
                     None にしてください)"
            ))
        }
    };

    anyhow::ensure!(
        audio_segments.len() == diff_inputs.video_segment_durations.len(),
        "検査5(音声の位置)に失敗: audio_segments の区間数({}) と \
         video_segment_durations の区間数({})が一致しません",
        audio_segments.len(),
        diff_inputs.video_segment_durations.len()
    );
    anyhow::ensure!(
        audio_segments.len() == independent_source_starts.len(),
        "検査5(音声の位置)に失敗: audio_segments の区間数({}) と \
         independent_source_starts の区間数({})が一致しません",
        audio_segments.len(),
        independent_source_starts.len()
    );

    // 「期待される音声区間」を audio_segments とは独立に、独立に再計算した DTS
    // (independent_source_starts) から再計算する(select_audio_segments と同じ
    // アルゴリズム)。audio_segments の値自体が誤っていた場合にそれを検出するため、
    // ここでは audio_segments を一切参照しない。
    let (expected_segments, _drift) = audio::select_audio_segments(
        diff_inputs.video_segment_durations,
        independent_source_starts,
        diff_inputs.video_timescale,
        diff_inputs.audio_samples,
        diff_inputs.audio_timescale,
    )
    .context("検査5(音声の位置)に失敗: 期待される音声区間の再計算に失敗しました")?;
    anyhow::ensure!(
        expected_segments.len() == audio_segments.len(),
        "検査5(音声の位置)に失敗: 再計算した期待区間数({}) が audio_segments の \
         区間数({})と一致しません",
        expected_segments.len(),
        audio_segments.len()
    );

    // 誤差を「パケット単位」に正規化するための典型的な1パケットの長さ。実データは
    // 先頭パケットがプライミング分だけ外れ値になりうるので全体の平均を使う
    // （src/audio.rs::select_audio_segments の average_frame_size と同じ考え方）。
    let average_frame_size = if diff_inputs.audio_samples.is_empty() {
        1.0
    } else {
        let total: u64 = diff_inputs
            .audio_samples
            .iter()
            .map(|s| s.duration as u64)
            .sum();
        total as f64 / diff_inputs.audio_samples.len() as f64
    };
    // 半パケット + 1パケット長。音声パケット長は一定でない(先頭のプライミング
    // パケットが特に長い)ため、緩めの上限にしている。67msのような系統的なずれ
    // （20msグリッドのOpusなら3パケット分程度）はこの上限を確実に超える。
    const MAX_ALLOWED_ERROR_PACKETS: f64 = 1.5;

    // audio_segments は出力に書き出された順そのものなので、区間ごとの長さ
    // (end - start) を先頭から積算するだけで、出力ファイル内でのその区間の
    // 先頭パケットのインデックスが求まる。
    let mut output_cursor = 0usize;
    for (i, ((seg, expected), &independent_start)) in audio_segments
        .iter()
        .zip(expected_segments.iter())
        .zip(independent_source_starts.iter())
        .enumerate()
    {
        let seg_len = (seg.end - seg.start) as usize;
        if seg_len == 0 {
            // 空区間(パケット無し)は先頭パケットが存在しないため比較しようがない。
            continue;
        }

        // 検査5-1: 「出力の先頭音声パケットのソース上の時刻」(実際に cut が選んだ
        // audio_segments[i].start を元ファイルの音声で見た累積時間) と
        // 「dts(区間先頭サンプル)」(independent_start を音声 timescale に変換した
        // 目標時刻)の差が緩い上限以内であること。
        let actual_source_time =
            cumulative_audio_duration(diff_inputs.audio_samples, seg.start.0 as usize) as f64;
        let target_time = (independent_start as f64 * diff_inputs.audio_timescale as f64)
            / diff_inputs.video_timescale as f64;
        let error_packets = (actual_source_time - target_time).abs() / average_frame_size;
        anyhow::ensure!(
            error_packets <= MAX_ALLOWED_ERROR_PACKETS,
            "検査5(音声の位置)に失敗: 区間{}の出力先頭音声パケットのソース時刻({actual_source_time:.0}) \
             が dts(区間先頭サンプル)から音声timescaleに変換した目標時刻({target_time:.0})から \
             {error_packets:.2}パケット分ずれています(許容{MAX_ALLOWED_ERROR_PACKETS}パケット)。 \
             合成時刻(PTS)とDTSを取り違えていないか確認すること \
             (docs/lossless-cut.md「実際に起きた誤り」参照)",
            i + 1
        );

        let output_sample = *output_audio_samples.get(output_cursor).ok_or_else(|| {
            anyhow::anyhow!(
                "検査5(音声の位置)に失敗: 区間{}の出力側先頭音声パケット(インデックス{}) \
                 が出力の音声サンプル総数({})の範囲外です",
                i + 1,
                output_cursor,
                output_audio_samples.len()
            )
        })?;

        let expected_start = expected.start.0 as usize;
        let expected_sample = *diff_inputs
            .audio_samples
            .get(expected_start)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "検査5(音声の位置)に失敗: 区間{}の期待される先頭音声パケット \
                 (元ファイルのインデックス{}) が元ファイルの音声サンプル総数({}) \
                 の範囲外です",
                    i + 1,
                    expected_start,
                    diff_inputs.audio_samples.len()
                )
            })?;

        let output_bytes =
            read_byte_range(output_path, output_sample.file_offset, output_sample.size)
                .with_context(|| {
                    format!(
                        "検査5(音声の位置)に失敗: 区間{}の出力側パケットの読み込みに失敗しました",
                        i + 1
                    )
                })?;
        let expected_bytes = read_byte_range(
            input_path,
            expected_sample.file_offset,
            expected_sample.size,
        )
        .with_context(|| {
            format!(
                "検査5(音声の位置)に失敗: 区間{}の元ファイル側パケットの読み込みに失敗しました",
                i + 1
            )
        })?;

        anyhow::ensure!(
            output_bytes == expected_bytes,
            "検査5(音声の位置)に失敗: 区間{}の出力の先頭音声パケット(出力インデックス{}, \
             {}バイト)が元ファイルの期待位置(インデックス{}, {}バイト)と一致しません。 \
             出力の音声が出力タイムライン上の累積時間から詰められていないか確認すること \
             (docs/lossless-cut.md「実際に起きた誤り」参照)",
            i + 1,
            output_cursor,
            output_sample.size,
            expected_start,
            expected_sample.size
        );

        output_cursor += seg_len;
    }

    Ok(())
}

/// `path` の `offset` から `size` バイトだけを読む。ファイル全体は読み込まない。
fn read_byte_range(path: &Path, offset: u64, size: u32) -> anyhow::Result<Vec<u8>> {
    let mut file = File::open(path)
        .with_context(|| format!("ファイル({})を開けませんでした", path.display()))?;
    file.seek(SeekFrom::Start(offset)).with_context(|| {
        format!(
            "ファイル({})の{offset}バイト目へのシークに失敗しました",
            path.display()
        )
    })?;
    let mut buf = vec![0u8; size as usize];
    file.read_exact(&mut buf).with_context(|| {
        format!(
            "ファイル({})の{offset}..{}バイト目の読み込みに失敗しました",
            path.display(),
            offset + size as u64
        )
    })?;
    Ok(buf)
}

/// 検査6（音声の累積誤差の最大値をログ出力する）。
///
/// `--video-only` 相当（`audio_segments` / `audio_samples_for_diff` がどちらも `None`）
/// の場合は音声処理そのものをスキップする。どちらか一方だけが `None` の場合は呼び出し側
/// の取り違えとみなしエラーにする。
fn log_audio_drift(
    audio_segments: Option<&[AudioSegment]>,
    audio_samples_for_diff: Option<AudioDiffInputs<'_>>,
) -> anyhow::Result<Option<AvSyncReport>> {
    match (audio_segments, audio_samples_for_diff) {
        (None, None) => Ok(None),
        (Some(segments), Some(inputs)) => {
            let report = audio::av_sync_report(
                inputs.video_segment_durations,
                inputs.video_timescale,
                segments,
                inputs.audio_samples,
                inputs.audio_timescale,
            )
            .context("検査6(音声ドリフトのログ)に失敗: av_sync_report の計算に失敗しました")?;
            eprintln!("{}", audio::format_av_sync_report(&report));
            Ok(Some(report))
        }
        _ => Err(anyhow::anyhow!(
            "audio_segments と audio_samples_for_diff は両方 Some か両方 None である \
             必要があります(音声ありの出力なら両方指定し、--video-only 相当なら両方 \
             None にしてください)"
        )),
    }
}

// ---------------------------------------------------------------------
// ffprobe によるパケット単位 CRC32 の一致確認（`--verify` 指定時、CLAUDE.md の罠2）。
// ---------------------------------------------------------------------

/// 映像ストリームの全パケットの CRC32 を、コンテナの格納順
/// （= デコード順、[`crate::mp4io::read::samples`] が返す `SampleInfo` と同じ順序）で
/// 取得する。`video_keep`（元ファイルのデコード順 `DecodeIdx` 列）のインデックスと
/// そのまま対応するので、`-ss` によるタイムスタンプベースのシークが不要になる。
fn video_packet_crc32_in_decode_order(
    ffprobe_path: &Path,
    path: &Path,
) -> anyhow::Result<Vec<String>> {
    crate::ffprobe::csv_rows(ffprobe_path, path, "v:0", "packet=size,data_hash")
}

/// 音声ストリームの全パケットの CRC32 集合を取得する（集合比較のため）。
fn audio_packet_crc32_set(ffprobe_path: &Path, path: &Path) -> anyhow::Result<HashSet<String>> {
    Ok(
        crate::ffprobe::csv_rows(ffprobe_path, path, "a:0", "packet=data_hash")?
            .into_iter()
            .collect(),
    )
}

/// 音声ストリームの全パケットの dts を格納順に取得する（順序検証用。集合比較では
/// 順序や重複を検出できないため、これで補う）。
fn audio_packet_dts(ffprobe_path: &Path, path: &Path) -> anyhow::Result<Vec<i64>> {
    crate::ffprobe::csv_rows(ffprobe_path, path, "a:0", "packet=dts")?
        .into_iter()
        .map(|line| {
            line.parse::<i64>().with_context(|| {
                format!("音声パケットの dts が整数としてパースできません: {line:?}")
            })
        })
        .collect()
}

/// 映像: `video_keep` が指す元ファイルのデコード順インデックス列から「期待される
/// CRC32列」を組み立て、出力の実際のCRC32列と1対1で比較する。不一致なら最初に食い違った
/// パケット番号を含むエラーを返す。
fn verify_video_packets_with_ffprobe(
    ffprobe_path: &Path,
    input_path: &Path,
    output_path: &Path,
    video_keep: &[DecodeIdx],
) -> anyhow::Result<()> {
    let original_crc32 = video_packet_crc32_in_decode_order(ffprobe_path, input_path)
        .context("元ファイルの映像パケットCRC32の取得に失敗しました")?;

    let expected: Vec<&str> = video_keep
        .iter()
        .map(|idx| {
            original_crc32
                .get(idx.0 as usize)
                .map(String::as_str)
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "video_keep が指すデコード順インデックス({})が元ファイルの映像 \
                         パケット総数({})の範囲外です",
                        idx.0,
                        original_crc32.len()
                    )
                })
        })
        .collect::<anyhow::Result<Vec<_>>>()?;

    let got = video_packet_crc32_in_decode_order(ffprobe_path, output_path)
        .context("出力ファイルの映像パケットCRC32の取得に失敗しました")?;

    anyhow::ensure!(
        got.len() == expected.len(),
        "映像パケットのCRC32比較に失敗: 出力のパケット数({}) が期待値({})と一致しません",
        got.len(),
        expected.len()
    );

    for (i, (g, e)) in got.iter().zip(expected.iter()).enumerate() {
        anyhow::ensure!(
            g == e,
            "映像パケットのCRC32比較に失敗: 最初に食い違ったパケット番号 = {i} \
             (出力={g:?}, 期待値(元ファイルのデコード順インデックス{})={e:?})",
            video_keep[i].0
        );
    }

    Ok(())
}

/// 音声: 出力の全パケットが元ファイルのパケット集合に含まれるか（集合比較）を確認し、
/// 加えて出力の音声パケットの dts が単調増加であることを assert する
/// （集合比較では順序や重複を検出できないため）。
fn verify_audio_packets_with_ffprobe(
    ffprobe_path: &Path,
    input_path: &Path,
    output_path: &Path,
) -> anyhow::Result<()> {
    let src_set = audio_packet_crc32_set(ffprobe_path, input_path)
        .context("元ファイルの音声パケットCRC32の取得に失敗しました")?;
    let out_set = audio_packet_crc32_set(ffprobe_path, output_path)
        .context("出力ファイルの音声パケットCRC32の取得に失敗しました")?;

    let mut diff: Vec<&String> = out_set.difference(&src_set).collect();
    diff.sort();
    anyhow::ensure!(
        diff.is_empty(),
        "音声パケットの集合比較に失敗: 出力にしか存在しない音声パケットが{}件あります \
         (先頭5件: {:?})。ビットコピーではない音声パケットが含まれている可能性があります",
        diff.len(),
        diff.iter().take(5).collect::<Vec<_>>()
    );

    let dts = audio_packet_dts(ffprobe_path, output_path)
        .context("出力の音声パケットのdts取得に失敗しました")?;
    for (i, w) in dts.windows(2).enumerate() {
        anyhow::ensure!(
            w[1] > w[0],
            "出力の音声パケットのdtsが単調増加ではありません(パケット{i}→{}: {} -> {})",
            i + 1,
            w[0],
            w[1]
        );
    }

    Ok(())
}

/// [`cut_and_verify`] が成功した後（`output_path` へ rename 済み）に呼ぶ。
///
/// ffprobe でパケット単位の CRC32 を突き合わせ、不一致なら `output_path` を削除して
/// エラーを返す（CLAUDE.md 罠2「無劣化の検証にmd5を使わない」の実装）。
///
/// - 映像: `video_keep` から期待される CRC32 列を組み立て、出力の実際の CRC32 列と
///   1対1で比較する。
/// - 音声: `has_audio` が真の場合、出力の全パケットが元ファイルのパケット集合に
///   含まれるか（集合比較）、および出力の音声パケットの dts が単調増加で
///   あるかを確認する。
///
/// `ffprobe_path` は呼び出し側が [`crate::tools::resolve_tool`] で解決済みのものを渡す
/// （`--verify` 指定時に ffprobe が見つからない場合にエラーにするのは呼び出し側の責務。
/// 本関数は解決済みのパスを受け取るだけでよい）。
///
/// `-show_data_hash CRC32` は全パケットを読むためファイルサイズなりの時間がかかる。
/// かかった時間は `eprintln!` でログに出す。
pub fn verify_with_ffprobe(
    input_path: &Path,
    output_path: &Path,
    video_keep: &[DecodeIdx],
    has_audio: bool,
    ffprobe_path: &Path,
) -> anyhow::Result<()> {
    let started = Instant::now();

    let outcome: anyhow::Result<()> = (|| {
        verify_video_packets_with_ffprobe(ffprobe_path, input_path, output_path, video_keep)?;
        if has_audio {
            verify_audio_packets_with_ffprobe(ffprobe_path, input_path, output_path)?;
        }
        Ok(())
    })();

    eprintln!(
        "[verify] ffprobe によるパケット単位のCRC32検証にかかった時間: {:.3}秒 (対象: {})",
        started.elapsed().as_secs_f64(),
        output_path.display()
    );

    if outcome.is_err() {
        // 1つでも不一致があれば、既に rename 済みの本番出力を破棄する。
        let _ = std::fs::remove_file(output_path);
    }

    outcome
}

/// `--verify` 指定時の一連の流れ: [`cut_and_verify`] → [`verify_with_ffprobe`]。
///
/// `ffprobe_path` は呼び出し側が `tools::resolve_tool` で解決済みのものを渡す設計
/// （見つからない場合に呼び出し側でエラーにする）。
#[allow(clippy::too_many_arguments)]
pub fn cut_verify_and_ffprobe_check(
    input_path: &Path,
    output_path: &Path,
    moov: &Moov,
    video_track_index: usize,
    audio_track_index: Option<usize>,
    snapped_video_ranges: &[SnappedRange],
    video_keep: &[DecodeIdx],
    audio_segments: Option<&[AudioSegment]>,
    video_order: &OrderMap,
    dtvi: Option<&Dtvi>,
    audio_samples_for_diff: Option<AudioDiffInputs<'_>>,
    ffprobe_path: &Path,
) -> anyhow::Result<VerifyReport> {
    let report = cut_and_verify(
        input_path,
        output_path,
        moov,
        video_track_index,
        audio_track_index,
        snapped_video_ranges,
        video_keep,
        audio_segments,
        video_order,
        dtvi,
        audio_samples_for_diff,
    )?;

    verify_with_ffprobe(
        input_path,
        output_path,
        video_keep,
        audio_track_index.is_some(),
        ffprobe_path,
    )?;

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Snap;
    use crate::plan::{self, SnappedBoundary};
    use crate::trim::TrimList;
    use crate::{dtvi, order::DisplayIdx};

    // ---------------------------------------------------------------
    // 合成データによる検査1・3の単体テスト（verify_video_ranges を直接呼ぶ）。
    // ---------------------------------------------------------------

    fn sample(is_sync: bool) -> SampleInfo {
        SampleInfo {
            file_offset: 0,
            size: 10,
            duration: 1001,
            cts_offset: 0,
            is_sync,
        }
    }

    fn boundary(v: u32) -> SnappedBoundary {
        SnappedBoundary {
            original: DisplayIdx(v),
            snapped: DisplayIdx(v),
            delta_frames: 0,
        }
    }

    fn range(start: u32, end: u32) -> SnappedRange {
        SnappedRange {
            start: boundary(start),
            end: boundary(end),
        }
    }

    #[test]
    fn verify_video_ranges_succeeds_on_well_formed_output() {
        // 区間0: [0,4) 4パケット、区間1: [10,13) 3パケット。先頭はどちらも同期サンプル。
        let mut output = vec![sample(true), sample(false), sample(false), sample(false)];
        output.extend([sample(true), sample(false), sample(false)]);

        let ranges = vec![range(0, 4), range(10, 13)];
        verify_video_ranges(&output, &ranges).expect("正しいデータは検査を通るはず");
    }

    #[test]
    fn verify_video_ranges_fails_when_a_range_is_dropped() {
        // 想定は2区間(4パケット+3パケット=7パケット)だが、出力は1区間分(4パケット)しかない
        // (「範囲を1つ削る」の再現)。
        let output = vec![sample(true), sample(false), sample(false), sample(false)];
        let ranges = vec![range(0, 4), range(10, 13)];

        let err = verify_video_ranges(&output, &ranges).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("検査1"), "message={message}");
        assert!(message.contains('7'), "期待合計7を含むこと: {message}");
        assert!(message.contains('4'), "実際の総数4を含むこと: {message}");
    }

    #[test]
    fn verify_video_ranges_fails_when_range_start_is_not_a_sync_sample() {
        // 区間1の先頭が同期サンプルではない(区間の開始位置がずれた場合の再現)。
        let mut output = vec![sample(true), sample(false), sample(false), sample(false)];
        output.extend([sample(false), sample(false), sample(false)]);

        let ranges = vec![range(0, 4), range(10, 13)];
        let err = verify_video_ranges(&output, &ranges).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("検査3"), "message={message}");
        assert!(message.contains('2'), "区間番号2を含むこと: {message}");
    }

    // ---------------------------------------------------------------
    // 合成データによる検査2の単体テスト
    // (verify_display_order_is_contiguous を直接呼ぶ)。
    // ---------------------------------------------------------------

    #[test]
    fn verify_display_order_succeeds_without_reordering_bugs() {
        // B フレーム的な並べ替えを含むが、合成時刻に重複は無い正常なデータ。
        let output = vec![
            SampleInfo {
                file_offset: 0,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: true,
            },
            SampleInfo {
                file_offset: 10,
                size: 10,
                duration: 1000,
                cts_offset: 3000,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 20,
                size: 10,
                duration: 1000,
                cts_offset: -1000,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 30,
                size: 10,
                duration: 1000,
                cts_offset: -1000,
                is_sync: false,
            },
        ];

        verify_display_order_is_contiguous(&output).expect("重複のない合成時刻は検査を通るはず");
    }

    #[test]
    fn verify_display_order_fails_when_reordering_creates_a_tie() {
        // 「順序を入れ替える」ことで合成時刻が衝突するケース
        // (decode3 の cts_offset を、decode2 と同じ合成時刻になるよう変更した)。
        let output = vec![
            SampleInfo {
                file_offset: 0,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: true,
            },
            SampleInfo {
                file_offset: 10,
                size: 10,
                duration: 1000,
                cts_offset: 3000,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 20,
                size: 10,
                duration: 1000,
                cts_offset: -1000, // dts=2000, cts=1000
                is_sync: false,
            },
            SampleInfo {
                file_offset: 30,
                size: 10,
                duration: 1000,
                cts_offset: -2000, // dts=3000, cts=1000 (decode2 と衝突)
                is_sync: false,
            },
        ];

        let err = verify_display_order_is_contiguous(&output).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("検査2"), "message={message}");
    }

    // ---------------------------------------------------------------
    // 実フィクスチャ + .dtvi を使った統合テスト。
    // ---------------------------------------------------------------

    // cwd 非依存にする（`external::tests` がプロセスの cwd を一時的に変えるため）。
    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.mp4");
    const DTVI_SAMPLE: &str = include_str!("../tests/data/sample.dtvi");

    fn skip_if_fixture_missing() -> bool {
        if Path::new(FIXTURE).exists() {
            return false;
        }
        eprintln!(
            "{FIXTURE} が無いためスキップします。`tests/fixtures/gen.sh` を実行してください。"
        );
        true
    }

    /// フィクスチャ(GOP120・599フレーム)から、cut_and_verify を呼ぶのに必要な一式を
    /// 組み立てる。テストごとに個別の要素を壊してから `cut_and_verify` に渡す。
    struct Fixture {
        input_path: PathBuf,
        moov: Moov,
        video_track_index: usize,
        audio_track_index: usize,
        snapped: Vec<SnappedRange>,
        video_keep: Vec<DecodeIdx>,
        audio_segments: Vec<AudioSegment>,
        video_order: OrderMap,
        dtvi: Dtvi,
        video_segment_durations: Vec<u64>,
        video_segment_source_starts: Vec<u64>,
        video_timescale: u32,
        audio_samples: Vec<SampleInfo>,
        audio_timescale: u32,
    }

    fn build_fixture() -> Fixture {
        let input_path = PathBuf::from(FIXTURE);
        let moov = read_moov(&input_path).expect("moov を読めること");

        let (video_trak, video_info) = find_video_track(&moov).expect("映像トラックが見つかること");
        let video_samples = samples(&video_trak.mdia.minf.stbl);
        let total_frames = video_samples.len() as u32;

        let map = DisplayDecodeMap::build(&video_samples).expect("同値の合成時刻は無いはず");
        let sync_display = map.sync_display_indices();

        // GOP=120・599フレームのフィクスチャ前提(video_e2e.rsと同じ値)。キーフレームから
        // わざとずらしたTrimを outward スナップし、[0,120) と [360,480) の2区間にする。
        let trim =
            TrimList::parse("Trim(10,109) ++ Trim(370,469)").expect("Trim をパースできること");
        let snapped = plan::snap(&trim, &sync_display, total_frames, Snap::Outward)
            .expect("スナップ後の区間が重ならないこと");
        let video_keep = plan::keep_list(&snapped, &map.order).expect("keep_list が成功すること");

        let (audio_trak, audio_info) = find_audio_track(&moov).expect("音声トラックが見つかること");
        let audio_samples = samples(&audio_trak.mdia.minf.stbl);

        // 区間ごとの映像再生時間(音声ドリフト計算用)。keep_list と同じ区切り方をする。
        let mut video_segment_durations = Vec::new();
        let mut cursor = 0usize;
        for r in &snapped {
            let count = (r.end.snapped - r.start.snapped) as usize;
            let duration: u64 = video_keep[cursor..cursor + count]
                .iter()
                .map(|d| u64::from(video_samples[d.0 as usize].duration))
                .sum();
            video_segment_durations.push(duration);
            cursor += count;
        }

        // 区間ごとのソース上の絶対DTS(音声パケット選択の起点。src/commands.rs の
        // segment_video_source_starts と同じ計算)。合成時刻(PTS)ではなく DTS を
        // 使う理由、出力タイムライン上の累積時間ではないことは
        // segment_video_source_starts の doc comment 参照
        // (docs/lossless-cut.md「実際に起きた誤り」も参照)。
        let video_segment_source_starts: Vec<u64> = snapped
            .iter()
            .map(|r| {
                let decode = map
                    .order
                    .to_decode(r.start.snapped)
                    .expect("区間開始の表示順に対応するデコード順があるはず");
                crate::mp4io::order_map::decode_timestamp(&video_samples, decode)
                    .expect("デコード順が映像サンプルの範囲内のはず")
            })
            .collect();

        let (audio_segments, _drift) = crate::audio::select_audio_segments(
            &video_segment_durations,
            &video_segment_source_starts,
            video_info.timescale,
            &audio_samples,
            audio_info.timescale,
        )
        .expect("select_audio_segments が成功すること");

        let video_track_index = moov
            .trak
            .iter()
            .position(|t| {
                matches!(
                    t.mdia.minf.stbl.stsd.codecs.first(),
                    Some(mp4_atom::Codec::Avc1(_))
                )
            })
            .expect("映像トラックのインデックスが見つかること");
        let audio_track_index = moov
            .trak
            .iter()
            .position(|t| {
                t.mdia
                    .minf
                    .stbl
                    .stsd
                    .codecs
                    .first()
                    .is_some_and(crate::mp4io::read::is_audio_codec)
            })
            .expect("音声トラックのインデックスが見つかること");

        let dtvi = dtvi::parse(DTVI_SAMPLE).expect(".dtvi をパースできること");

        Fixture {
            input_path,
            moov,
            video_track_index,
            audio_track_index,
            snapped,
            video_keep,
            audio_segments,
            video_order: map.order,
            dtvi,
            video_segment_durations,
            video_segment_source_starts,
            video_timescale: video_info.timescale,
            audio_samples,
            audio_timescale: audio_info.timescale,
        }
    }

    /// テストごとに独立した一時ディレクトリを作る。
    fn make_tmp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tachikaze-verify-test-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("一時ディレクトリを作れること");
        dir
    }

    #[test]
    fn cut_and_verify_succeeds_on_well_formed_fixture() {
        if skip_if_fixture_missing() {
            return;
        }
        let f = build_fixture();
        let tmp_dir = make_tmp_dir("happy-path");
        let output_path = tmp_dir.join("out.mp4");

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &f.video_segment_durations,
            video_segment_source_starts: &f.video_segment_source_starts,
            video_timescale: f.video_timescale,
            audio_samples: &f.audio_samples,
            audio_timescale: f.audio_timescale,
        };

        let report = cut_and_verify(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &f.video_keep,
            Some(&f.audio_segments),
            &f.video_order,
            Some(&f.dtvi),
            Some(diff_inputs),
        )
        .expect("正常なフィクスチャは全検査を通るはず");

        assert!(output_path.exists(), "出力ファイルが作られているはず");
        assert_eq!(report.video_packet_count, f.video_keep.len());
        assert_eq!(report.video_range_count, f.snapped.len());
        assert!(report.av_sync.is_some());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn cut_and_verify_requires_dtvi_by_default() {
        if skip_if_fixture_missing() {
            return;
        }
        let f = build_fixture();
        let tmp_dir = make_tmp_dir("no-dtvi");
        let output_path = tmp_dir.join("out.mp4");

        let err = cut_and_verify(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &f.video_keep,
            Some(&f.audio_segments),
            &f.video_order,
            None,
            None,
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(
            message.contains("analyze"),
            ".dtvi が無い場合は analyze を促すこと: {message}"
        );
        assert!(!output_path.exists());
        assert!(
            std::fs::read_dir(&tmp_dir).unwrap().next().is_none(),
            "一時ファイルも含めて何も残っていないこと"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn cut_and_verify_discards_output_when_a_range_is_dropped_from_keep_list() {
        if skip_if_fixture_missing() {
            return;
        }
        let f = build_fixture();
        let tmp_dir = make_tmp_dir("check1");
        let output_path = tmp_dir.join("out.mp4");

        // 区間ごとの内訳(120パケット + 120パケット)から、後半区間をまるごと削る
        // (「範囲を1つ削る」の再現)。
        let first_range_len = (f.snapped[0].end.snapped - f.snapped[0].start.snapped) as usize;
        let broken_keep = f.video_keep[..first_range_len].to_vec();

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &f.video_segment_durations,
            video_segment_source_starts: &f.video_segment_source_starts,
            video_timescale: f.video_timescale,
            audio_samples: &f.audio_samples,
            audio_timescale: f.audio_timescale,
        };

        let err = cut_and_verify(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &broken_keep,
            Some(&f.audio_segments),
            &f.video_order,
            Some(&f.dtvi),
            Some(diff_inputs),
        )
        .unwrap_err();

        assert!(err.to_string().contains("検査1"));
        assert!(!output_path.exists(), "出力ファイルが残っていないこと");
        assert!(
            std::fs::read_dir(&tmp_dir).unwrap().next().is_none(),
            "一時ファイルも含めて何も残っていないこと(ディレクトリが空であること)"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn cut_and_verify_discards_output_when_dtvi_row_is_corrupted() {
        if skip_if_fixture_missing() {
            return;
        }
        let mut f = build_fixture();
        let tmp_dir = make_tmp_dir("check4");
        let output_path = tmp_dir.join("out.mp4");

        // 実際に dtvindex で生成した .dtvi データを1行だけ書き換える
        // (order_map.rs の検証と同じ手法)。
        let corrupted_frame_number = f.dtvi.frames[5].frame_number.0;
        f.dtvi.frames[5].sample_number = DecodeIdx(9999);

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &f.video_segment_durations,
            video_segment_source_starts: &f.video_segment_source_starts,
            video_timescale: f.video_timescale,
            audio_samples: &f.audio_samples,
            audio_timescale: f.audio_timescale,
        };

        let err = cut_and_verify(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &f.video_keep,
            Some(&f.audio_segments),
            &f.video_order,
            Some(&f.dtvi),
            Some(diff_inputs),
        )
        .unwrap_err();

        let message = err.to_string();
        assert!(message.contains("検査4"), "message={message}");
        assert!(
            message.contains(&corrupted_frame_number.to_string())
                || message.contains("9999")
                || err.chain().any(|c| c.to_string().contains("9999")),
            "行番号または壊した値がエラーに含まれること: {message}"
        );

        assert!(!output_path.exists(), "出力ファイルが残っていないこと");
        assert!(
            std::fs::read_dir(&tmp_dir).unwrap().next().is_none(),
            "一時ファイルも含めて何も残っていないこと(ディレクトリが空であること)"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn cut_and_verify_rejects_inconsistent_audio_arguments() {
        if skip_if_fixture_missing() {
            return;
        }
        let f = build_fixture();
        let tmp_dir = make_tmp_dir("audio-mismatch");
        let output_path = tmp_dir.join("out.mp4");

        // audio_track_index はあるのに audio_segments が無い(取り違えの再現)。
        let err = cut_and_verify(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &f.video_keep,
            None,
            &f.video_order,
            Some(&f.dtvi),
            None,
        )
        .unwrap_err();

        assert!(err.to_string().contains("audio_track_index"));
        assert!(!output_path.exists());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn log_audio_drift_skips_for_video_only() {
        let result = log_audio_drift(None, None).expect("video-only はエラーにならないはず");
        assert!(result.is_none());
    }

    #[test]
    fn log_audio_drift_rejects_mismatched_optionals() {
        let audio_samples = vec![sample(true)];
        let durations = [1001u64];
        let segments = [AudioSegment {
            start: DecodeIdx(0),
            end: DecodeIdx(1),
        }];

        // audio_segments は Some だが audio_samples_for_diff が None。
        assert!(log_audio_drift(Some(&segments), None).is_err());

        let inputs = AudioDiffInputs {
            video_segment_durations: &durations,
            video_segment_source_starts: &[0u64],
            video_timescale: 30000,
            audio_samples: &audio_samples,
            audio_timescale: 48000,
        };
        // 逆に audio_segments が None だが audio_samples_for_diff が Some。
        assert!(log_audio_drift(None, Some(inputs)).is_err());
    }

    // ---------------------------------------------------------------
    // 検査5 (verify_audio_segment_positions) の単体テスト。
    // ---------------------------------------------------------------

    /// `verify_audio_segment_positions` のバイト比較テスト用に、一意な一時ファイルの
    /// パスを作る(親ディレクトリは存在する状態にする。内容は呼び出し側が書く)。
    fn temp_file_path(tag: &str) -> PathBuf {
        let dir = make_tmp_dir(&format!("audio-position-{tag}"));
        dir.join("data.bin")
    }

    /// `count` 個、`size` バイトずつのダミー `SampleInfo` 列を組み立てる。
    /// サンプル `i` は `offset = i * size` に置かれている前提
    /// (対応するファイルの内容もこのレイアウトで書く)。
    fn synthetic_audio_samples(count: u32, size: u32) -> Vec<SampleInfo> {
        (0..count)
            .map(|i| SampleInfo {
                file_offset: (i * size) as u64,
                size,
                duration: 1000,
                cts_offset: 0,
                is_sync: true,
            })
            .collect()
    }

    /// `sample_count` 個、`size` バイトずつのバイト列を組み立てる。サンプル `i` の
    /// 中身は `size` バイトすべて値 `i` で埋める(位置によって内容が変わるようにして、
    /// 「別の位置の音声を読んでいないか」を判別できるようにする)。
    fn synthetic_audio_bytes(sample_count: u8, size: u32) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(sample_count as usize * size as usize);
        for i in 0..sample_count {
            bytes.extend(std::iter::repeat_n(i, size as usize));
        }
        bytes
    }

    #[test]
    fn verify_audio_segment_positions_skips_for_video_only() {
        let input = PathBuf::from("/nonexistent-input.mp4");
        let output = PathBuf::from("/nonexistent-output.mp4");
        verify_audio_segment_positions(&input, &output, None, None, None, None)
            .expect("video-only(すべてNone)はエラーにならないはず");
    }

    #[test]
    fn verify_audio_segment_positions_rejects_mismatched_optionals() {
        let input = PathBuf::from("/nonexistent-input.mp4");
        let output = PathBuf::from("/nonexistent-output.mp4");
        let segments = [AudioSegment {
            start: DecodeIdx(0),
            end: DecodeIdx(1),
        }];

        // audio_segments だけ Some、他は None(取り違えの再現)。
        let err =
            verify_audio_segment_positions(&input, &output, Some(&segments), None, None, None)
                .unwrap_err();
        assert!(err.to_string().contains("すべて Some かすべて None"));
    }

    #[test]
    fn verify_audio_segment_positions_succeeds_when_output_matches_source_position() {
        // 元ファイル: 5サンプル、各4バイト、サンプルiの中身は値iで埋める。
        let size = 4u32;
        let input_path = temp_file_path("succeeds-input");
        std::fs::write(&input_path, synthetic_audio_bytes(5, size))
            .expect("元ファイルを書けること");
        let audio_samples = synthetic_audio_samples(5, size);

        // 区間: 映像1区間、ソース開始時刻2000(=サンプル2個分)、長さ2000(=サンプル2個分)。
        // video/audio timescale を揃えているので、素の値のまま「期待される音声
        // サンプル範囲 = [2,4)」になる(src/audio.rs::select_audio_segments と同じ計算)。
        let video_segment_durations = [2000u64];
        let video_segment_source_starts = [2000u64];

        // 出力ファイル: 元ファイルのサンプル2,3をそのまま連結したもの
        // (正しく書き出された場合の内容)。
        let output_path = temp_file_path("succeeds-output");
        std::fs::write(&output_path, [2u8, 2, 2, 2, 3, 3, 3, 3]).expect("出力ファイルを書けること");
        let output_audio_samples = synthetic_audio_samples(2, size);

        // audio_segments はサンプル2,3を書き出したことを表す(正しい値)。
        let audio_segments = [AudioSegment {
            start: DecodeIdx(2),
            end: DecodeIdx(4),
        }];

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &video_segment_durations,
            video_segment_source_starts: &video_segment_source_starts,
            video_timescale: 1000,
            audio_samples: &audio_samples,
            audio_timescale: 1000,
        };

        verify_audio_segment_positions(
            &input_path,
            &output_path,
            Some(&audio_segments),
            Some(&output_audio_samples),
            Some(diff_inputs),
            Some(&video_segment_source_starts),
        )
        .expect("出力が期待位置と一致する場合は検査を通るはず");
    }

    /// 完了条件: 意図的にずれた `AudioSegment` を渡すと検査5が落ちること
    /// (docs/lossless-cut.md「実際に起きた誤り」の再現)。
    #[test]
    fn verify_audio_segment_positions_detects_shifted_audio_segment() {
        // 元ファイルは成功テストと同じ(5サンプル、各4バイト、値iで埋める)。
        let size = 4u32;
        let input_path = temp_file_path("shifted-input");
        std::fs::write(&input_path, synthetic_audio_bytes(5, size))
            .expect("元ファイルを書けること");
        let audio_samples = synthetic_audio_samples(5, size);

        // 正しい期待位置は成功テストと同じ(ソース開始時刻2000 -> サンプル[2,4))。
        let video_segment_durations = [2000u64];
        let video_segment_source_starts = [2000u64];

        // しかし実際に書き出されたのはサンプル[0,2)
        // (select_audio_segments が出力タイムライン上の累積時間(=0)を起点にして
        // しまった過去のバグを再現。audio_segments が意図的にずれている)。
        let output_path = temp_file_path("shifted-output");
        std::fs::write(&output_path, [0u8, 0, 0, 0, 1, 1, 1, 1]).expect("出力ファイルを書けること");
        let output_audio_samples = synthetic_audio_samples(2, size);

        // 意図的にずれた audio_segments (本来は{start:2,end:4}のはずが{start:0,end:2})。
        let audio_segments = [AudioSegment {
            start: DecodeIdx(0),
            end: DecodeIdx(2),
        }];

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &video_segment_durations,
            video_segment_source_starts: &video_segment_source_starts,
            video_timescale: 1000,
            audio_samples: &audio_samples,
            audio_timescale: 1000,
        };

        let err = verify_audio_segment_positions(
            &input_path,
            &output_path,
            Some(&audio_segments),
            Some(&output_audio_samples),
            Some(diff_inputs),
            Some(&video_segment_source_starts),
        )
        .unwrap_err();

        // 意図的なずれ(サンプル2個分 = 2000)は許容(1.5パケット=1500)を超えるため、
        // まず数値のずれチェック(検査5-1)で落ちる。
        let message = err.to_string();
        assert!(message.contains("検査5"), "message={message}");
        assert!(
            message.contains("ずれています"),
            "位置がずれていることが分かるメッセージであること: {message}"
        );
    }

    #[test]
    fn temp_output_path_is_distinct_from_output_and_in_same_directory() {
        let output = PathBuf::from("/tmp/some/dir/out.mp4");
        let tmp = temp_output_path(&output);

        assert_ne!(tmp, output);
        assert_eq!(tmp.parent(), output.parent());
        assert!(tmp
            .file_name()
            .unwrap()
            .to_string_lossy()
            .starts_with("out.mp4.verify-tmp-"));
    }

    // ---------------------------------------------------------------
    // ffprobe によるパケット単位 CRC32 の一致確認。
    // ---------------------------------------------------------------

    /// フィクスチャ/ffmpeg/ffprobe に加え、この一連のテストが必要とする ffprobe を
    /// 解決する。見つからなければ（スキップ理由を出力して）`None` を返す。
    fn skip_if_ffprobe_missing() -> Option<PathBuf> {
        match crate::tools::resolve_tool(crate::tools::FFPROBE) {
            Ok(path) => Some(path),
            Err(err) => {
                eprintln!("ffprobe が見つからないためスキップします: {err}");
                None
            }
        }
    }

    /// 完了条件1: フィクスチャで `cut --verify` 相当の処理（`cut_and_verify` →
    /// `verify_with_ffprobe`）が成功する。
    #[test]
    #[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
    fn cut_verify_and_ffprobe_check_succeeds_on_well_formed_fixture() {
        if skip_if_fixture_missing() {
            return;
        }
        let Some(ffprobe_path) = skip_if_ffprobe_missing() else {
            return;
        };

        let f = build_fixture();
        let tmp_dir = make_tmp_dir("ffprobe-happy-path");
        let output_path = tmp_dir.join("out.mp4");

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &f.video_segment_durations,
            video_segment_source_starts: &f.video_segment_source_starts,
            video_timescale: f.video_timescale,
            audio_samples: &f.audio_samples,
            audio_timescale: f.audio_timescale,
        };

        let report = cut_verify_and_ffprobe_check(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &f.video_keep,
            Some(&f.audio_segments),
            &f.video_order,
            Some(&f.dtvi),
            Some(diff_inputs),
            &ffprobe_path,
        )
        .expect("正常なフィクスチャは cut_and_verify + ffprobe 検証を通るはず");

        assert!(
            output_path.exists(),
            "ffprobe 検証も通った出力ファイルが残っているはず"
        );
        assert_eq!(report.video_packet_count, f.video_keep.len());

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    /// 完了条件2: `ffprobe` 不在の環境で `--verify` 相当を指定するとエラーになる。
    ///
    /// `tools::resolve_tool` が「見つからなければエラー」を正しく返すことを確認する。
    /// 現在のテストプロセスの `PATH` を直接書き換えると、並行に走る他のテスト
    /// （`tools.rs` 自身が `PATH` を書き換えるテストを持つ）と競合しうるため、
    /// 自分自身の実行ファイルを子プロセスとして起動し、その子プロセスの環境変数
    /// だけを空にして検証する（親プロセスの状態には一切触れない）。
    #[test]
    fn resolve_tool_ffprobe_not_found_when_path_is_empty() {
        let exe = std::env::current_exe().expect("自身の実行ファイルパスが取れること");

        let output = std::process::Command::new(&exe)
            .arg("--exact")
            .arg("verify::tests::assert_resolve_tool_fails_without_path_child")
            .arg("--ignored")
            .arg("--nocapture")
            .env("PATH", "")
            .env(CHILD_MARKER_ENV, "1")
            .output()
            .expect("子プロセス(自分自身)を起動できること");

        let stdout = String::from_utf8_lossy(&output.stdout);
        assert!(
            output.status.success(),
            "PATH が空の子プロセスで resolve_tool(FFPROBE) がエラーにならなかった: \
             stdout={stdout}\nstderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
        // 子が「隔離環境ではない」と判断して素通りした場合も終了コードは 0 に
        // なる。マーカーの受け渡しが壊れたときに、検証していないのに緑になる
        // のを防ぐため、実際にアサートまで到達した印を確認する。
        assert!(
            stdout.contains(CHILD_RAN_MARKER),
            "子プロセスが本体のアサートまで到達していない（{CHILD_MARKER_ENV} が \
             伝わっていない可能性）: stdout={stdout}"
        );
    }

    /// 下の子テストを「親から起動されたときだけ本体を実行する」ように切り替える
    /// ための環境変数。`cargo test -- --ignored` は `#[ignore]` の付いた子テスト
    /// も直接実行してしまい、その場合 `PATH` は通常どおり通っているので
    /// `resolve_tool` が成功してアサートが落ちる。ignore 属性だけでは
    /// 「親経由でのみ実行する」を表現できないため、マーカーで判別する。
    const CHILD_MARKER_ENV: &str = "TACHIKAZE_TEST_EMPTY_PATH_CHILD";

    /// 子テストが本体のアサートまで到達したことを親へ伝える印（標準出力）。
    const CHILD_RAN_MARKER: &str = "empty-path-child: asserted";

    /// 上のテストからのみ、子プロセスとして起動される。`--ignored` を付けているのは
    /// 単体の `cargo test` 実行では走らせず、必ず親経由の隔離された環境で実行するため。
    ///
    /// ただし `cargo test -- --ignored`（CLAUDE.md が E2E 用に挙げているコマンド）は
    /// このテストも直接実行する。そのときは `PATH` が通っているので本体を走らせると
    /// 必ず落ちる。[`CHILD_MARKER_ENV`] が無ければ隔離環境ではないと判断して
    /// 何もしない（親から起動されたかどうかは、この環境変数だけで判別する）。
    #[test]
    #[ignore = "resolve_tool_ffprobe_not_found_when_path_is_empty からのみ子プロセスとして起動する"]
    fn assert_resolve_tool_fails_without_path_child() {
        if std::env::var_os(CHILD_MARKER_ENV).is_none() {
            println!(
                "empty-path-child: {CHILD_MARKER_ENV} が無いため素通りする\
                 （親テスト resolve_tool_ffprobe_not_found_when_path_is_empty から実行される）"
            );
            return;
        }

        let result = crate::tools::resolve_tool(crate::tools::FFPROBE);
        assert!(
            result.is_err(),
            "PATH が空なら ffprobe は見つからないはず: {result:?}"
        );
        println!("{CHILD_RAN_MARKER}");
    }

    /// 完了条件3: 意図的に1パケット壊した出力で不一致が検出され、出力が破棄される。
    #[test]
    #[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
    fn verify_with_ffprobe_detects_and_discards_a_single_corrupted_video_packet() {
        if skip_if_fixture_missing() {
            return;
        }
        let Some(ffprobe_path) = skip_if_ffprobe_missing() else {
            return;
        };

        let f = build_fixture();
        let tmp_dir = make_tmp_dir("ffprobe-corrupt");
        let output_path = tmp_dir.join("out.mp4");

        let diff_inputs = AudioDiffInputs {
            video_segment_durations: &f.video_segment_durations,
            video_segment_source_starts: &f.video_segment_source_starts,
            video_timescale: f.video_timescale,
            audio_samples: &f.audio_samples,
            audio_timescale: f.audio_timescale,
        };

        // まず正常な出力を作る(cut_and_verify のみ。ffprobe 検証はまだ走らせない)。
        cut_and_verify(
            &f.input_path,
            &output_path,
            &f.moov,
            f.video_track_index,
            Some(f.audio_track_index),
            &f.snapped,
            &f.video_keep,
            Some(&f.audio_segments),
            &f.video_order,
            Some(&f.dtvi),
            Some(diff_inputs),
        )
        .expect("正常なフィクスチャは cut_and_verify を通るはず");

        // 壊す前提のセルフチェック: 壊す前は ffprobe 検証も通るはず。
        verify_with_ffprobe(
            &f.input_path,
            &output_path,
            &f.video_keep,
            true,
            &ffprobe_path,
        )
        .expect("壊す前は ffprobe 検証も通るはず");

        // 出力ファイルの映像パケットのうち1つのペイロードを1バイト壊す
        // (「意図的に1パケット壊す」の再現)。
        let output_moov = read_moov(&output_path).expect("出力moovを読めること");
        let (output_video_trak, _) =
            find_video_track(&output_moov).expect("出力に映像トラックがあること");
        let output_samples = samples(&output_video_trak.mdia.minf.stbl);
        let target = &output_samples[5];
        assert!(target.size > 4, "壊すのに十分なサイズのサンプルであること");

        {
            use std::io::{Read, Seek, SeekFrom, Write};
            let mut file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&output_path)
                .expect("出力ファイルを開けること");
            let offset = target.file_offset + u64::from(target.size) / 2;
            file.seek(SeekFrom::Start(offset)).expect("seek できること");
            let mut byte = [0u8; 1];
            file.read_exact(&mut byte).expect("1バイト読めること");
            byte[0] ^= 0xFF;
            file.seek(SeekFrom::Start(offset)).expect("seek できること");
            file.write_all(&byte).expect("1バイト書き込めること");
        }

        let err = verify_with_ffprobe(
            &f.input_path,
            &output_path,
            &f.video_keep,
            true,
            &ffprobe_path,
        )
        .expect_err("1パケット壊した出力はffprobe検証で検出されるはず");
        let message = err.to_string();
        assert!(
            message.contains("食い違った"),
            "最初に食い違ったパケット番号を含むこと: {message}"
        );

        assert!(
            !output_path.exists(),
            "不一致が検出されたら出力ファイルが破棄されているはず"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
