// cut パイプラインの CLI 配線（別 issue）から呼ばれるまで未使用。配線されたら外す。
#![allow(dead_code)]

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
//! 3. 音声を含む場合、A/V ずれの最大値をログ出力する（検査5）。
//! 4. 1つでも失敗すれば一時ファイルを削除してエラーを返す。すべて通れば
//!    `std::fs::rename` で `output_path` へ移動する。
//!
//! ffprobe によるパケット単位 CRC32 の一致確認（`--verify` 指定時、CLAUDE.md の罠2）は
//! 別 issue（#37）が担当し、本モジュールでは実装しない。
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
//! `DisplayDecodeMap` が `.dtvi` の全行と一致すること」（`#27` の
//! [`crate::mp4io::order_map::verify_against_dtvi`]）をもう一度確認する
//! （[`verify_dtvi_consistency`]）。`#27` の検証と論理的には重複するが、cut パイプライン
//! 全体を1箇所で検証する「最後の関門」として、表示順とデコード順の混同（CLAUDE.md の
//! 罠3、唯一の重大バグ源）を実行時にもう一度確認する意味がある。
//!
//! ## 設計判断2: `.dtvi` は既定で必須
//!
//! 検査4（`.dtvi` との突き合わせ）が、表示順とデコード順の混同という唯一の重大バグ源
//! （CLAUDE.md の罠3）に対する実効的な防御である。`.dtvi` が無いと検査4を一切実行でき
//! ず、混同があってもエラーを出さずに間違った位置で切られたファイルが出力されてしまう
//! （CLAUDE.md「静かに壊れる3つの罠」の通り、混同は例外を飛ばさない）。そのため
//! [`cut_and_verify`] は `dtvi: None` を早期エラーにする。エラーメッセージには
//! `tachikaze analyze` を先に実行するよう促す趣旨の文言を含める。`.dtvi` 無しでの実行を
//! 許容するオプション（例: `--skip-dtvi-check`）は本 issue のスコープ外とし、将来
//! 必要になった時点で明示的なフラグとして追加する。

use std::path::{Path, PathBuf};

use anyhow::Context;
use mp4_atom::Moov;

use crate::audio::{self, AudioSegment, AvSyncReport};
use crate::dtvi::Dtvi;
use crate::mp4io::order_map::{verify_against_dtvi, DisplayDecodeMap};
#[cfg(test)]
use crate::mp4io::read::find_audio_track;
use crate::mp4io::read::{find_video_track, read_moov, samples, SampleInfo};
use crate::mp4io::write::write_mp4;
use crate::order::{DecodeIdx, OrderMap};
use crate::plan::SnappedRange;

/// 検査5（音声ドリフトのログ出力）に必要な入力をまとめたもの。
///
/// 生のタプルのままだと `cut_and_verify` の引数が読みにくくなるため、意味のある名前を
/// 持つ小さな構造体として切り出す（issue が許容している調整）。
#[derive(Clone, Copy)]
pub struct AudioDiffInputs<'a> {
    /// 出力に並べる順の各映像区間の再生時間（映像トラックの timescale 単位）。
    pub video_segment_durations: &'a [u64],
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
/// （1〜4）と音声ドリフトのログ出力（5）を行う。1つでも検査に失敗すれば一時ファイルを
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

/// 書き出した一時ファイルを読み直し、検査1〜5を行う。
#[allow(clippy::too_many_arguments)]
fn verify_written_output(
    tmp_path: &Path,
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

    // 検査5: 音声の累積誤差の最大値をログ出力する（--video-only ならスキップ）。
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

/// 検査5（音声の累積誤差の最大値をログ出力する）。
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
            .context("検査5(音声ドリフトのログ)に失敗: av_sync_report の計算に失敗しました")?;
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

    const FIXTURE: &str = "tests/fixtures/sample.mp4";
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

        let (audio_segments, _drift) = crate::audio::select_audio_segments(
            &video_segment_durations,
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
                matches!(
                    t.mdia.minf.stbl.stsd.codecs.first(),
                    Some(mp4_atom::Codec::Opus(_))
                )
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
            video_timescale: 30000,
            audio_samples: &audio_samples,
            audio_timescale: 48000,
        };
        // 逆に audio_segments が None だが audio_samples_for_diff が Some。
        assert!(log_audio_drift(None, Some(inputs)).is_err());
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
}
