//! [E14-6] `make-logo` サブコマンドの E2E。
//!
//! `tests/fixtures/sample.mp4`（640x360、testsrc2、`tests/fixtures/gen.sh` 参照）に対し
//! 実際に `tachikaze make-logo` を実行し、書き出した `.lgd` を E14-3 の読み込み
//! （`tachikaze::logo::lgd::read`）で読めることを確認する。
//!
//! 矩形 `620,4,8,8` は testsrc2 の右上付近で、外周1ピクセルが多くのフレームで
//! ほぼ単色になる（実測: 全599フレーム中582フレームが既定閾値12を通る）ことを
//! 事前に確認して選んだ座標。テスト用の合成映像であり実際のロゴではないため、
//! 係数の値そのものは検証しない（単体テスト`src/logo/scan.rs`が既知のa,bからの
//! 回帰精度を確認している）。

mod common;

use std::process::Command;

use tachikaze::logo::lgd;

#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn make_logo_output_round_trips_through_lgd_read() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }

    let cwd = common::make_tmp_dir("make-logo-e2e-round-trip");
    let output = cwd.join("out.lgd");

    let status = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("make-logo")
        .arg(common::fixture_path())
        .arg("--rect")
        .arg("620,4,8,8")
        .arg("-o")
        .arg(&output)
        .status()
        .expect("tachikaze make-logo を起動できるはず");
    assert!(status.success(), "make-logo が失敗しました: {status:?}");

    let logo = lgd::read(&output).expect(".lgd を読み込めるはず（E14-3 との往復）");

    assert_eq!(logo.w, 8);
    assert_eq!(logo.h, 8);
    assert_eq!(logo.imgw, 640);
    assert_eq!(logo.imgh, 360);
    assert_eq!(logo.imgx, 620);
    assert_eq!(logo.imgy, 4);
    assert_eq!(logo.a_y.len(), 8 * 8);
    assert_eq!(logo.b_y.len(), 8 * 8);
    // クロマ平面は学習しない(恒等変換で埋める、src/logo/scan.rs のモジュール doc
    // comment「クロマ平面は学習しない」参照)。
    assert!(logo.a_u.iter().all(|&a| a == 1.0));
    assert!(logo.b_u.iter().all(|&b| b == 0.0));
}

#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn make_logo_rounds_odd_rect_to_even_and_warns_on_stderr() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }

    let cwd = common::make_tmp_dir("make-logo-e2e-rounding");
    let output = cwd.join("out.lgd");

    // 620,4,8,8 に対して x を1増やして奇数にする(621) と、丸めで 620 に戻る
    // ことを期待する。
    let result = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("make-logo")
        .arg(common::fixture_path())
        .arg("--rect")
        .arg("621,4,9,9")
        .arg("-o")
        .arg(&output)
        .output()
        .expect("tachikaze make-logo を起動できるはず");
    assert!(
        result.status.success(),
        "make-logo が失敗しました: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );

    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("2の倍数に丸めました"),
        "丸めの通知が出るはず: stderr={stderr}"
    );

    let logo = lgd::read(&output).expect(".lgd を読み込めるはず");
    assert_eq!(logo.w, 8);
    assert_eq!(logo.h, 8);
    assert_eq!(logo.imgx, 620);
    assert_eq!(logo.imgy, 4);
}

#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn make_logo_fails_when_rect_has_no_usable_frames() {
    if common::skip_if_fixture_missing() || common::skip_if_missing("ffmpeg") {
        return;
    }

    let cwd = common::make_tmp_dir("make-logo-e2e-insufficient");
    let output = cwd.join("out.lgd");

    // 画面中央付近はテストパターンの模様が激しく動くため、外周1ピクセルが単色に
    // なるフレームがほぼ無い(閾値を極端に下げて確実に0件にする)。
    let result = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("make-logo")
        .arg(common::fixture_path())
        .arg("--rect")
        .arg("300,150,32,32")
        .arg("--threshold")
        .arg("0")
        .arg("-o")
        .arg(&output)
        .output()
        .expect("tachikaze make-logo を起動できるはず");

    assert!(
        !result.status.success(),
        "有効フレームが無いので失敗するはず"
    );
    assert!(
        !output.exists(),
        "失敗時に壊れた .lgd を書き出してはいけない"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(stderr.contains("有効フレーム"), "stderr={stderr}");
}
