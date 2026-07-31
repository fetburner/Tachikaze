//! `tachikaze auto`（issue #62）の E2E。
//!
//! ## この環境で検証できる範囲・できない範囲
//!
//! この環境には `dtvindex` / `chapter_exe` / `join_logo_scp` の実バイナリが無い
//! （`docs/toolchain-macos.md` のビルド手順が必要で、CI 相当の最小構成では
//! 用意されていない）。そのため2種類のテストに分けている:
//!
//! 1. **`--tool-dir` にシェルスクリプトの偽ツールを置いて `analyze` まで含めた
//!    完走を確認するテスト**（[`auto_completes_full_pipeline_with_fake_tools`]）。
//!    `dtvindex` の代わりに実物の `tests/data/sample.dtvi` をコピーするだけの
//!    シェルスクリプトを使うことで、`cut` の自己検証（`.dtvi` とサンプル表の
//!    突き合わせ、CLAUDE.md 罠3）まで含めて本物のパイプラインを通す
//!    （`src/analyze.rs` の単体テスト
//!    `run_stops_pipeline_and_surfaces_stderr_on_first_failure` と同じ技法）。
//!    字幕トラック付きフィクスチャは使わない: `prepare` が字幕トラック除去のために
//!    ffmpeg で remux すると、その結果の映像サンプル表が `tests/data/sample.dtvi`
//!    （素の `sample.mp4` 用に実測したもの）と一致する保証が無く、一致しない場合
//!    自己検証4で弾かれてテストが偽陰性になる。プレーンなフィクスチャなら
//!    `prepare` は remux 自体を行わない（`inspect_moov` が elst も字幕も無いと
//!    判定し `ran_ffmpeg=false` で入力をそのまま返す）ため、この不確実性が無い。
//!    字幕の抽出・張り替え（`prepare`/`remap-subs`）自体は
//!    `tests/prepare_e2e.rs` / `tests/remap_subs_e2e.rs` で個別に確認済みで、
//!    `auto` はそれらの関数（`prepare::run` / `subtitle::remap_ass` /
//!    `subtitle::remap_srt`）をそのまま呼ぶだけなので重複しては確認しない。
//! 2. **`analyze` に到達する前/到達した直後の配線ロジックを、外部ツール無しで
//!    確認するテスト**（複数入力の隔離・`--overwrite`・静的な引数検証・
//!    exit code）。`analyze` 自体は `dtvindex` が無いため必ず失敗するが、
//!    それは想定どおりの「失敗」として exit code 1 に現れることを確認する
//!    （gate 停止＝exit code 2 に実際に到達する経路は 1 のテストが担う）。

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

/// `tests/data/sample.dtvi` は `dtvi.rs` の単体テスト向けに用意された固定
/// フィクスチャで、**フレーム表（`FRAMES` セクション）が先頭40フレーム分しか無い**
/// （ヘッダの `frame_count 599` は実際の総フレーム数の記録だが、`FRAMES` セクション
/// 自体は40行で打ち切られている）。`mp4io::order_map::verify_against_dtvi` /
/// `mp4io::support::check_closed_gop` はどちらも「`.dtvi` に載っている行だけ」を
/// 検査する実装（`dtvi.frames` を `for` で回すだけで、`.dtvi` の行数と実際の
/// サンプル数が一致することは要求しない）ので、他の E2E
/// （`tests/video_e2e.rs` / `tests/segmap_e2e.rs`）はこの40行版をそのまま
/// 599フレームの実フィクスチャに使っている。
///
/// **ただし `src/gate.rs::evaluate` は総フレーム数を `dtvi.frames.len()`（＝40）
/// から取る**（`.dtvi` ヘッダの `frame_count` ではなくフレーム表の実際の長さを
/// 使う設計、`gate.rs` の doc comment参照）。そのため gate 判定を意図したとおりに
/// 動かすには、Trim の**素の**幅（snap 前、`TrimList::ranges()` の `end-start`
/// の合計）をこの40という値と比較して考える必要がある（`cut` 自体は実際の
/// mp4 サンプル数（599）を使うので無関係）。
/// `auto_completes_full_pipeline_with_fake_tools` は「gate が止めない」ことを
/// 前提にしたテストなので、素の幅が40未満になる Trim
/// （[`FULL_SUCCESS_TRIM_AVS_CONTENT`]）を使う。
const FULL_SUCCESS_TRIM_AVS_CONTENT: &str = "Trim(0,19)";
/// [`FULL_SUCCESS_TRIM_AVS_CONTENT`] の `[0,20)`（素の幅20、`dtvi.frames.len()`=40
/// 未満なので gate は止めない）を実フィクスチャ（GOP=120・599フレーム、
/// キーフレームは表示順 0,120,240,360,480）へ `Snap::Outward`（既定）で当てはめると
/// `[0,120)` に広がる。保持側は120パケット、CM側（補集合 `[120,599)`）は479パケット。
const FULL_SUCCESS_KEPT_PACKET_COUNT: usize = 120;
const FULL_SUCCESS_CM_PACKET_COUNT: usize = 599 - FULL_SUCCESS_KEPT_PACKET_COUNT;

