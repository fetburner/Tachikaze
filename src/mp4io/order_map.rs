//! 表示順（display order）とデコード順（decode order）の対応を実データから導出し、
//! `.dtvi` と突き合わせる。
//!
//! - デコード順サンプル `i` の合成時刻（表示順を決める値）は `dts(i) + cts_offset(i)`。
//!   `dts(i)` は [`SampleInfo::duration`] の累積（`dts(0) = 0`）。
//! - 合成時刻でソートした順序が表示順になる。同値が起きるのは想定外の入力なので
//!   エラーにする（[`DisplayDecodeMap::build`]）。
//! - 同期サンプル（[`SampleInfo::is_sync`]）の `DecodeIdx` を昇順に保持し、任意の
//!   `DecodeIdx` に対して「直前（以下）の同期サンプル」を引けるようにする
//!   （[`DisplayDecodeMap::nearest_preceding_sync`]）。
//! - `.dtvi` の全行がこの導出結果と一致するかを検証する（[`verify_against_dtvi`]）。
//!
//! カット処理に `.dtvi`（dtvindex の出力）自体は不要（`stss` と `ctts` から自力で
//! 導出できる）。しかしフレーム番号の解釈がずれると、エラーを出さずに間違った位置で
//! 切ってしまう。外部ツールの索引との突き合わせが唯一の実効的な防御になる
//! （docs/pipeline.md「dtvindex の位置づけ」参照）。

use anyhow::{bail, Context, Result};

use crate::dtvi::Dtvi;
use crate::mp4io::read::SampleInfo;
use crate::order::{DecodeIdx, DisplayIdx, OrderMap};

/// デコード順サンプル `decode` の DTS（`dts(decode)`、デコード順の duration 累積、
/// `dts(0) = 0`）を求める。
///
/// 定義は [`DisplayDecodeMap::build`] の doc comment の `dts(i)` と同じ
/// （`samples[0..i]` の `duration` 累積）。**合成時刻（`dts + cts_offset`、表示順を
/// 決める値）とは別物**であることに注意（`cts_offset` は加えない）。
///
/// 区間の「ソース上の絶対開始時刻」を求めるときはこちらの DTS を使う。合成時刻
/// （PTS 相当）を使うと、出力側で `ctts` を引き継ぐ都合上、音声が
/// `cts_offset`（B フレームの並べ替え深度ぶん）先行してしまう系統的なずれの原因になる
/// （なぜ合成時刻ではなく DTS が正しいのかの導出は
/// `src/commands.rs::segment_video_source_starts` の doc comment を参照）。
///
/// `decode` が `samples` の範囲外なら `None`。
///
/// 呼び出しごとに `samples[0..decode.0]` の duration を合計し直す（O(n)）。
/// カット処理では出力区間の数（せいぜい数十）だけ呼ばれるため、この程度の
/// 計算量で十分（`DisplayDecodeMap::build` のようにファイル全体を1回で処理する
/// 必要がある場面では、代わりに `build` 内の累積ループを使うこと）。
pub fn decode_timestamp(samples: &[SampleInfo], decode: DecodeIdx) -> Option<u64> {
    let idx = decode.0 as usize;
    if idx >= samples.len() {
        return None;
    }
    let dts: u64 = samples[..idx].iter().map(|s| s.duration as u64).sum();
    Some(dts)
}

/// 表示順 ⇔ デコード順の対応（[`OrderMap`]）と、同期サンプルの一覧を保持する。
pub struct DisplayDecodeMap {
    /// 表示順 ⇔ デコード順の対応。
    pub order: OrderMap,
    /// 同期サンプルの `DecodeIdx` を昇順に並べたもの。
    sync_decode_indices: Vec<DecodeIdx>,
}

