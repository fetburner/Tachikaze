//! `tachikaze remap-subs`（issue #59）の E2E。
//!
//! `segmap_e2e.rs`（#57）は「区間マップが実出力の ffprobe 上の区間境界と一致する」
//! ことを既に確認済みなので、ここでは区間マップ自体の正しさは前提にする。この
//! テストが確認するのは:
//!
//! - 実際に `tachikaze cut --segment-map` が書いた本物の区間マップを使って
//!   `tachikaze remap-subs` を走らせたとき、シフト/破棄/クリップが仕様どおりの
//!   時刻になること（**末尾側の区間**（区間2、除去した CM ぶんの大きなシフトが
//!   乗る）でも正しいことを確認する。先頭区間だけだとシフト量 0 でも
//!   偶然正しく見えてしまう）
//! - ASS と SRT で同じ入力イベントが同じ分類・同じ出力時刻になること
//! - 区間マップ・字幕サイドカーのキャッシュからの自動解決と、明示指定の優先を確認する

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use tachikaze::segmap::SegmentMap;
use tachikaze::workdir;

/// フィクスチャ（GOP=120・599フレーム）に対する Trim リスト。`segmap_e2e.rs` と同じ値。
/// `Snap::Outward`（既定）で `[10,110)` は `[0,120)` へ、`[370,470)` は `[360,480)` へ広がる。
const TRIM_AVS_CONTENT: &str = "Trim(10,109) ++ Trim(370,469)";

/// `label` に `"remap-subs-e2e-"` を付けて [`common::make_tmp_dir`] を呼ぶ薄い
/// ラッパ（ディレクトリ名は元と同じ `tachikaze-remap-subs-e2e-<label>-<pid>`）。
fn make_tmp_dir(label: &str) -> PathBuf {
    common::make_tmp_dir(&format!("remap-subs-e2e-{label}"))
}

// ---------------------------------------------------------------------
// `src/subtitle.rs` の丸め方向（開始floor/終了ceil）と同じ変換を、独立した
// テスト用実装として持つ。テストが実装のprivate関数を直接呼べない
// （`tests/` は別クレート）ための重複だが、丸めの規則自体はこのモジュールの
// doc comment に明記された固定仕様であり、罠1/2のような「実装のバグを隠す」
// 類の式ではないので、期待値の独立計算として問題ないと判断した。
// ---------------------------------------------------------------------

/// `value` 以上で `unit` の倍数になる最小の値を返す。
///
/// テストが書く入力イベントの時刻は、`src/subtitle.rs` の丸め（floor/ceil）を
/// 経由せず**厳密に**テキストへ変換できる値（センチ秒/ミリ秒の単位=`unit`の
/// 倍数）に揃えるために使う。区間の `source_start_dts` はフレーム duration
/// （このフィクスチャでは1001）由来でセンチ秒に揃っていないことが多いため、
/// 「区間の開始からの相対オフセット」ではなく、まずこの関数で区間内の
/// アラインされた点を探してから使う。
fn align_up(value: u64, unit: u64) -> u64 {
    let rem = value % unit;
    if rem == 0 {
        value
    } else {
        value + (unit - rem)
    }
}

fn cs_floor(ticks: u64, timescale: u32) -> u64 {
    (u128::from(ticks) * 100 / u128::from(timescale)) as u64
}
fn cs_ceil(ticks: u64, timescale: u32) -> u64 {
    let n = u128::from(ticks) * 100;
    let d = u128::from(timescale);
    n.div_ceil(d) as u64
}
fn ms_floor(ticks: u64, timescale: u32) -> u64 {
    (u128::from(ticks) * 1000 / u128::from(timescale)) as u64
}
fn ms_ceil(ticks: u64, timescale: u32) -> u64 {
    let n = u128::from(ticks) * 1000;
    let d = u128::from(timescale);
    n.div_ceil(d) as u64
}
fn format_ass_time(cs: u64) -> String {
    let c = cs % 100;
    let total_s = cs / 100;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h}:{m:02}:{s:02}.{c:02}")
}
fn format_srt_time(ms: u64) -> String {
    let msec = ms % 1000;
    let total_s = ms / 1000;
    let s = total_s % 60;
    let total_m = total_s / 60;
    let m = total_m % 60;
    let h = total_m / 60;
    format!("{h:02}:{m:02}:{s:02},{msec:03}")
}

