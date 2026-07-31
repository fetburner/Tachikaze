//! [E11-2] `prepare` サブコマンドの統合テスト。
//!
//! `tests/fixtures/gen.sh` は elst 付き・字幕トラック付きのフィクスチャを持たない
//! (フィクスチャの追加・変更は別issue #60 の担当範囲)。そのためこのファイルでは、
//! 既存の `tests/fixtures/sample.mp4` / `sample_aac.mp4` を元に、`ffmpeg` で
//! **テスト実行時にその場で** elst 付き・字幕付きの一時 mp4 を作ってから検証する
//! (`tests/fixtures/` には何もコミットしない)。
//!
//! `Cargo.toml` に `[lib]` ターゲットがある(#11)ため、`tests/` から
//! `tachikaze::prepare` / `tachikaze::mp4io` を直接呼べる(他の `*_e2e.rs` が使う
//! `#[path]` インクルードや `CARGO_BIN_EXE_tachikaze` 起動は、lib ターゲットが
//! 無かった頃の回避策)。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tachikaze::mp4io::read::read_moov;
use tachikaze::prepare;

fn skip_if_missing(bin: &str) -> bool {
    match Command::new(bin).arg("-version").output() {
        Ok(output) if output.status.success() => false,
        _ => {
            eprintln!("{bin} が無いためスキップします。");
            true
        }
    }
}

/// テストごとに一意な一時ディレクトリを作る(`tempfile` クレートに依存しない、
/// 他のテストファイルと同じ素朴な方式)。
fn make_scratch_dir(label: &str) -> PathBuf {
    let base = std::env::temp_dir();
    let pid = std::process::id();
    for attempt in 0..100 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!(
            "tachikaze-prepare-e2e-{label}-{pid}-{nanos}-{attempt}"
        ));
        if fs::create_dir_all(&candidate).is_ok() {
            return candidate;
        }
    }
    panic!("scratch dir の作成に失敗しました");
}

/// `prepare::run` が使ったキャッシュディレクトリを片付ける(テスト後の掃除。
/// 実運用では消さない方針だが、テストが `~/.cache/tachikaze` を汚し続けない
/// ようにする)。
fn cleanup_cache_for(input: &Path) {
    if let Ok(dir) = tachikaze::workdir::prepared_input_path(input) {
        if let Some(parent) = dir.parent() {
            let _ = fs::remove_dir_all(parent);
        }
    }
}

/// `sample_aac.mp4` を `-c copy` で remux すると、ffmpeg が AAC の priming
/// (エンコーダ遅延)を補正する `elst` を既定で付与することがある
/// (`tests/fixtures/gen.sh` の `-use_editlist 0` コメント参照。あちらは
/// フィクスチャ自身が拒否されないよう明示的に外している)。ここでは逆に、
/// `-use_editlist 0` を付けずに remux することで意図的に elst 付き mp4 を作る。
fn make_elst_fixture(dir: &Path) -> PathBuf {
    let src = common::aac_fixture_path();
    let out = dir.join("IN_elst.mp4");
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&src)
        .args(["-c", "copy"])
        .arg(&out)
        .status()
        .expect("ffmpeg の起動に失敗しました");
    assert!(
        status.success(),
        "elst 付きフィクスチャの作成に失敗しました"
    );
    out
}

/// `sample.mp4` に mov_text (`Tx3g`) 字幕トラックを1本追加した mp4 を作る。
///
/// `-use_editlist 0` を付けて、ffmpeg が Opus の priming 補正で elst を
/// 自動付与しないようにする(`tests/fixtures/gen.sh` と同じ理由)。これにより
/// このフィクスチャは「字幕のみ、elst なし」を単独で検証できる。
fn make_subtitle_fixture(dir: &Path) -> PathBuf {
    let src = common::fixture_path();
    let srt = dir.join("in.srt");
    fs::write(
        &srt,
        "1\n00:00:00,000 --> 00:00:02,000\nhello\n\n2\n00:00:05,000 --> 00:00:07,000\nworld\n",
    )
    .expect("字幕ソースの書き込みに失敗しました");

    let out = dir.join("IN_subs.mp4");
    let status = Command::new("ffmpeg")
        .args(["-y", "-hide_banner", "-loglevel", "error", "-i"])
        .arg(&src)
        .arg("-i")
        .arg(&srt)
        .args([
            "-map",
            "0:v:0",
            "-map",
            "0:a:0",
            "-map",
            "1:s:0",
            "-c:v",
            "copy",
            "-c:a",
            "copy",
            "-c:s",
            "mov_text",
            "-use_editlist",
            "0",
        ])
        .arg(&out)
        .status()
        .expect("ffmpeg の起動に失敗しました");
    assert!(status.success(), "字幕付きフィクスチャの作成に失敗しました");
    out
}

/// 罠: 入力の隣に一時ファイル以外何も増えていないことを確認するヘルパ。
fn dir_entries(dir: &Path) -> Vec<PathBuf> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .expect("read_dir")
        .map(|e| e.expect("dir entry").path())
        .collect();
    entries.sort();
    entries
}

