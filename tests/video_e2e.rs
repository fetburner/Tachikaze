//! [E5-4] 映像パスの E2E: `cut --video-only` 相当の処理を実行し、出力の映像パケットが
//! 元ファイルとビット一致することを CRC32 で確認する。
//!
//! ## この統合テストが `src` の関数を直接呼んでいる理由・呼び方
//!
//! `video_only_cut_matches_source_packets_by_crc32` は `tachikaze cut` プロセスを
//! 起動するのではなく、cut パイプラインを組み立てる関数（`mp4io::read`,
//! `mp4io::order_map`, `plan`, `mp4io::write`）を直接呼び出して同じ処理を再現する。
//! スナップ後の区間やキーフレームに丸めた保持パケット数など、CLI の出力だけでは
//! 見えない中間状態も併せて検証したいため。
//!
//! これらは `tachikaze::`（`src/lib.rs` のライブラリクレート）経由で `pub` な項目
//! として参照する。`ffprobe::csv_rows` も同じ経由で参照する（`src/ffprobe.rs` の
//! doc comment「1か所に集約」参照）。
//!
//! `--cm-output` を検証する後半のテスト群は事情が異なる。CLI オプションそのものの
//! 検証（未指定時の挙動・`--snap inward` との併用エラー）も必要なため、
//! `tests/audio_e2e.rs` に倣い実際の `tachikaze` バイナリを起動する（下の
//! 「`--cm-output`」節を参照）。

mod common;

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use tachikaze::cli::Snap;
use tachikaze::mp4io::order_map::DisplayDecodeMap;
use tachikaze::mp4io::read::{find_video_track, read_moov, samples};
use tachikaze::mp4io::write::write_mp4;
use tachikaze::order::DecodeIdx;
use tachikaze::plan;
use tachikaze::trim::TrimList;

/// `-ss` によるキーフレーム seek が浮動小数点誤差で1フレーム手前に落ちるのを防ぐための
/// 補正値。docs/lossless-cut.md「参考: 検証で通した手順」と同じ値（1フレーム=33.4msに
/// 対して十分小さい）。カットの実装（パケット数ベース）とは無関係で、あくまで
/// 「比較対象を作るための ffmpeg -ss」の精度を上げるためだけに使う。
const SEEK_EPSILON_SECS: f64 = 0.005;

/// `path` の映像ストリームのパケット CRC32 一覧をファイル(=デコード)順に取得する。
///
/// CLAUDE.md の罠2 / docs/lossless-cut.md「無劣化の検証に md5 を使ってはいけない」節:
/// Annex B 変換した ES の md5 比較は誤り（`h264_mp4toannexb` が IDR ごとに SPS/PPS を
/// 再挿入するため）。ここでは mp4 コンテナのパケットをそのまま比較するので該当しないが、
/// 念のため同じ `-show_data_hash CRC32` の手法を使う。
///
/// 引数列の組み立ては `tachikaze::ffprobe::csv_rows` に委譲する(`src/ffprobe.rs`
/// のdoc comment「1か所に集約」を参照。以前はここに同じ引数列がベタ書きされていた)。
fn video_packet_crc32(path: &Path) -> Vec<String> {
    tachikaze::ffprobe::csv_rows(Path::new("ffprobe"), path, "v:0", "packet=size,data_hash")
        .expect("ffprobe を起動できること")
}

/// `path` の映像ストリームの `(pts_time, is_sync)` をファイル(=デコード)順に取得する。
///
/// デコード順での並びなので、`mp4io::read::samples` が返す `SampleInfo` のインデックス
/// (=`DecodeIdx`) とそのまま対応する。
fn video_packet_pts_time_and_flags(path: &Path) -> Vec<(f64, bool)> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=pts_time,flags",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe を起動できること");
    assert!(
        output.status.success(),
        "ffprobe (pts_time,flags) が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("ffprobe の出力が utf-8 であること")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            let mut parts = line.splitn(2, ',');
            let pts_time: f64 = parts
                .next()
                .expect("pts_time フィールドがあること")
                .parse()
                .expect("pts_time が数値であること");
            let flags = parts.next().unwrap_or("");
            (pts_time, flags.starts_with('K'))
        })
        .collect()
}