impl DisplayDecodeMap {
    /// `samples`（デコード順の一覧）から表示順 ⇔ デコード順の対応を構築する。
    ///
    /// デコード順サンプル `i` の合成時刻は `dts(i) + cts_offset(i)`。この値でソートした
    /// 順序を表示順とする。合成時刻が同値のサンプルが存在する場合はエラーにする
    /// （起こらないはずの事態であり、起きたらどのデコード順インデックス同士が
    /// 同値だったかをエラーメッセージに含める）。
    pub fn build(samples: &[SampleInfo]) -> Result<Self> {
        // dts(i) = duration の累積。dts(0) = 0。
        let mut composition_times: Vec<(i64, DecodeIdx)> = Vec::with_capacity(samples.len());
        let mut dts: u64 = 0;
        for (i, sample) in samples.iter().enumerate() {
            let cts = dts as i64 + sample.cts_offset;
            composition_times.push((cts, DecodeIdx(i as u32)));
            dts += sample.duration as u64;
        }

        // 合成時刻の昇順にソートする。これが表示順になる。
        composition_times.sort_by_key(|&(cts, _)| cts);

        // 同値検出: ソート後に隣り合う合成時刻が等しければ、その一群のデコード順
        // インデックスを添えてエラーにする。
        let mut i = 0;
        while i < composition_times.len() {
            let mut j = i + 1;
            while j < composition_times.len() && composition_times[j].0 == composition_times[i].0 {
                j += 1;
            }
            if j - i > 1 {
                let tied: Vec<u32> = composition_times[i..j]
                    .iter()
                    .map(|&(_, decode)| decode.0)
                    .collect();
                bail!(
                    "合成時刻 {} が複数のデコード順サンプルで同値です（デコード順インデックス: {:?}）",
                    composition_times[i].0,
                    tied
                );
            }
            i = j;
        }

        let pairs: Vec<(DisplayIdx, DecodeIdx)> = composition_times
            .into_iter()
            .enumerate()
            .map(|(display, (_, decode))| (DisplayIdx(display as u32), decode))
            .collect();

        let sync_decode_indices: Vec<DecodeIdx> = samples
            .iter()
            .enumerate()
            .filter(|(_, sample)| sample.is_sync)
            .map(|(i, _)| DecodeIdx(i as u32))
            .collect();

        Ok(Self {
            order: OrderMap::new(pairs),
            sync_decode_indices,
        })
    }

    /// 指定したデコード順サンプル以下で直近の同期サンプルの `DecodeIdx`。
    ///
    /// `i` 自身が同期サンプルであれば `i` を返す。`i` より前（含む）に同期サンプルが
    /// 1つも無ければ `None`。
    pub fn nearest_preceding_sync(&self, i: DecodeIdx) -> Option<DecodeIdx> {
        // sync_decode_indices は昇順なので、"i 以下" が真である先頭区間の長さを
        // partition_point で求め、その最後の要素を返す。
        let count = self.sync_decode_indices.partition_point(|&sync| sync <= i);
        count
            .checked_sub(1)
            .map(|idx| self.sync_decode_indices[idx])
    }

    /// 同期サンプルの `DisplayIdx` を昇順に並べたもの。
    ///
    /// 閉じた GOP では GOP は決定順・表示順のどちらで見ても同じフレーム集合の
    /// 塊になる（GOP 内の並べ替えは GOP をまたがない）。そのため同期サンプル
    /// （＝各 GOP の先頭）どうしの相対順序はデコード順でも表示順でも一致し、
    /// `sync_decode_indices` を表示順に変換するだけで表示順のリストが得られる
    /// （念のため昇順ソートしてから返す）。
    ///
    /// `plan`（#29）がキーフレーム境界スナップを表示順で行うために使う。
    pub fn sync_display_indices(&self) -> Vec<DisplayIdx> {
        let mut indices: Vec<DisplayIdx> = self
            .sync_decode_indices
            .iter()
            .filter_map(|&decode| self.order.to_display(decode))
            .collect();
        indices.sort();
        indices
    }
}