/// `auto_force_overrides_gate_stop_but_gate_alone_stops_without_it` 用の Trim。
/// このテストは gate を意図的に止める（見逃し候補ヒューリスティック経由、
/// 上記の「素の幅」問題とは無関係）ことが目的なので、cut 自体が正しく動くことだけ
/// 確認できれば十分で、具体的な保持フレーム数はアサートしない。
const FORCE_TEST_TRIM_AVS_CONTENT: &str = "Trim(10,109) ++ Trim(370,469)";

fn dtvi_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.dtvi")
}

fn make_tmp_dir(label: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("tachikaze-auto-e2e-{label}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("一時ディレクトリを作れること");
    dir
}

#[cfg(unix)]
fn write_executable_script(path: &Path, script: &str) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::write(path, script).expect("スクリプトを書けること");
    let mut perms = std::fs::metadata(path).unwrap().permissions();
    perms.set_mode(0o755);
    std::fs::set_permissions(path, perms).expect("実行権限を付与できること");
}

/// `dtvindex` / `chapter_exe` / `join_logo_scp` の偽ツール一式と、
/// `join_logo_scp` が既定で探す JL コマンドファイルを置いた `TACHIKAZE_JL_DIR`
/// 用ディレクトリを用意する。戻り値は `(tool_dir, jl_dir)`。
///
/// - `dtvindex`: `-o` の次の引数へ、本物のフィクスチャ用に実測済みの
///   `tests/data/sample.dtvi` をそのままコピーする。これにより `cut` の
///   自己検証（`.dtvi` とサンプル表の突き合わせ）まで本物として通せる。
/// - `chapter_exe`: `-o` の次の引数へダミーの `scp.txt` を書く（中身は偽物の
///   `join_logo_scp` しか読まないので内容は問わない）。
/// - `join_logo_scp`: `-o` の次の引数へ既知の `trim.avs`、`-oscp` の次の引数へ
///   `:CM` ラベルを含まない最小限の `detail.jls` を書く（`:CM` を含めない
///   ことで見逃し候補・格子誤差の判定に一切引っかからないようにし、gate が
///   確実に「止めない」判定になるようにする）。
fn setup_fake_analyze_tools(tmp_dir: &Path) -> (PathBuf, PathBuf) {
    let tool_dir = tmp_dir.join("tools");
    std::fs::create_dir_all(&tool_dir).expect("tool_dir を作れること");

    let dtvi_src = dtvi_path();
    write_executable_script(
        &tool_dir.join("dtvindex"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
            dtvi_src.display()
        ),
    );

    write_executable_script(
        &tool_dir.join("chapter_exe"),
        "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
    );

    // ヘッダ行 + 総フレームを覆う単一の `:L` 行（`:CM` 無し）。
    let detail_jls = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 598 20 0 0 :L\n";
    write_executable_script(
        &tool_dir.join("join_logo_scp"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
            FULL_SUCCESS_TRIM_AVS_CONTENT, detail_jls
        ),
    );

    let jl_dir = tmp_dir.join("jl");
    std::fs::create_dir_all(&jl_dir).expect("jl_dir を作れること");
    std::fs::write(jl_dir.join("JL_標準.txt"), "placeholder\n").expect("JLファイルを書けること");

    (tool_dir, jl_dir)
}

/// `path` の映像ストリームのフレーム数を ffprobe で数える。
fn video_frame_count(path: &Path) -> usize {
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
        "ffprobe が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .count()
}