/// `path` をデコードし、表示順（pts 昇順）のフレームの `pts`（整数、timebase 単位）を
/// 取得する。浮動小数点誤差を避けるため `pts_time` ではなく整数の `pts` を使う。
///
/// `ffprobe -show_frames` はデコーダが吐き出す順、すなわち表示順で結果を返す
/// （B フレームの並べ替え後）。
fn frame_pts_in_display_order(path: &Path) -> Vec<i64> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "frame=pts",
            "-of",
            "csv=p=0",
        ])
        .arg(path)
        .output()
        .expect("ffprobe を起動できること");
    assert!(
        output.status.success(),
        "ffprobe (frame pts) が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout)
        .expect("ffprobe の出力が utf-8 であること")
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| {
            // 一部の行(観測上は先頭フレームのみ)に末尾の空フィールドが付くことが
            // あるため、カンマ区切りの先頭フィールドだけを使う。
            let first_field = line.split(',').next().unwrap_or(line);
            first_field
                .parse::<i64>()
                .unwrap_or_else(|e| panic!("pts が整数であること: {first_field:?} ({e})"))
        })
        .collect()
}

/// 元ファイルから、同期サンプル `seek_pts_time` を起点にデコード順で `frame_count`
/// パケット分を `-c copy` で抜き出し、その映像パケット CRC32 一覧を返す。
///
/// **`-frames:v` を使う（`-t` は使わない）。** `-frames:v` は厳密にパケット数を数えるので
/// 決定的（CLAUDE.md の罠1 / docs/lossless-cut.md「切り出しはパケット数で行う」節）。
fn extract_reference_crc32(
    fixture: &Path,
    tmp_dir: &Path,
    seek_pts_time: f64,
    frame_count: u32,
    tag: &str,
) -> Vec<String> {
    let seg_path = tmp_dir.join(format!("ref_{tag}.mp4"));
    let seek_arg = format!("{:.6}", seek_pts_time + SEEK_EPSILON_SECS);

    let output = Command::new("ffmpeg")
        .args(["-y", "-ss", &seek_arg, "-i"])
        .arg(fixture)
        .args([
            "-frames:v",
            &frame_count.to_string(),
            "-c",
            "copy",
            "-map",
            "0:v:0",
        ])
        .arg(&seg_path)
        .output()
        .expect("ffmpeg (reference extraction) を起動できること");
    assert!(
        output.status.success(),
        "ffmpeg (reference extraction, tag={tag}) が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    video_packet_crc32(&seg_path)
}

/// 1つの保持区間内で、表示順のフレーム間隔が一定であることを確認する
/// （欠落があると間隔が飛び飛びになる。docs/lossless-cut.md の穴の実例と同種の検査）。
fn assert_no_gaps_within_range(pts_in_range: &[i64], range_index: usize) {
    if pts_in_range.len() < 2 {
        return;
    }
    let step = pts_in_range[1] - pts_in_range[0];
    assert!(
        step > 0,
        "range {range_index}: 表示順フレーム間隔が0以下です（重複または逆順の疑い）"
    );
    for (i, w) in pts_in_range.windows(2).enumerate() {
        let delta = w[1] - w[0];
        assert_eq!(
            delta,
            step,
            "range {range_index} 内、区間先頭から{i}枚目→{}枚目の表示順フレーム間隔が\
             一定ではありません（欠落の疑い）: 通常間隔={step}, 実際={delta}",
            i + 1
        );
    }
}

/// フィクスチャに対して `cut --video-only` 相当の処理を組み立て、出力の全映像パケットが
/// 元ファイルの該当区間とCRC32単位でビット一致すること、および表示順に欠落がないことを
/// 確認する。
///
/// Trim はキーフレーム境界からわざとずらした値を使う（GOP=120・599フレームのフィクスチャ
/// のキーフレームは表示順 0, 120, 240, 360, 480）。`Snap::Outward` でスナップすると
/// 表示順 [10,110) は [0,120) へ、[370,470) は [360,480) へ広がり、中間の [120,360)
/// （CM相当）が捨てられる。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn video_only_cut_matches_source_packets_by_crc32() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !common::tools_available() {
        return;
    }

    let fixture = common::fixture_path();

    let moov = read_moov(&fixture).expect("moov を読めること");
    let (video_trak, _) = find_video_track(&moov).expect("映像トラックが見つかること");
    let file_len = std::fs::metadata(&fixture).expect("fixture metadata").len();
    let video_samples = samples(&video_trak.mdia.minf.stbl, file_len).expect("samples");
    let total_frames = video_samples.len() as u32;
    assert_eq!(
        total_frames, 599,
        "フィクスチャの前提(599フレーム)が変わっている場合はこのテストの手書きTrimも要調整"
    );

    let map = DisplayDecodeMap::build(&video_samples).expect("同値の合成時刻は無いはず");
    let sync_display = map.sync_display_indices();
    assert_eq!(
        sync_display.len(),
        5,
        "GOP120・599フレームのフィクスチャは5個のキーフレーム(0,120,240,360,480)を持つはず"
    );

    // わざとキーフレーム境界からずれた Trim（表示順、両端を含む書式）。
    let trim = TrimList::parse("Trim(10,109) ++ Trim(370,469)").expect("Trim をパースできること");

    let snapped = plan::snap(&trim, &sync_display, total_frames, Snap::Outward)
        .expect("スナップ後の区間が重ならないこと");

    // テストの前提が壊れていないことのセルフチェック（outward で本編側に伸びること）。
    assert_eq!(snapped.len(), 2, "保持区間は2つのはず");
    assert_eq!(
        (snapped[0].start.snapped.0, snapped[0].end.snapped.0),
        (0, 120)
    );
    assert_eq!(
        (snapped[1].start.snapped.0, snapped[1].end.snapped.0),
        (360, 480)
    );

    let keep = plan::keep_list(&snapped, &map.order).expect("keep_list が成功すること");
    assert_eq!(keep.len(), 240, "保持パケット数は 120 + 120 のはず");

    // moov.trak の各トラックに対応する keep リストを組み立てる。
    // 映像トラック以外（音声など）は空リストにして --video-only を再現する。
    let keep_per_track: Vec<Vec<DecodeIdx>> = moov
        .trak
        .iter()
        .map(|trak| {
            let is_video = matches!(
                trak.mdia.minf.stbl.stsd.codecs.first(),
                Some(mp4_atom::Codec::Avc1(_))
            );
            if is_video {
                keep.clone()
            } else {
                Vec::new()
            }
        })
        .collect();

    let tmp_dir = std::env::temp_dir().join(format!("tachikaze-video-e2e-{}", std::process::id()));
    std::fs::create_dir_all(&tmp_dir).expect("一時ディレクトリを作れること");
    let out_path = tmp_dir.join("cut_video_only.mp4");

    write_mp4(
        fixture.as_path(),
        out_path.as_path(),
        &moov,
        &keep_per_track,
    )
    .expect("write_mp4 が成功すること");

    // --- 完了条件1: 全映像パケットの CRC32 が一致する ---
    // --- 完了条件3: 不一致時は最初に食い違ったパケット番号を表示する ---

    let got_crc32 = video_packet_crc32(&out_path);
    assert_eq!(
        got_crc32.len(),
        keep.len(),
        "出力パケット数が keep リストの長さと一致すること"
    );

    let decode_order = video_packet_pts_time_and_flags(&fixture);

    let mut want_crc32: Vec<String> = Vec::new();
    let mut range_lengths: Vec<usize> = Vec::new();
    for (i, range) in snapped.iter().enumerate() {
        let start_decode = map
            .order
            .to_decode(range.start.snapped)
            .expect("開始位置に対応するデコード順インデックスがあること");
        let count = range.end.snapped - range.start.snapped;

        let (seek_pts_time, is_sync) = decode_order[start_decode.0 as usize];
        assert!(
            is_sync,
            "range {i}: 開始パケット(decode={})は同期サンプルのはず",
            start_decode.0
        );

        let seg_crc32 =
            extract_reference_crc32(&fixture, &tmp_dir, seek_pts_time, count, &format!("r{i}"));
        assert_eq!(
            seg_crc32.len() as u32,
            count,
            "range {i}: 参照セグメントのパケット数が期待値と一致すること"
        );

        range_lengths.push(seg_crc32.len());
        want_crc32.extend(seg_crc32);
    }

    assert_eq!(
        got_crc32.len(),
        want_crc32.len(),
        "出力と参照(元ファイルから抜き出した区間の連結)の合計パケット数が一致すること"
    );

    // 先頭から順に比較する。assert_eq! はインデックス i で最初に失敗するので、
    // 「最初に食い違ったパケット番号」がそのままテスト失敗メッセージに出る。
    for (i, (got, want)) in got_crc32.iter().zip(want_crc32.iter()).enumerate() {
        assert_eq!(
            got,
            want,
            "最初に食い違ったパケット番号 = {i} (出力パケット数={}, 期待パケット数={})",
            got_crc32.len(),
            want_crc32.len()
        );
    }

    // --- 完了条件2: 表示順（pts昇順）に欠落がないことも同時に確認する ---

    let output_pts = frame_pts_in_display_order(&out_path);
    assert_eq!(
        output_pts.len(),
        keep.len(),
        "出力の表示フレーム数が keep リストの長さと一致すること"
    );
    for (i, w) in output_pts.windows(2).enumerate() {
        assert!(
            w[1] > w[0],
            "出力フレーム{i}→{}の表示順(pts)が昇順になっていません: {:?}",
            i + 1,
            w
        );
    }

    // 各保持区間ごとに、区間内では表示順フレーム間隔が一定であることを確認する
    // （区間の継ぎ目では CM を捨てた分だけ間隔が大きくなるのが正常なので、
    // 継ぎ目をまたいだ一定性は要求しない）。
    let mut offset = 0usize;
    for (i, &len) in range_lengths.iter().enumerate() {
        assert_no_gaps_within_range(&output_pts[offset..offset + len], i);
        offset += len;
    }

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// フィクスチャが無い環境でも「テストが存在し、意図が読める」ことを保証するための
/// プレースホルダ。フィクスチャ/ffmpeg/ffprobe が無い CI では上のテストは早期returnで
/// 成功するため実質不要だが、`--ignored` を付け忘れて実行した場合にも
/// 何が起きるかが分かるようにしておく。
#[test]
fn video_e2e_module_compiles_and_helpers_are_reachable() {
    // ヘルパ関数・型が到達可能であることのコンパイル時チェックを兼ねる。
    let _ = common::tools_available;
    let _ = video_packet_crc32;
    let _ = video_packet_pts_time_and_flags;
    let _ = frame_pts_in_display_order;
    let _ = extract_reference_crc32;
    let _ = assert_no_gaps_within_range;
    let _: fn() -> PathBuf = common::fixture_path;
}