/// `.dtvi`（`dtvi`）の全行が自前導出（`map`）と一致するか検証する。
///
/// 不一致行があれば、その行の `frame_number` と両方の値（`.dtvi` の値と自前導出の値）を
/// 含むエラーを返す。全行一致すれば `Ok(())`。
pub fn verify_against_dtvi(map: &DisplayDecodeMap, dtvi: &Dtvi) -> Result<()> {
    for frame in &dtvi.frames {
        let derived_decode = map.order.to_decode(frame.frame_number).with_context(|| {
            format!(
                "frame_number={} に対応するデコード順インデックスが自前導出に存在しません（.dtvi の sample_number={}）",
                frame.frame_number.0, frame.sample_number.0
            )
        })?;

        if derived_decode != frame.sample_number {
            bail!(
                "frame_number={} の sample_number が不一致です（.dtvi={}, 自前導出={}）",
                frame.frame_number.0,
                frame.sample_number.0,
                derived_decode.0
            );
        }

        let derived_sync = map
            .nearest_preceding_sync(derived_decode)
            .with_context(|| {
                format!(
                "frame_number={} (sample_number={}) の直前の同期サンプルが自前導出に存在しません",
                frame.frame_number.0, derived_decode.0
            )
            })?;

        if derived_sync != frame.random_access_sample {
            bail!(
                "frame_number={} の random_access_sample が不一致です（.dtvi={}, 自前導出={}）",
                frame.frame_number.0,
                frame.random_access_sample.0,
                derived_sync.0
            );
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtvi;
    use crate::mp4io::read::{find_video_track, read_moov, samples};

    /// フィクスチャ: H.264 (Avc1) + Opus, GOP 120, 30000/1001fps, クローズド GOP の mp4。
    /// `tests/fixtures/gen.sh`（issue #15）で生成する。無ければスキップする。
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

    /// 実際に `dtvindex build` で生成した `.dtvi` の抜粋（ヘッダ全体 + 先頭40フレーム）。
    /// `tests/fixtures/gen.sh` と全く同じ手順（同じ ffmpeg コマンド）で生成した
    /// `sample.mp4` に対して実際に `dtvindex` バイナリ（tobitti0/dtvindex を `make` した
    /// もの）を実行して得たものであり、手書きではない。
    /// (`dtvi.rs` の #23 実装時に確認済みの実データ。本 issue (#27) でも同じ mp4 フィクスチャに
    /// 対して再生成し、mtime 以外完全一致することを確認した。)
    const DTVI_SAMPLE: &str = include_str!("../../tests/data/sample.dtvi");

    /// フィクスチャ mp4 から映像トラックの `SampleInfo` 一覧を読み込む。
    fn video_samples() -> Vec<SampleInfo> {
        let moov = read_moov(FIXTURE).expect("moov を読めること");
        let (video_trak, _) = find_video_track(&moov).expect("映像トラックが見つかること");
        samples(&video_trak.mdia.minf.stbl)
    }

    // --- 完了条件1: 実フィクスチャで .dtvi と全行一致する ---

    #[test]
    fn verify_against_real_dtvi_succeeds() {
        if skip_if_fixture_missing() {
            return;
        }
        let samples = video_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値の合成時刻は無いはず");
        let dtvi = dtvi::parse(DTVI_SAMPLE).expect(".dtvi をパースできること");

        verify_against_dtvi(&map, &dtvi).expect(".dtvi の全行が自前導出と一致するはず");
    }

    // --- 完了条件2: 1行だけ故意に書き換えると、その行番号を示して停止する ---

    #[test]
    fn verify_against_dtvi_stops_on_corrupted_sample_number() {
        if skip_if_fixture_missing() {
            return;
        }
        let samples = video_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値の合成時刻は無いはず");
        let mut dtvi = dtvi::parse(DTVI_SAMPLE).expect(".dtvi をパースできること");

        // frame_number=5 の sample_number を意図的にずらす。
        let corrupted_frame_number = dtvi.frames[5].frame_number.0;
        let original_sample_number = dtvi.frames[5].sample_number.0;
        dtvi.frames[5].sample_number = DecodeIdx(9999);

        let err = verify_against_dtvi(&map, &dtvi)
            .expect_err("sample_number をずらした行があるので失敗するはず");
        let message = err.to_string();

        assert!(
            message.contains(&corrupted_frame_number.to_string()),
            "エラーメッセージに行番号 (frame_number={corrupted_frame_number}) が含まれること: {message}"
        );
        assert!(
            message.contains("9999"),
            ".dtvi 側の値 (9999) がエラーメッセージに含まれること: {message}"
        );
        assert_ne!(
            original_sample_number, 9999,
            "テストの前提: 元の値は 9999 ではないこと"
        );
    }

    #[test]
    fn verify_against_dtvi_stops_on_corrupted_random_access_sample() {
        if skip_if_fixture_missing() {
            return;
        }
        let samples = video_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値の合成時刻は無いはず");
        let mut dtvi = dtvi::parse(DTVI_SAMPLE).expect(".dtvi をパースできること");

        // frame_number=10 の random_access_sample を意図的にずらす。
        let corrupted_frame_number = dtvi.frames[10].frame_number.0;
        dtvi.frames[10].random_access_sample = DecodeIdx(9999);

        let err = verify_against_dtvi(&map, &dtvi)
            .expect_err("random_access_sample をずらした行があるので失敗するはず");
        let message = err.to_string();

        assert!(
            message.contains(&corrupted_frame_number.to_string()),
            "エラーメッセージに行番号 (frame_number={corrupted_frame_number}) が含まれること: {message}"
        );
        assert!(
            message.contains("9999"),
            ".dtvi 側の値 (9999) がエラーメッセージに含まれること: {message}"
        );
    }

    // --- 完了条件3: 閉じた GOP では同期サンプルの DecodeIdx == DisplayIdx ---

    #[test]
    fn closed_gop_sync_samples_have_matching_display_and_decode_index() {
        if skip_if_fixture_missing() {
            return;
        }
        let samples = video_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値の合成時刻は無いはず");

        let sync_decode_indices: Vec<DecodeIdx> = samples
            .iter()
            .enumerate()
            .filter(|(_, s)| s.is_sync)
            .map(|(i, _)| DecodeIdx(i as u32))
            .collect();
        assert!(
            !sync_decode_indices.is_empty(),
            "GOP 120 のフィクスチャなら同期サンプルが複数あるはず"
        );

        for decode in sync_decode_indices {
            let display = map
                .order
                .to_display(decode)
                .expect("同期サンプルの表示順が存在すること");
            assert_eq!(
                display,
                DisplayIdx(decode.0),
                "閉じた GOP では同期サンプルの DecodeIdx と DisplayIdx が一致するはず（decode={decode:?}）"
            );
        }
    }

    // --- build() / nearest_preceding_sync() の合成データによる単体テスト ---

    /// B フレームによる並べ替えを模した合成データで、合成時刻の昇順が表示順に
    /// 一致することを確認する。
    ///
    /// パターン: I(0) P(1) b(2) b(3) （デコード順）、表示順は I(0) b(2) b(3) P(1)。
    fn reordered_samples() -> Vec<SampleInfo> {
        // duration は全サンプル一律1000。cts_offset で表示順を入れ替える。
        // dts: 0, 1000, 2000, 3000
        // cts = dts + cts_offset:
        //   decode0: 0 + 0    = 0    -> display 0
        //   decode1: 1000+3000= 4000 -> display 3
        //   decode2: 2000+(-1000)=1000 -> display 1
        //   decode3: 3000+(-1000)=2000 -> display 2
        vec![
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
        ]
    }

    #[test]
    fn build_derives_expected_display_order_from_composition_time() {
        let samples = reordered_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値は無いはず");

        assert_eq!(map.order.to_display(DecodeIdx(0)), Some(DisplayIdx(0)));
        assert_eq!(map.order.to_display(DecodeIdx(1)), Some(DisplayIdx(3)));
        assert_eq!(map.order.to_display(DecodeIdx(2)), Some(DisplayIdx(1)));
        assert_eq!(map.order.to_display(DecodeIdx(3)), Some(DisplayIdx(2)));

        assert_eq!(map.order.to_decode(DisplayIdx(0)), Some(DecodeIdx(0)));
        assert_eq!(map.order.to_decode(DisplayIdx(1)), Some(DecodeIdx(2)));
        assert_eq!(map.order.to_decode(DisplayIdx(2)), Some(DecodeIdx(3)));
        assert_eq!(map.order.to_decode(DisplayIdx(3)), Some(DecodeIdx(1)));
    }

    /// [`decode_timestamp`] が `DisplayDecodeMap::build` と同じ `dts(i)` の定義
    /// （duration 累積、`cts_offset` は加えない）で DTS を求めることを、同じ合成データ
    /// （`reordered_samples`）で確認する。合成時刻（cts）とは異なる値になることも
    /// あわせて確認する（decode1 は cts=4000 だが dts=1000）。
    #[test]
    fn decode_timestamp_matches_build_derivation() {
        let samples = reordered_samples();

        // reordered_samples() のコメントに書かれている dts の累積そのもの:
        // decode0: dts=0
        // decode1: dts=1000
        // decode2: dts=2000
        // decode3: dts=3000
        assert_eq!(decode_timestamp(&samples, DecodeIdx(0)), Some(0));
        assert_eq!(decode_timestamp(&samples, DecodeIdx(1)), Some(1000));
        assert_eq!(decode_timestamp(&samples, DecodeIdx(2)), Some(2000));
        assert_eq!(decode_timestamp(&samples, DecodeIdx(3)), Some(3000));
    }

    #[test]
    fn decode_timestamp_out_of_range_returns_none() {
        let samples = reordered_samples();
        assert_eq!(decode_timestamp(&samples, DecodeIdx(99)), None);
    }

    #[test]
    fn build_fails_with_tied_composition_time() {
        let mut samples = reordered_samples();
        // decode3 の合成時刻を decode2 と同値にする(2000+(-1000)=1000 -> 2000+0=2000 ではなく
        // decode2 と同じ 1000 になるよう cts_offset を変更する)。
        samples[3].cts_offset = -2000; // dts(3)=3000, cts=1000 (decode2 と同値)

        // `DisplayDecodeMap` は `OrderMap`(触ってよいファイル外・Debug 未実装) を
        // 保持しているため `expect_err` は使えない。`match` で明示的に取り出す。
        let message = match DisplayDecodeMap::build(&samples) {
            Ok(_) => panic!("合成時刻の同値はエラーになるはず"),
            Err(err) => err.to_string(),
        };
        assert!(
            message.contains('2'),
            "同値だったデコード順インデックス2を含むこと: {message}"
        );
        assert!(
            message.contains('3'),
            "同値だったデコード順インデックス3を含むこと: {message}"
        );
    }

    #[test]
    fn sync_display_indices_are_sorted_ascending_across_gops() {
        // 2 GOP、それぞれ I P b b（デコード順）。GOP内の並べ替えは既存の
        // reordered_samples() と同じパターンだが、GOPをまたいだ相対順序は
        // デコード順・表示順のどちらで見ても一致するはず。
        fn gop(base_offset: u64) -> Vec<SampleInfo> {
            vec![
                SampleInfo {
                    file_offset: base_offset,
                    size: 10,
                    duration: 1000,
                    cts_offset: 0,
                    is_sync: true,
                },
                SampleInfo {
                    file_offset: base_offset + 10,
                    size: 10,
                    duration: 1000,
                    // 元の reordered_samples() では 3000 だが、それだと合成時刻が
                    // 次の GOP の先頭 (dts の切れ目) と衝突するため、GOP の
                    // duration 合計 (4000) を超えない値に下げる。
                    cts_offset: 2500,
                    is_sync: false,
                },
                SampleInfo {
                    file_offset: base_offset + 20,
                    size: 10,
                    duration: 1000,
                    cts_offset: -1000,
                    is_sync: false,
                },
                SampleInfo {
                    file_offset: base_offset + 30,
                    size: 10,
                    duration: 1000,
                    cts_offset: -1000,
                    is_sync: false,
                },
            ]
        }

        let mut samples = gop(0);
        samples.extend(gop(40));

        let map = DisplayDecodeMap::build(&samples).expect("同値は無いはず");
        assert_eq!(
            map.sync_display_indices(),
            vec![DisplayIdx(0), DisplayIdx(4)],
            "GOPをまたいでも同期サンプルの表示順は昇順のはず"
        );
    }

    #[test]
    fn nearest_preceding_sync_finds_last_sync_at_or_before() {
        let samples = vec![
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
                cts_offset: 0,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 20,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: true,
            },
            SampleInfo {
                file_offset: 30,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: false,
            },
        ];
        let map = DisplayDecodeMap::build(&samples).expect("同値は無いはず");

        assert_eq!(map.nearest_preceding_sync(DecodeIdx(0)), Some(DecodeIdx(0)));
        assert_eq!(map.nearest_preceding_sync(DecodeIdx(1)), Some(DecodeIdx(0)));
        assert_eq!(map.nearest_preceding_sync(DecodeIdx(2)), Some(DecodeIdx(2)));
        assert_eq!(map.nearest_preceding_sync(DecodeIdx(3)), Some(DecodeIdx(2)));
    }

    #[test]
    fn nearest_preceding_sync_returns_none_when_no_sync_before() {
        let samples = vec![
            SampleInfo {
                file_offset: 0,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 10,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: true,
            },
        ];
        let map = DisplayDecodeMap::build(&samples).expect("同値は無いはず");

        assert_eq!(map.nearest_preceding_sync(DecodeIdx(0)), None);
        assert_eq!(map.nearest_preceding_sync(DecodeIdx(1)), Some(DecodeIdx(1)));
    }

    // --- verify_against_dtvi() の合成データによる単体テスト ---

    fn frame(frame_number: u32, sample_number: u32, random_access_sample: u32) -> dtvi::DtviFrame {
        dtvi::DtviFrame {
            frame_number: DisplayIdx(frame_number),
            sample_number: DecodeIdx(sample_number),
            random_access_sample: DecodeIdx(random_access_sample),
            file_offset: 0,
            pts: 0,
            dts: 0,
            duration: 1001,
            flags: 0,
        }
    }

    #[test]
    fn verify_against_dtvi_succeeds_on_matching_synthetic_data() {
        let samples = reordered_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値は無いはず");

        // 自前導出: display 0->decode0, 1->decode2, 2->decode3, 3->decode1。
        // 同期サンプルは decode0 のみなので、random_access_sample は常に0。
        let dtvi = Dtvi {
            format_version: 1,
            header: Default::default(),
            frames: vec![
                frame(0, 0, 0),
                frame(1, 2, 0),
                frame(2, 3, 0),
                frame(3, 1, 0),
            ],
        };

        verify_against_dtvi(&map, &dtvi).expect("一致するはず");
    }

    #[test]
    fn verify_against_dtvi_reports_row_and_both_values_on_mismatch() {
        let samples = reordered_samples();
        let map = DisplayDecodeMap::build(&samples).expect("同値は無いはず");

        let dtvi = Dtvi {
            format_version: 1,
            header: Default::default(),
            frames: vec![
                frame(0, 0, 0),
                // 本来 sample_number=2 のはずが 42 に化けている。
                frame(1, 42, 0),
                frame(2, 3, 0),
                frame(3, 1, 0),
            ],
        };

        let err = verify_against_dtvi(&map, &dtvi).expect_err("不一致なので失敗するはず");
        let message = err.to_string();
        assert!(
            message.contains('1'),
            "行番号 (frame_number=1) を含むこと: {message}"
        );
        assert!(
            message.contains("42"),
            ".dtvi 側の値 (42) を含むこと: {message}"
        );
        assert!(
            message.contains('2'),
            "自前導出の値 (2) を含むこと: {message}"
        );
    }
}
