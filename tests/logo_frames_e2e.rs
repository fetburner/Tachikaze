//! [E14-5] `logo::frames::stream_luma_frames` の E2E。
//!
//! `tests/fixtures/sample.mp4`（640x360、599フレーム、30000/1001。
//! `tests/fixtures/gen.sh` 参照）から実際に ffmpeg を起動し、
//! ロゴ矩形を crop したフレーム数が `.dtvi` の `frame_count`（599）と一致することと、
//! 期待値を偽ると検査で弾かれることを確認する。

mod common;

use tachikaze::logo::frames::{stream_luma_frames, LogoRect, VideoSize};
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