// =====================================================================
// `--cm-output`（CM として除去した区間を別ファイルに出す）の E2E。
//
// 上の `video_only_cut_matches_source_packets_by_crc32` は cut パイプラインを
// 組み立てる `tachikaze::` の関数を直接呼び出すが、`--cm-output` は CLI オプション
// そのものの検証（未指定時の挙動・`--snap inward` との併用エラー）も必要なので、
// こちらは `tests/audio_e2e.rs` に倣い実際の `tachikaze` バイナリを起動する。
// =====================================================================

/// フィクスチャ（GOP=120・599フレーム）に対する Trim リスト。他のテスト
/// （`video_only_cut_matches_source_packets_by_crc32` / `tests/audio_e2e.rs`）と同じ値を
/// 使い、`Snap::Outward`（既定）で `[10,110)` は `[0,120)` へ、`[370,470)` は `[360,480)` へ
/// 広がる。捨てられる中間区間 `[120,360)` と末尾の `[480,599)` が CM 側の補集合になる。
const CM_OUTPUT_TRIM_AVS_CONTENT: &str = "Trim(10,109) ++ Trim(370,469)";

/// `label` に `"video-e2e-cm-"` を付けて [`common::make_tmp_dir`] を呼ぶ薄いラッパ
/// （ディレクトリ名は元と同じ `tachikaze-video-e2e-cm-<label>-<pid>`）。
fn make_cm_output_tmp_dir(label: &str) -> PathBuf {
    common::make_tmp_dir(&format!("video-e2e-cm-{label}"))
}