/// 完了条件: elst 付き mp4 に `prepare` を実行すると、キャッシュに elst なしの
/// mp4 が作られ、`cut` が使う `check_supported` がそれを受け付ける。
#[test]
#[ignore = "tests/fixtures/sample_aac.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn prepare_strips_edit_list_and_result_is_accepted_by_check_supported() {
    if common::skip_if_fixture_missing_at(&common::aac_fixture_path()) || skip_if_missing("ffmpeg")
    {
        return;
    }

    let dir = make_scratch_dir("elst");
    let input = make_elst_fixture(&dir);

    // 元ファイルには実際に elst が付いていることを確認しておく(前提が崩れて
    // いないか)。
    let original_moov = read_moov(&input).expect("元ファイルの moov を読めること");
    assert!(
        prepare::inspect_moov(&original_moov).has_edit_list,
        "テストの前提: 元ファイルに elst が付いているはず"
    );

    let outcome = prepare::run(&input, None, None).expect("prepare が成功するはず");
    assert!(outcome.ran_ffmpeg, "elst 除去のため ffmpeg を実行するはず");
    assert!(outcome.had_edit_list);
    assert_ne!(
        outcome.media_path, input,
        "elst 除去後は別ファイル(キャッシュ内)を指すはず"
    );

    let prepared_moov = read_moov(&outcome.media_path).expect("前処理済み moov を読めること");
    assert!(
        !prepare::inspect_moov(&prepared_moov).has_edit_list,
        "前処理済みファイルに elst が残っていてはいけない"
    );

    // `cut` が実際に使う入力検証(オープンGOP判定を除く)を通ることを確認する。
    // `.dtvi` はここでは無いので `check_track_counts` / `check_no_edit_list` /
    // `check_single_stsd_entry` だけが効く経路として、`.dtvi` 無し用のエラーが
    // 「オープンGOP判定不可」以外の理由でないことを確認する。
    let err = tachikaze::mp4io::support::check_supported(&prepared_moov, None)
        .expect_err(".dtvi 無しなので何らかのエラーにはなる");
    assert!(
        err.reason.contains(".dtvi"),
        "elst / トラック構成の理由で拒否されてはいけない(実際の理由: {})",
        err.reason
    );

    cleanup_cache_for(&input);
    fs::remove_dir_all(&dir).ok();
}

/// 完了条件: 字幕トラック付き mp4 から字幕サイドカーが抽出され、mp4 側からは
/// 字幕トラックが落ちている。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn prepare_extracts_subtitle_and_drops_track_from_media() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") {
        return;
    }

    let dir = make_scratch_dir("subs");
    let input = make_subtitle_fixture(&dir);

    let original_moov = read_moov(&input).expect("元ファイルの moov を読めること");
    let original_inspection = prepare::inspect_moov(&original_moov);
    assert_eq!(
        original_inspection.subtitle,
        Some(prepare::SubtitleFormat::Tx3g),
        "テストの前提: 元ファイルは Tx3g(mov_text) 字幕トラックを持つはず"
    );

    let outcome = prepare::run(&input, None, None).expect("prepare が成功するはず");
    assert!(outcome.ran_ffmpeg);
    assert!(!outcome.had_edit_list);

    let subtitle_path = outcome
        .subtitle_path
        .expect("字幕サイドカーが抽出されているはず");
    assert!(subtitle_path.exists(), "字幕サイドカーが実在するはず");
    let subtitle_content = fs::read_to_string(&subtitle_path).expect("字幕サイドカーを読めること");
    assert!(
        subtitle_content.contains("hello") && subtitle_content.contains("world"),
        "抽出した字幕の内容が入力と一致しない: {subtitle_content}"
    );

    let prepared_moov = read_moov(&outcome.media_path).expect("前処理済み moov を読めること");
    let prepared_inspection = prepare::inspect_moov(&prepared_moov);
    assert_eq!(
        prepared_inspection.subtitle, None,
        "前処理済みファイルに字幕トラックが残っていてはいけない"
    );
    assert_eq!(
        prepared_moov.trak.len(),
        2,
        "前処理済みファイルは映像+音声の2トラックのみのはず"
    );

    cleanup_cache_for(&input);
    fs::remove_dir_all(&dir).ok();
}

/// 完了条件: elst も字幕も無い入力では新しいファイルを作らない。
/// 罠: 入力ファイルの隣に何も作られない。
#[test]
fn prepare_is_noop_for_plain_fixture_and_creates_nothing_next_to_input() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let fixture = common::fixture_path();
    let fixture_dir = fixture
        .parent()
        .expect("フィクスチャに親ディレクトリがあるはず");
    let before = dir_entries(fixture_dir);

    let outcome = prepare::run(&fixture, None, None).expect("prepare が成功するはず");
    assert!(
        !outcome.ran_ffmpeg,
        "前処理不要なら ffmpeg を実行しないはず"
    );
    assert_eq!(
        outcome.media_path, fixture,
        "前処理不要なら入力をそのまま返すはず"
    );
    assert_eq!(outcome.subtitle_path, None);

    let after = dir_entries(fixture_dir);
    assert_eq!(
        before, after,
        "前処理不要な入力では、隣のディレクトリの内容が変わってはいけない"
    );
}

/// `--subs PATH` を指定すると、mp4 内蔵の字幕トラックではなく指定したファイルが
/// 使われる(mp4 内蔵の字幕トラック自体は引き続き除去される)。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn prepare_prefers_external_subs_over_embedded_track() {
    if common::skip_if_fixture_missing() || skip_if_missing("ffmpeg") {
        return;
    }

    let dir = make_scratch_dir("external-subs");
    let input = make_subtitle_fixture(&dir);
    let external = dir.join("external.ass");
    fs::write(&external, "external ass placeholder\n").expect("外部字幕の書き込みに失敗しました");

    let outcome = prepare::run(&input, None, Some(&external)).expect("prepare が成功するはず");
    assert_eq!(
        outcome.subtitle_path.as_deref(),
        Some(external.as_path()),
        "--subs で指定したパスがそのまま使われるはず"
    );

    let prepared_moov = read_moov(&outcome.media_path).expect("前処理済み moov を読めること");
    assert_eq!(
        prepare::inspect_moov(&prepared_moov).subtitle,
        None,
        "--subs 指定時も mp4 内蔵の字幕トラックは除去されるはず"
    );

    cleanup_cache_for(&input);
    fs::remove_dir_all(&dir).ok();
}
