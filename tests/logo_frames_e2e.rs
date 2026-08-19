//! [E14-5] `logo::frames::stream_luma_frames` / [E18-1]
//! `logo::frames::stream_keyframe_luma_frames` の E2E。
//!
//! `tests/fixtures/sample.mp4`（640x360、599フレーム、30000/1001。
//! `tests/fixtures/gen.sh` 参照）から実際に ffmpeg を起動し、
//! ロゴ矩形を crop したフレーム数が `.dtvi` の `frame_count`（599）と一致することと、
//! 期待値を偽ると検査で弾かれることを確認する。

mod common;

use std::process::Command;

use tachikaze::logo::frames::{
    stream_keyframe_luma_frames, stream_luma_frames, LogoRect, VideoSize,
};
use tachikaze::tools;

/// フィクスチャの映像サイズ（`tests/fixtures/gen.sh` の `testsrc2=size=640x360`）。
const VIDEO_SIZE: VideoSize = VideoSize {
    width: 640,
    height: 360,
};
/// フィクスチャの実フレーム数（CLAUDE.md / 他の E2E テストと同じ値）。
const EXPECTED_FRAME_COUNT: u64 = 599;

/// 映像内に収まる適当なロゴ矩形（左上寄り、64x64）。
fn sample_rect() -> LogoRect {
    LogoRect {
        x: 0,
        y: 0,
        w: 64,
        h: 64,
    }
}

#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn crop_frame_count_matches_dtvi_frame_count() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }

    let ffmpeg = tools::resolve_tool(tools::FFMPEG).expect("ffmpeg を解決できること");
    let cwd = common::make_tmp_dir("logo-frames-happy");
    let rect = sample_rect();

    let mut frame_lengths = Vec::new();
    let n = stream_luma_frames(
        &ffmpeg,
        &common::fixture_path(),
        &cwd,
        rect,
        VIDEO_SIZE,
        EXPECTED_FRAME_COUNT,
        |frame| {
            frame_lengths.push(frame.len());
            Ok(())
        },
    )
    .expect("フレーム数が一致するので成功するはず");

    assert_eq!(n, EXPECTED_FRAME_COUNT);
    assert_eq!(frame_lengths.len(), EXPECTED_FRAME_COUNT as usize);
    assert!(
        frame_lengths
            .iter()
            .all(|&len| len == (rect.w * rect.h) as usize),
        "全フレームが w*h バイトであるはず"
    );
}

#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn wrong_expected_frame_count_is_an_error() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }

    let ffmpeg = tools::resolve_tool(tools::FFMPEG).expect("ffmpeg を解決できること");
    let cwd = common::make_tmp_dir("logo-frames-mismatch");
    let rect = sample_rect();

    // 実際は599フレームだが、598だと偽って渡す。
    let err = stream_luma_frames(
        &ffmpeg,
        &common::fixture_path(),
        &cwd,
        rect,
        VIDEO_SIZE,
        EXPECTED_FRAME_COUNT - 1,
        |_frame| Ok(()),
    )
    .expect_err("期待フレーム数を偽ったのでエラーになるはず");

    let message = err.to_string();
    assert!(message.contains("598"), "message={message}");
    assert!(message.contains("599"), "message={message}");
    assert!(message.contains("CM"), "message={message}");
}

/// ffmpeg 自体が異常終了した場合、`read_frames` のフレーム数不一致（`.dtvi`
/// との食い違い、CM がずれる旨の文言）に隠さず、ffmpeg の失敗（終了コード・
/// stderr）を報告することを確認する。壊れた入力を渡すとほぼ確実にフレーム数も
/// 0 になり不一致にもなるが、根本原因は「ffmpeg が起動できなかった」方なので
/// そちらを優先して報告する（レビューで見つかった、`wait()` に到達しないため
/// ffmpeg の失敗が握り潰されていた回帰の防止）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn ffmpeg_failure_is_reported_instead_of_frame_count_mismatch() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }
    let ffmpeg = tools::resolve_tool(tools::FFMPEG).expect("ffmpeg を解決できること");
    let cwd = common::make_tmp_dir("logo-frames-corrupted");

    let broken = cwd.join("broken.mp4");
    std::fs::write(&broken, b"not a video at all").unwrap();

    let rect = sample_rect();
    let err = stream_luma_frames(
        &ffmpeg,
        &broken,
        &cwd,
        rect,
        VIDEO_SIZE,
        EXPECTED_FRAME_COUNT,
        |_frame| Ok(()),
    )
    .expect_err("壊れた入力なのでエラーになるはず");

    let message = err.to_string();
    assert!(
        message.contains("外部プロセスが失敗しました"),
        "ffmpeg の異常終了そのものが報告されるはず: message={message}"
    );
    assert!(
        !message.contains("CM の位置が黙ってずれます"),
        "フレーム数不一致の文言に隠れてはいけない: message={message}"
    );
}