/// フィクスチャに対して `tachikaze cut --cm-output` を実行する。
///
/// `snap` は `cli::Snap` の `--snap` 引数の文字列表現（`"outward"` / `"inward"`）を渡す。
/// 戻り値はプロセスの `Output` そのもの（失敗ケースも呼び出し側で判定できるように
/// `assert` はしない）。
fn run_cut_with_cm_output(
    label: &str,
    snap: &str,
) -> (PathBuf, PathBuf, PathBuf, std::process::Output) {
    let fixture = common::fixture_path();
    let tmp_dir = make_cm_output_tmp_dir(label);
    let trim_path = tmp_dir.join("trim.avs");
    let out_path = tmp_dir.join("out.mp4");
    let cm_path = tmp_dir.join("cm.mp4");
    std::fs::write(&trim_path, CM_OUTPUT_TRIM_AVS_CONTENT).expect("trim.avs を書けること");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("cut")
        .arg(&fixture)
        .arg("--trim")
        .arg(&trim_path)
        .arg("-o")
        .arg(&out_path)
        .arg("--dtvi")
        .arg(common::dtvi_path())
        .arg("--snap")
        .arg(snap)
        .arg("--cm-output")
        .arg(&cm_path)
        .output()
        .expect("tachikaze cut の起動に失敗した");

    (tmp_dir, out_path, cm_path, output)
}