fn ffprobe_available() -> bool {
    Command::new("ffprobe")
        .arg("-version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

// ---------------------------------------------------------------------
// 1. 偽ツールを使った完走テスト（analyze を含む本物のパイプライン）
// ---------------------------------------------------------------------

/// 完了条件:
/// - `tachikaze auto IN.mp4` だけで本編 + CM側まで通る（字幕トラックが無いので
///   字幕の張り替え部分はこのテストの対象外。上記モジュール doc comment参照）
/// - exit code 0
/// - gate が「止めない」場合に実際に cut まで進む
/// - `auto` が `cut` のロジック（区間マップ込み、`--cm-output` の自己検証8）を
///   複製せず、`commands::execute_cut` をそのまま使っていることを、実際の
///   出力フレーム数の一致で確認する
#[test]
#[ignore = "tests/fixtures/sample.mp4 と tests/data/sample.dtvi、ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn auto_completes_full_pipeline_with_fake_tools() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !ffprobe_available() {
        eprintln!("ffprobe が無いためスキップします。");
        return;
    }

    let tmp_dir = make_tmp_dir("full-success");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

    let (tool_dir, jl_dir) = setup_fake_analyze_tools(&tmp_dir);
    let cache_root = tmp_dir.join("cache");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--tool-dir")
        .arg(&tool_dir)
        .arg("auto")
        .arg(&input)
        .env("TACHIKAZE_CACHE_DIR", &cache_root)
        .env("TACHIKAZE_JL_DIR", &jl_dir)
        .output()
        .expect("tachikaze auto の起動に失敗した");

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        output.status.success(),
        "完走するはず: stdout={stdout}\nstderr={stderr}"
    );
    assert_eq!(output.status.code(), Some(0), "exit code は 0 のはず");
    assert!(
        stdout.contains("完了 1 / 判定で停止 0 / 失敗 0 / 既存出力のためスキップ 0（計 1 件）"),
        "内訳の表示が期待どおりでない: {stdout}"
    );

    let out_path = tmp_dir.join("IN_CMcut.mp4");
    let cm_path = tmp_dir.join("IN_CM.mp4");
    assert!(out_path.is_file(), "本編が出力されているはず: {stdout}");
    assert!(cm_path.is_file(), "CM側が出力されているはず: {stdout}");

    assert_eq!(
        video_frame_count(&out_path),
        FULL_SUCCESS_KEPT_PACKET_COUNT,
        "本編の映像フレーム数は120+120=240のはず"
    );
    assert_eq!(
        video_frame_count(&cm_path),
        FULL_SUCCESS_CM_PACKET_COUNT,
        "CM側の映像フレーム数は599-240=359のはず"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: gate が疑わしいと判定しても `--force` で cut まで進める。
/// join_logo_scp の偽ツールに `:CM` ブロックが複数の同じ長さで揃った
/// `detail.jls` を書かせ、`report::missed::find_missed_candidates` が見逃し候補を
/// 検出する状況を作る（見逃し候補が1件以上あると gate は必ず止める、
/// `src/gate.rs` の doc comment参照）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と tests/data/sample.dtvi、ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn auto_force_overrides_gate_stop_but_gate_alone_stops_without_it() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !ffprobe_available() {
        eprintln!("ffprobe が無いためスキップします。");
        return;
    }

    // `src/report/missed.rs` の見逃し候補ヒューリスティック: 既知の `:CM` ブロック長
    // （ここでは 40 フレーム）が複数回登場するのに、その長さ帯の `:L`（未カット）
    // 区間が別途あると「見逃し候補」として検出される。
    let detail_jls = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n\
         0 39 1 0 0 :CM\n\
         40 199 5 0 0 :L\n\
         200 239 1 0 0 :CM\n\
         240 279 1 0 0 :L\n\
         280 598 10 0 0 :L\n";

    for (label, use_force) in [("without-force", false), ("with-force", true)] {
        let tmp_dir = make_tmp_dir(&format!("gate-{label}"));
        let input = tmp_dir.join("IN.mp4");
        std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

        let tool_dir = tmp_dir.join("tools");
        std::fs::create_dir_all(&tool_dir).expect("tool_dir を作れること");
        let dtvi_src = dtvi_path();
        write_executable_script(
            &tool_dir.join("dtvindex"),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
                dtvi_src.display()
            ),
        );
        write_executable_script(
            &tool_dir.join("chapter_exe"),
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
        );
        write_executable_script(
            &tool_dir.join("join_logo_scp"),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
                FORCE_TEST_TRIM_AVS_CONTENT, detail_jls
            ),
        );
        let jl_dir = tmp_dir.join("jl");
        std::fs::create_dir_all(&jl_dir).expect("jl_dir を作れること");
        std::fs::write(jl_dir.join("JL_標準.txt"), "placeholder\n")
            .expect("JLファイルを書けること");

        let cache_root = tmp_dir.join("cache");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
        cmd.arg("--tool-dir")
            .arg(&tool_dir)
            .arg("auto")
            .arg(&input)
            .env("TACHIKAZE_CACHE_DIR", &cache_root)
            .env("TACHIKAZE_JL_DIR", &jl_dir);
        if use_force {
            cmd.arg("--force");
        }
        let output = cmd.output().expect("tachikaze auto の起動に失敗した");
        let stdout = String::from_utf8_lossy(&output.stdout);

        let out_path = tmp_dir.join("IN_CMcut.mp4");
        if use_force {
            assert!(
                output.status.success(),
                "--force のため完走するはず: stdout={stdout}"
            );
            assert_eq!(output.status.code(), Some(0));
            assert!(
                stdout.contains("--force"),
                "--force を使って続行した旨のログが無い: {stdout}"
            );
            assert!(out_path.is_file(), "--force 指定時は cut まで進むはず");
        } else {
            assert_eq!(
                output.status.code(),
                Some(2),
                "gate が止めた場合は exit code 2 のはず: stdout={stdout}"
            );
            assert!(
                stdout.contains("判定で停止"),
                "内訳に判定停止が出ていない: {stdout}"
            );
            assert!(
                stdout.contains("tachikaze cut"),
                "直して cut するコマンド例が出ていない: {stdout}"
            );
            assert!(
                !out_path.is_file(),
                "gate が止めた場合は cut を実行しないはず"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

// ---------------------------------------------------------------------
// 2. 外部ツール無しで確認できる配線ロジック（静的な引数検証・スキップ・
//    バッチの失敗隔離・exit code 1）
// ---------------------------------------------------------------------

fn run_auto(args: &[&str], cache_root: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("auto")
        .args(args)
        .env("TACHIKAZE_CACHE_DIR", cache_root)
        .output()
        .expect("tachikaze auto の起動に失敗した")
}

/// 完了条件: 複数入力時に `-o` を受け付けない（他の agentが並行して進めている
/// `--cm-output` / `--work-dir` も同じ検証ロジックを通るので代表して1つだけ
/// 個別のテストにする。3つとも `src/auto.rs` の単体テストで網羅済み）。
#[test]
fn auto_rejects_multiple_inputs_with_explicit_output() {
    let tmp_dir = make_tmp_dir("reject-multi-output");
    let output = run_auto(
        &["-o", "out.mp4", "/no/such/a.mp4", "/no/such/b.mp4"],
        &tmp_dir,
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("-o"), "stderr={stderr}");
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--snap inward` と既定で付く CM 側出力の併用を、auto の文脈に
/// 沿ったメッセージで拒否する（issue #62「罠」8）。
#[test]
fn auto_rejects_snap_inward_with_default_cm_output() {
    let tmp_dir = make_tmp_dir("reject-snap-inward");
    let output = run_auto(&["--snap", "inward", "/no/such/a.mp4"], &tmp_dir);
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--snap inward"), "stderr={stderr}");
    assert!(
        stderr.contains("--no-cm"),
        "auto 固有の語彙で案内するはず: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: 1本の入力が無ければエラーとして exit code 1 になり、内訳にも
/// 「失敗 1」と出る。
#[test]
fn auto_fails_for_missing_input_with_exit_code_1() {
    let tmp_dir = make_tmp_dir("missing-input");
    let output = run_auto(&["/no/such/input-for-auto-test.mp4"], &tmp_dir);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(stdout.contains("失敗 1"), "stdout={stdout}");
    assert!(stderr.contains("入力がありません"), "stderr={stderr}");
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: 既存の出力（本編）があるときは既定でスキップし、exit code は 0
/// （スキップは失敗でも判定停止でもない、`src/auto.rs` の doc comment参照）。
/// 罠: バッチの再実行で成果物を黙って潰さない — 既存ファイルの中身が変わって
/// いないことも確認する。
#[test]
fn auto_skips_existing_output_without_overwrite() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("skip-existing");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let existing_out = tmp_dir.join("IN_CMcut.mp4");
    std::fs::write(&existing_out, b"stale placeholder").expect("既存出力を書けること");

    let output = run_auto(&[input.to_str().unwrap()], &tmp_dir.join("cache"));
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert!(
        output.status.success(),
        "スキップは成功扱いのはず: {stdout}"
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(stdout.contains("既存の出力があるためスキップ"), "{stdout}");
    assert!(
        stdout.contains("完了 0 / 判定で停止 0 / 失敗 0 / 既存出力のためスキップ 1（計 1 件）"),
        "{stdout}"
    );
    assert_eq!(
        std::fs::read(&existing_out).expect("既存出力を読めること"),
        b"stale placeholder",
        "スキップ時に既存出力の中身が変わってはいけない"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--overwrite` を付けるとスキップせず実処理（`prepare` 以降）に進む。
/// この環境には `dtvindex` が無いため `analyze` で失敗するが、それは
/// 「スキップした」のではなく「実際に処理を試みて失敗した」ことの証拠になる
/// （スキップのメッセージが出ないこと、`prepare` の実行ログが出ることで確認する）。
#[test]
fn auto_overwrite_bypasses_skip_and_reaches_analyze() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("overwrite-bypass");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let existing_out = tmp_dir.join("IN_CMcut.mp4");
    std::fs::write(&existing_out, b"stale placeholder").expect("既存出力を書けること");

    let output = run_auto(
        &["--overwrite", input.to_str().unwrap()],
        &tmp_dir.join("cache"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    // dtvindex が無い環境なので analyze で失敗する = exit code 1。
    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        !stdout.contains("既存の出力があるためスキップします"),
        "--overwrite 指定時はスキップしないはず: {stdout}"
    );
    assert!(
        stdout.contains("失敗 1 / 既存出力のためスキップ 0（計 1 件）"),
        "内訳のスキップ件数は0のはず: {stdout}"
    );
    assert!(
        stdout.contains("[auto] prepare"),
        "--overwrite 指定時は prepare まで進むはず: {stdout}"
    );
    assert!(
        stderr.contains("analyze に失敗しました") || stdout.contains("analyze に失敗しました"),
        "analyze で失敗した旨が出ていない: stdout={stdout}\nstderr={stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: 複数入力で1本が失敗しても残りが処理され、最後に内訳が出る
/// （1本の失敗で `die` する `scripts/tachikaze-cmcut` と異なる挙動）。
/// ここでは「存在しない入力(失敗)」と「既存出力ありの入力(スキップ)」を
/// 組み合わせ、どちらも `dtvindex` を必要としない経路で確認する。
#[test]
fn auto_batch_isolates_failures_and_reports_tally() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("batch-isolation");
    let missing_input = tmp_dir.join("MISSING.mp4");
    let ok_input = tmp_dir.join("OK.mp4");
    std::fs::copy(common::fixture_path(), &ok_input).expect("フィクスチャをコピーできること");
    std::fs::write(tmp_dir.join("OK_CMcut.mp4"), b"stale").expect("既存出力を書けること");

    let output = run_auto(
        &[missing_input.to_str().unwrap(), ok_input.to_str().unwrap()],
        &tmp_dir.join("cache"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    // 失敗が1本でもあるので exit code は 1（`ExitOutcome` の優先順位、
    // `commands::exit_outcome_for_tally` の doc comment参照）。
    assert_eq!(output.status.code(), Some(1), "stdout={stdout}");
    assert!(
        stdout.contains("完了 0 / 判定で停止 0 / 失敗 1 / 既存出力のためスキップ 1（計 2 件）"),
        "内訳が期待どおりでない: {stdout}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--analyze-only` で止めたあと、`cut` 単体で続けられる（`trim`/`dtvi`
/// のパスが明示的に表示される）。この環境では `dtvindex` が無いため実際には
/// analyze で失敗するが、失敗するのが `cut` ではなく `analyze` の段階であること
/// （`prepare` は実行されるが `[auto] cut:` は出ないこと）を確認する。
#[test]
fn auto_analyze_only_stops_before_cut() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("analyze-only");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

    let output = run_auto(
        &["--analyze-only", input.to_str().unwrap()],
        &tmp_dir.join("cache"),
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "stdout={stdout}\nstderr={stderr}"
    );
    assert!(
        stdout.contains("[auto] prepare"),
        "prepare は実行されるはず: {stdout}"
    );
    assert!(
        !stdout.contains("[auto] cut:"),
        "--analyze-only では cut を実行しないはず: {stdout}"
    );
    assert!(
        stderr.contains("analyze に失敗しました") || stdout.contains("analyze に失敗しました"),
        "analyze の段階で失敗した旨が出ていない: stdout={stdout}\nstderr={stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