/// `tachikaze cut --segment-map` を実行し、`(区間マップのパス, out.mp4のパス)` を返す。
fn run_cut_with_segment_map(tmp_dir: &Path, cache_root: &Path) -> (PathBuf, PathBuf) {
    let fixture = common::fixture_path();
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    let segmap_path = tmp_dir.join("seg.json");
    std::fs::write(&trim_path, TRIM_AVS_CONTENT).expect("trim.avs を書けること");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(cache_root)
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--dtvi")
        .arg(common::dtvi_path())
        .arg("--segment-map")
        .arg(&segmap_path)
        .output()
        .expect("tachikaze cut の起動に失敗した");
    assert!(
        output.status.success(),
        "tachikaze cut --segment-map が失敗した: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    (segmap_path, out_path)
}

/// 完了条件:
/// - 手書きの区間マップ + ASS でシフト/破棄/クリップの3分類が単体テストで固定されて
///   いる（`src/subtitle.rs` の単体テスト）ことに加えて、こちらは**実際の `cut`
///   実行が書いた本物の区間マップ**でも同じ式が成立することを確認する
/// - 末尾の区間（区間2、CM除去ぶんの大きなシフトが乗る）でも正しく張り替わる
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn remap_subs_ass_matches_real_segment_map_including_tail_segment() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !common::tools_available() {
        return;
    }

    let tmp_dir = make_tmp_dir("ass-main");
    let cache_root = make_tmp_dir("ass-main-cache");
    let (segmap_path, _out_path) = run_cut_with_segment_map(&tmp_dir, &cache_root);

    let segmap_json = std::fs::read_to_string(&segmap_path).expect("区間マップを読めること");
    let segmap = SegmentMap::from_json(&segmap_json).expect("区間マップをパースできること");
    assert_eq!(
        segmap.video_timescale, 30000,
        "フィクスチャのtimescaleは30000のはず（変わっていたらこのテストの前提が崩れている）"
    );
    assert_eq!(segmap.segments.len(), 2, "保持区間は2個のはず");
    let seg0 = segmap.segments[0];
    let seg1 = segmap.segments[1];
    let timescale = segmap.video_timescale;
    // ticks からセンチ秒/ミリ秒への変換が割り切れる単位(300 ticks = 1cs = 10ms、
    // timescale=30000のとき)。イベントの時刻をこの倍数に揃えて選ぶことで、
    // 「入力テキストを書く」時点での丸め(floor/ceil)を経由せず、テストの期待値
    // 計算を単純にする（丸めそのものの検証は `src/subtitle.rs` の単体テスト
    // `ticks_to_units_floor_and_ceil_bracket_non_exact_division` 等が既にカバー
    // している）。
    let align_unit = u64::from(timescale) / 100;

    // イベント1: 区間1(先頭, output_start=0)に完全に含まれる → シフト。
    let shift1_src_start = align_up(seg0.source_start_dts, align_unit) + align_unit * 10;
    let shift1_src_end = shift1_src_start + align_unit * 10;
    assert!(shift1_src_end < seg0.source_start_dts + seg0.duration);

    // イベント2: 区間1と区間2の間(除去区間=CM)に完全に含まれる → 破棄。
    let gap_start = seg0.source_start_dts + seg0.duration;
    let gap_end = seg1.source_start_dts;
    assert!(
        gap_end > gap_start,
        "outward snapで2区間の間にCM区間があるはず"
    );
    let discard_src_start = align_up(gap_start, align_unit) + align_unit * 10;
    let discard_src_end = discard_src_start + align_unit * 10;
    assert!(
        discard_src_end < gap_end,
        "破棄用イベントが実際にCM区間に収まっていること"
    );

    // イベント3: 区間2(末尾, 大きなシフトが乗る)に完全に含まれる → シフト。
    // ここが「末尾でも合っているか」の本体: output_start[1]はCM除去ぶん
    // 大きくシフトしている(単体テストの手書きデータでは検証できない実測値)。
    let shift2_src_start = align_up(seg1.source_start_dts, align_unit) + align_unit * 10;
    let shift2_src_end = shift2_src_start + align_unit * 10;
    assert!(shift2_src_end < seg1.source_start_dts + seg1.duration);
    // output_start[1] は区間1の長さ(累積)そのもの。一方 source_start_dts[1] は
    // ソース上ではその手前に CM 区間(gap)を挟んだずっと先の時刻。つまり
    // 「シフト量 = source_start_dts[1] - output_start[1]」は CM の長さぶん
    // 大きい非ゼロ値になる。これが「末尾でも合っているか」の検証対象になる
    // 実際のシフト量。
    assert_eq!(
        seg1.output_start, seg0.duration,
        "output_start[1]は区間1の長さの累積のはず"
    );
    assert!(
        seg1.source_start_dts > seg1.output_start,
        "区間2はCM除去ぶんの非ゼロなシフトが乗っているはず（先頭区間のシフト量0とは違う形になっていること）"
    );

    // イベント4: 区間2の終端を跨いで伸びる(区間2が最後の保持区間なので後続区間は
    // 無い) → 終端でクリップ。開始は区間2の終端より手前のアラインされた点、
    // 終端は区間外まで(align_unit分)大きく伸ばす。
    let seg1_end = seg1.source_start_dts + seg1.duration;
    let clip_src_start = align_up(seg1_end, align_unit) - align_unit * 10;
    let clip_src_end = align_up(seg1_end, align_unit) + align_unit * 1000;
    assert!(
        clip_src_start < seg1_end,
        "クリップ用イベントの開始は区間2の内側のはず"
    );
    assert!(
        clip_src_end > seg1_end,
        "クリップ用イベントの終端は区間2の外側まで伸びているはず"
    );

    let ass_content = format!(
        "[Script Info]\r\n\
Title: e2e\r\n\
\r\n\
[Events]\r\n\
Format: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n\
Dialogue: 0,{},{},Default,,0,0,0,,shift1\r\n\
Dialogue: 0,{},{},Default,,0,0,0,,discard\r\n\
Dialogue: 0,{},{},Default,,0,0,0,,shift2 (tail)\r\n\
Dialogue: 0,{},{},Default,,0,0,0,,clip (tail boundary)\r\n",
        format_ass_time(cs_floor(shift1_src_start, timescale)),
        format_ass_time(cs_ceil(shift1_src_end, timescale)),
        format_ass_time(cs_floor(discard_src_start, timescale)),
        format_ass_time(cs_ceil(discard_src_end, timescale)),
        format_ass_time(cs_floor(shift2_src_start, timescale)),
        format_ass_time(cs_ceil(shift2_src_end, timescale)),
        format_ass_time(cs_floor(clip_src_start, timescale)),
        format_ass_time(cs_ceil(clip_src_end, timescale)),
    );

    let subs_path = tmp_dir.join("subs.ass");
    std::fs::write(&subs_path, &ass_content).expect("subs.ass を書けること");
    let out_ass_path = tmp_dir.join("result.ass");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("remap-subs")
        .arg(common::fixture_path())
        .arg("--segment-map")
        .arg(&segmap_path)
        .arg("--subs")
        .arg(&subs_path)
        .arg("-o")
        .arg(&out_ass_path)
        .output()
        .expect("tachikaze remap-subs の起動に失敗した");
    assert!(
        output.status.success(),
        "tachikaze remap-subs が失敗した: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("シフト 2 件")
            && stderr.contains("破棄 1 件")
            && stderr.contains("クリップ 1 件"),
        "件数のログが出ているはず: {stderr}"
    );

    let result = std::fs::read_to_string(&out_ass_path).expect("結果を読めること");

    // シフト1: output_start[0](=0) + (src - seg0.source_start_dts)。
    let expected_shift1_start = format_ass_time(cs_floor(
        seg0.output_start + (shift1_src_start - seg0.source_start_dts),
        timescale,
    ));
    let expected_shift1_end = format_ass_time(cs_ceil(
        seg0.output_start + (shift1_src_end - seg0.source_start_dts),
        timescale,
    ));
    assert!(
        result.contains(&format!(
            "Dialogue: 0,{expected_shift1_start},{expected_shift1_end},Default,,0,0,0,,shift1"
        )),
        "シフト1が期待どおりの時刻になっていない: {result}"
    );

    // 破棄されたイベントは残らない。
    assert!(!result.contains("discard"));

    // シフト2(末尾): output_start[1] + (src - seg1.source_start_dts)。区間1とは
    // 違う非ゼロの大きなシフト量が正しく効いていることを確認する。
    let expected_shift2_start = format_ass_time(cs_floor(
        seg1.output_start + (shift2_src_start - seg1.source_start_dts),
        timescale,
    ));
    let expected_shift2_end = format_ass_time(cs_ceil(
        seg1.output_start + (shift2_src_end - seg1.source_start_dts),
        timescale,
    ));
    assert!(
        result.contains(&format!(
            "Dialogue: 0,{expected_shift2_start},{expected_shift2_end},Default,,0,0,0,,shift2 (tail)"
        )),
        "末尾区間のシフトが期待どおりの時刻になっていない: {result}"
    );

    // クリップ(末尾境界): 終端は区間2の終端(source_start_dts+duration)にクランプされる。
    let expected_clip_start = format_ass_time(cs_floor(
        seg1.output_start + (clip_src_start - seg1.source_start_dts),
        timescale,
    ));
    let expected_clip_end_ticks = seg1.output_start + seg1.duration;
    let expected_clip_end = format_ass_time(cs_ceil(expected_clip_end_ticks, timescale));
    assert!(
        result.contains(&format!(
            "Dialogue: 0,{expected_clip_start},{expected_clip_end},Default,,0,0,0,,clip (tail boundary)"
        )),
        "末尾のクリップが期待どおりの時刻になっていない: {result}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// 完了条件: SRTでも同じ結果になる。上のASSテストと同じ4イベントをSRTで表現し、
/// 同じ区間マップに対して同じシフト/破棄/クリップ件数・同じ出力時刻になることを
/// 確認する。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn remap_subs_srt_matches_ass_result_for_same_events() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !common::tools_available() {
        return;
    }

    let tmp_dir = make_tmp_dir("srt-main");
    let cache_root = make_tmp_dir("srt-main-cache");
    let (segmap_path, _out_path) = run_cut_with_segment_map(&tmp_dir, &cache_root);

    let segmap_json = std::fs::read_to_string(&segmap_path).expect("区間マップを読めること");
    let segmap = SegmentMap::from_json(&segmap_json).expect("区間マップをパースできること");
    let seg0 = segmap.segments[0];
    let seg1 = segmap.segments[1];
    let timescale = segmap.video_timescale;

    // アラインの理由は `remap_subs_ass_matches_real_segment_map_including_tail_segment`
    // 参照（300ticks=1cs=10msの倍数に揃えることで、入力テキスト構築時に丸めを
    // 経由しない）。
    let align_unit = u64::from(timescale) / 100;
    let shift1_src_start = align_up(seg0.source_start_dts, align_unit) + align_unit * 10;
    let shift1_src_end = shift1_src_start + align_unit * 10;
    let gap_start = seg0.source_start_dts + seg0.duration;
    let discard_src_start = align_up(gap_start, align_unit) + align_unit * 10;
    let discard_src_end = discard_src_start + align_unit * 10;
    let shift2_src_start = align_up(seg1.source_start_dts, align_unit) + align_unit * 10;
    let shift2_src_end = shift2_src_start + align_unit * 10;
    let seg1_end = seg1.source_start_dts + seg1.duration;
    let clip_src_start = align_up(seg1_end, align_unit) - align_unit * 10;
    let clip_src_end = align_up(seg1_end, align_unit) + align_unit * 1000;

    let srt_content = format!(
        "1\r\n{} --> {}\r\nshift1\r\n\r\n\
2\r\n{} --> {}\r\ndiscard\r\n\r\n\
3\r\n{} --> {}\r\nshift2 (tail)\r\n\r\n\
4\r\n{} --> {}\r\nclip (tail boundary)\r\n\r\n",
        format_srt_time(ms_floor(shift1_src_start, timescale)),
        format_srt_time(ms_ceil(shift1_src_end, timescale)),
        format_srt_time(ms_floor(discard_src_start, timescale)),
        format_srt_time(ms_ceil(discard_src_end, timescale)),
        format_srt_time(ms_floor(shift2_src_start, timescale)),
        format_srt_time(ms_ceil(shift2_src_end, timescale)),
        format_srt_time(ms_floor(clip_src_start, timescale)),
        format_srt_time(ms_ceil(clip_src_end, timescale)),
    );

    let subs_path = tmp_dir.join("subs.srt");
    std::fs::write(&subs_path, &srt_content).expect("subs.srt を書けること");
    let out_srt_path = tmp_dir.join("result.srt");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("remap-subs")
        .arg(common::fixture_path())
        .arg("--segment-map")
        .arg(&segmap_path)
        .arg("--subs")
        .arg(&subs_path)
        .arg("-o")
        .arg(&out_srt_path)
        .output()
        .expect("tachikaze remap-subs の起動に失敗した");
    assert!(
        output.status.success(),
        "tachikaze remap-subs (SRT) が失敗した: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("シフト 2 件")
            && stderr.contains("破棄 1 件")
            && stderr.contains("クリップ 1 件"),
        "件数のログが出ているはず: {stderr}"
    );

    let result = std::fs::read_to_string(&out_srt_path).expect("結果を読めること");
    assert!(!result.contains("discard"));

    let expected_shift2_start = format_srt_time(ms_floor(
        seg1.output_start + (shift2_src_start - seg1.source_start_dts),
        timescale,
    ));
    let expected_shift2_end = format_srt_time(ms_ceil(
        seg1.output_start + (shift2_src_end - seg1.source_start_dts),
        timescale,
    ));
    assert!(
        result.contains(&format!(
            "{expected_shift2_start} --> {expected_shift2_end}"
        )),
        "末尾区間のシフト(SRT)が期待どおりの時刻になっていない: {result}"
    );
    assert!(result.contains("shift2 (tail)"));

    let expected_clip_end_ticks = seg1.output_start + seg1.duration;
    let expected_clip_end = format_srt_time(ms_ceil(expected_clip_end_ticks, timescale));
    assert!(
        result.contains(&expected_clip_end),
        "末尾のクリップ(SRT)の終端が期待どおりの時刻になっていない: {result}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// 完了条件:
/// - キャッシュからの自動解決が動く（`--segment-map`/`--subs` 省略時、`cut`/`prepare`
///   と同じキャッシュディレクトリから見つかる）
/// - 明示指定が最優先になる（キャッシュに別内容があっても `--segment-map`/`--subs`
///   を渡せばそちらが使われる）
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn remap_subs_resolves_from_cache_and_explicit_args_take_priority() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !common::tools_available() {
        return;
    }

    let tmp_dir = make_tmp_dir("cache-resolve");
    let cache_root = make_tmp_dir("cache-resolve-cache");
    // 既定の出力先（`-o` 省略時）は入力の隣に書かれる。リポジトリの
    // `tests/fixtures/` を汚さないよう、フィクスチャを一時ディレクトリへコピーし、
    // そちらを入力として使う。
    let fixture = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &fixture).expect("フィクスチャをコピーできること");

    // `cut --segment-map` を省略して実行し、既定のキャッシュ(work.mp4.segmap.json)に
    // 区間マップが残ることを利用する（segmap_e2e.rs の
    // `segment_map_is_written_to_default_cache_without_explicit_flag` と同じ前提）。
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    std::fs::write(&trim_path, TRIM_AVS_CONTENT).expect("trim.avs を書けること");
    let cut_output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache_root)
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--dtvi")
        .arg(common::dtvi_path())
        .output()
        .expect("tachikaze cut の起動に失敗した");
    assert!(cut_output.status.success());

    // `prepare` を経由せず、`workdir::subs_path` が指すキャッシュの場所へ直接
    // 字幕を置く(このテストの関心は remap-subs のパス解決であって prepare の
    // 抽出処理ではないため)。キャッシュの根を引数で直接渡せる(E12-2)ため、
    // 環境変数をプロセス全体で差し替える必要はない。
    let cache_subs_path = workdir::subs_path(Some(&cache_root), &fixture, "ass")
        .expect("cache subs path を計算できること");
    std::fs::create_dir_all(cache_subs_path.parent().unwrap()).expect("親ディレクトリを作れること");
    let cache_ass_content = "[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\nDialogue: 0,0:00:00.00,0:00:00.50,Default,,0,0,0,,from cache\r\n";
    std::fs::write(&cache_subs_path, cache_ass_content).expect("キャッシュへ字幕を書けること");

    // --segment-map/--subs/-o を省略 → どちらもキャッシュから自動解決されるはず。
    let default_out = fixture.parent().unwrap().join(format!(
        "{}_CMcut.ass",
        fixture.file_stem().unwrap().to_str().unwrap()
    ));
    let _ = std::fs::remove_file(&default_out); // 前回実行の残骸を消しておく
    let auto_output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache_root)
        .arg("remap-subs")
        .arg(&fixture)
        .output()
        .expect("tachikaze remap-subs(自動解決) の起動に失敗した");
    assert!(
        auto_output.status.success(),
        "キャッシュからの自動解決に失敗した: stderr={}",
        String::from_utf8_lossy(&auto_output.stderr)
    );
    assert!(
        default_out.is_file(),
        "既定の出力先(入力の隣, *_CMcut.ass)に書かれているはず: {}",
        default_out.display()
    );
    let auto_result = std::fs::read_to_string(&default_out).expect("既定出力を読めること");
    assert!(
        auto_result.contains("from cache"),
        "キャッシュの字幕が使われているはず: {auto_result}"
    );
    let _ = std::fs::remove_file(&default_out);

    // 明示指定(--segment-map/--subs/-o)をすべて渡すと、キャッシュではなくそちらが
    // 使われる。
    let explicit_subs = tmp_dir.join("explicit.ass");
    std::fs::write(
        &explicit_subs,
        "[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\nDialogue: 0,0:00:00.00,0:00:00.50,Default,,0,0,0,,from explicit\r\n",
    )
    .expect("明示指定の字幕を書けること");
    let explicit_segmap = tmp_dir.join("explicit_seg.json");
    // 明示の区間マップとして、`cut --segment-map` で別途書いたものを使う
    // (中身がキャッシュのものと同一でも、経路が明示指定であることを確認できれば
    // 十分)。
    let cut_output2 = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache_root)
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(tmp_dir.join("out2.mp4"))
        .arg("--dtvi")
        .arg(common::dtvi_path())
        .arg("--segment-map")
        .arg(&explicit_segmap)
        .output()
        .expect("tachikaze cut(2回目) の起動に失敗した");
    assert!(cut_output2.status.success());

    let explicit_out = tmp_dir.join("explicit_result.ass");
    let explicit_run = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache_root)
        .arg("remap-subs")
        .arg(&fixture)
        .arg("--segment-map")
        .arg(&explicit_segmap)
        .arg("--subs")
        .arg(&explicit_subs)
        .arg("-o")
        .arg(&explicit_out)
        .output()
        .expect("tachikaze remap-subs(明示指定) の起動に失敗した");
    assert!(
        explicit_run.status.success(),
        "明示指定でのremap-subsに失敗した: stderr={}",
        String::from_utf8_lossy(&explicit_run.stderr)
    );
    let explicit_result = std::fs::read_to_string(&explicit_out).expect("明示出力を読めること");
    assert!(
        explicit_result.contains("from explicit"),
        "明示指定の字幕が使われているはず: {explicit_result}"
    );
    assert!(!explicit_result.contains("from cache"));

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&cache_root);
}

/// フィクスチャ/ツールが無い環境でもテストの意図が読めるようにするプレースホルダ。
#[test]
fn remap_subs_e2e_module_compiles_and_helpers_are_reachable() {
    let _ = common::tools_available;
    let _ = cs_floor;
    let _ = cs_ceil;
    let _ = ms_floor;
    let _ = ms_ceil;
    let _ = format_ass_time;
    let _ = format_srt_time;
    let _ = run_cut_with_segment_map;
}