/// 完了条件:
/// - 保持側 + CM側の映像パケット数の合計が入力の総パケット数と一致する
/// - CM側の映像パケットの CRC32 がすべて入力に存在する（ビットコピー）
/// - CM側と保持側の映像パケット CRC32 集合が互いに素
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn cm_output_packet_counts_sum_to_input_total_and_sets_are_disjoint() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !common::tools_available() {
        return;
    }

    let fixture = common::fixture_path();
    let (tmp_dir, out_path, cm_path, output) = run_cut_with_cm_output("counts", "outward");
    assert!(
        output.status.success(),
        "tachikaze cut --cm-output が失敗した: status={:?}\nstdout={}\nstderr={}",
        output.status,
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );

    let input_crc32 = video_packet_crc32(&fixture);
    let kept_crc32 = video_packet_crc32(&out_path);
    let cm_crc32 = video_packet_crc32(&cm_path);

    // --- 完了条件1: 保持側 + CM側のパケット数の合計 == 入力の総パケット数 ---
    assert_eq!(
        kept_crc32.len() + cm_crc32.len(),
        input_crc32.len(),
        "保持側({})+CM側({}) の映像パケット数の合計が入力の総数({})と一致しない",
        kept_crc32.len(),
        cm_crc32.len(),
        input_crc32.len()
    );

    // フィクスチャの前提（599フレーム、保持側240パケット、CM側359パケット）が
    // 変わっていないことのセルフチェック。
    assert_eq!(input_crc32.len(), 599);
    assert_eq!(kept_crc32.len(), 240, "保持側は120+120=240パケットのはず");
    assert_eq!(cm_crc32.len(), 359, "CM側は240+119=359パケットのはず");

    // --- 完了条件2: CM側の映像パケットの CRC32 がすべて入力に存在する（ビットコピー） ---
    let input_set: HashSet<&str> = input_crc32.iter().map(String::as_str).collect();
    let cm_only: Vec<&str> = cm_crc32
        .iter()
        .map(String::as_str)
        .filter(|c| !input_set.contains(c))
        .collect();
    assert!(
        cm_only.is_empty(),
        "CM側にのみ存在する映像パケットが{}件ある(ビットコピーでない疑い): {:?}",
        cm_only.len(),
        cm_only
    );

    // --- 完了条件3: CM側と保持側の映像パケット CRC32 集合が互いに素 ---
    let kept_set: HashSet<&str> = kept_crc32.iter().map(String::as_str).collect();
    let cm_set: HashSet<&str> = cm_crc32.iter().map(String::as_str).collect();
    let overlap: Vec<&&str> = kept_set.intersection(&cm_set).collect();
    assert!(
        overlap.is_empty(),
        "保持側とCM側の映像パケット CRC32 集合が互いに素ではない(重複{}件): {:?}",
        overlap.len(),
        overlap
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--snap inward --cm-output` はエラーで落ちる
/// （docs/lossless-cut.md「CM 側（除去した区間）を別ファイルに出す」節: inward スナップ
/// では保持区間が退化しうり、補集合の順序も壊れるため併用を拒否する設計）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn snap_inward_with_cm_output_is_rejected() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !common::tools_available() {
        return;
    }

    let (tmp_dir, out_path, cm_path, output) = run_cut_with_cm_output("inward-rejected", "inward");

    assert!(
        !output.status.success(),
        "--snap inward --cm-output が成功してしまった（拒否されるべき）"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("inward") && stderr.contains("cm-output"),
        "エラーメッセージに inward / cm-output の併用を拒否する理由が含まれていること: \
         {stderr}"
    );
    assert!(!out_path.exists(), "エラー時は保持側の出力も作られないこと");
    assert!(!cm_path.exists(), "エラー時はCM側の出力も作られないこと");

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