/// `on_frame` コールバックが1フレーム目でエラーを返して読み取りを中断した場合、
/// そのエラーメッセージがそのまま返ることを確認する（ffmpeg はまだ生きている
/// ので `kill()` されるが、`kill` によるシグナル終了エラーで `on_frame` 本来の
/// エラーが隠れてはいけない。レビューで見つかった回帰の防止）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn callback_error_is_not_masked_by_kill() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }
    let ffmpeg = tools::resolve_tool(tools::FFMPEG).expect("ffmpeg を解決できること");
    let cwd = common::make_tmp_dir("logo-frames-callback-error");
    let rect = sample_rect();

    let err = stream_luma_frames(
        &ffmpeg,
        &common::fixture_path(),
        &cwd,
        rect,
        VIDEO_SIZE,
        EXPECTED_FRAME_COUNT,
        |_frame| anyhow::bail!("on_frame からの特有のエラー文言"),
    )
    .expect_err("on_frame のエラーが伝播するはず");

    let message = err.to_string();
    assert!(
        message.contains("on_frame からの特有のエラー文言"),
        "on_frame 本来のエラーが出るはず（kill 由来のエラーに隠れていないはず）: message={message}"
    );
}

// ---------------------------------------------------------------
// [E18-1] stream_keyframe_luma_frames
// ---------------------------------------------------------------

/// `ffprobe -show_entries packet=flags` でフィクスチャの映像ストリームのキーフレーム
/// 数（フラグ `K` を含むパケットの数）を数える。
fn count_keyframes_via_ffprobe(input: &std::path::Path) -> usize {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "packet=flags",
            "-of",
            "csv=p=0",
        ])
        .arg(input)
        .output()
        .expect("ffprobe を起動できること");
    assert!(
        output.status.success(),
        "ffprobe が失敗しました: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains('K'))
        .count()
}

/// キーフレームだけを読む [`stream_keyframe_luma_frames`] が読めるフレーム数が、
/// `ffprobe` で数えたキーフレーム数と一致することを確認する（完了条件）。
/// あわせて、クロップしていない全画面（`640*360` バイト）が流れてくることも確認する。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg/ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn keyframe_only_frame_count_matches_ffprobe_keyframe_count() {
    // `count_keyframes_via_ffprobe` は ffprobe を `.expect(...)` で起動するため、
    // ffmpeg はあっても ffprobe が無い環境ではスキップではなく panic になってしまう。
    // 両方の有無を確認する `tools_available()` を使う（`tests/audio_e2e.rs` と同じ流儀）。
    if common::skip_if_fixture_missing() || !common::tools_available() {
        return;
    }

    let expected_keyframes = count_keyframes_via_ffprobe(&common::fixture_path());
    assert!(
        expected_keyframes > 0,
        "フィクスチャに1枚もキーフレームが無い（テスト前提が崩れている）"
    );

    let ffmpeg = tools::resolve_tool(tools::FFMPEG).expect("ffmpeg を解決できること");
    let cwd = common::make_tmp_dir("logo-frames-keyframe-happy");

    let mut frame_lengths = Vec::new();
    let n = stream_keyframe_luma_frames(
        &ffmpeg,
        &common::fixture_path(),
        &cwd,
        VIDEO_SIZE,
        |frame| {
            frame_lengths.push(frame.len());
            Ok(())
        },
    )
    .expect("キーフレームが読めるはず");

    assert_eq!(n as usize, expected_keyframes);
    assert_eq!(frame_lengths.len(), expected_keyframes);
    assert!(
        frame_lengths
            .iter()
            .all(|&len| len == (VIDEO_SIZE.width * VIDEO_SIZE.height) as usize),
        "クロップせず全画面（w*h バイト）が流れてくるはず"
    );
}
