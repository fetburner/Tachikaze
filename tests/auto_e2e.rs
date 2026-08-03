//! `tachikaze auto`（issue #62）の E2E。
//!
//! ## この環境で検証できる範囲・できない範囲
//!
//! この環境には `dtvindex` / `chapter_exe` / `join_logo_scp` の実バイナリが無い
//! （`docs/toolchain-macos.md` のビルド手順が必要で、CI 相当の最小構成では
//! 用意されていない）。そのため2種類のテストに分けている:
//!
//! 1. **子プロセスの `PATH` にシェルスクリプトの偽ツールを前置して `analyze` まで
//!    含めた完走を確認するテスト**（[`auto_completes_full_pipeline_with_fake_tools`]）。
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
//!    確認するテスト**（`--overwrite`・静的な引数検証・exit code）。
//!    `analyze` 自体は `dtvindex` が無いため必ず失敗するが、
//!    それは想定どおりの「失敗」として exit code 1 に現れることを確認する
//!    （gate 停止＝exit code 3 に実際に到達する経路は 1 のテストが担う）。

mod common;

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

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

/// `dtvindex` / `chapter_exe` / `join_logo_scp` の偽ツール一式を、`make install`
/// と同じ配置（`$PREFIX/bin/join_logo_scp` + `$PREFIX/share/join_logo_scp/JL/`、
/// `docs/toolchain-macos.md`「ビルド後の配置とインストール」節）で用意する。
/// 戻り値は `PATH` に前置するビンディレクトリ（呼び出し側が [`prepend_path`] で
/// 使う）。
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
///
/// JL コマンドファイルは `<bin_dir の親>/share/join_logo_scp/JL/JL_標準.txt` に
/// 置く。`tools::default_jl_command_file` が `join_logo_scp` の実体パス
/// （`resolve_tool` が canonicalize 済みで返す）から `../../share/...` を1段で
/// 導出するため（`src/tools.rs` の doc comment参照）、追加の環境変数は不要。
fn setup_fake_analyze_tools(tmp_dir: &Path) -> PathBuf {
    let bin_dir = tmp_dir.join("tools").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin_dir を作れること");

    let dtvi_src = dtvi_path();
    write_executable_script(
        &bin_dir.join("dtvindex"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
            dtvi_src.display()
        ),
    );

    write_executable_script(
        &bin_dir.join("chapter_exe"),
        "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
    );

    // ヘッダ行 + 総フレームを覆う単一の `:L` 行（`:CM` 無し）。
    let detail_jls = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 598 20 0 0 :L\n";
    write_executable_script(
        &bin_dir.join("join_logo_scp"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
            FULL_SUCCESS_TRIM_AVS_CONTENT, detail_jls
        ),
    );

    let jl_dir = tmp_dir
        .join("tools")
        .join("share")
        .join("join_logo_scp")
        .join("JL");
    std::fs::create_dir_all(&jl_dir).expect("jl_dir を作れること");
    std::fs::write(jl_dir.join("JL_標準.txt"), "placeholder\n").expect("JLファイルを書けること");

    bin_dir
}

/// `dir` を既存の `PATH` の先頭に前置した文字列を返す（子プロセスの `PATH` に
/// 偽ツールを注入するためのヘルパ。`--tool-dir` が無くなったため、外部ツール
/// の解決先を差し替える唯一の手段になった）。
fn prepend_path(dir: &Path) -> std::ffi::OsString {
    let mut value = dir.as_os_str().to_os_string();
    if let Some(existing) = std::env::var_os("PATH") {
        value.push(":");
        value.push(existing);
    }
    value
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

    let bin_dir = setup_fake_analyze_tools(&tmp_dir);
    let cache_root = tmp_dir.join("cache");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache_root)
        .arg("auto")
        .arg(&input)
        .env("PATH", prepend_path(&bin_dir))
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
        stderr.contains("[auto] 完了:"),
        "完了の表示が期待どおりでない: {stderr}"
    );

    let out_path = tmp_dir.join("IN_CMcut.mp4");
    let cm_path = tmp_dir.join("IN_CM.mp4");
    assert!(out_path.is_file(), "本編が出力されているはず: {stdout}");
    assert!(cm_path.is_file(), "CM側が出力されているはず: {stdout}");

    assert_eq!(
        video_frame_count(&out_path),
        FULL_SUCCESS_KEPT_PACKET_COUNT,
        "本編の映像フレーム数は120のはず"
    );
    assert_eq!(
        video_frame_count(&cm_path),
        FULL_SUCCESS_CM_PACKET_COUNT,
        "CM側の映像フレーム数は599-120=479のはず"
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

        let bin_dir = tmp_dir.join("tools").join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin_dir を作れること");
        let dtvi_src = dtvi_path();
        write_executable_script(
            &bin_dir.join("dtvindex"),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
                dtvi_src.display()
            ),
        );
        write_executable_script(
            &bin_dir.join("chapter_exe"),
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
        );
        write_executable_script(
            &bin_dir.join("join_logo_scp"),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
                FORCE_TEST_TRIM_AVS_CONTENT, detail_jls
            ),
        );
        // JL コマンドファイルは `join_logo_scp` の実体パスから `../../share/...`
        // を1段で導出する配置（`setup_fake_analyze_tools` の doc comment参照）。
        let jl_dir = tmp_dir
            .join("tools")
            .join("share")
            .join("join_logo_scp")
            .join("JL");
        std::fs::create_dir_all(&jl_dir).expect("jl_dir を作れること");
        std::fs::write(jl_dir.join("JL_標準.txt"), "placeholder\n")
            .expect("JLファイルを書けること");

        let cache_root = tmp_dir.join("cache");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
        cmd.arg("--cache-dir")
            .arg(&cache_root)
            .arg("auto")
            .arg(&input)
            .env("PATH", prepend_path(&bin_dir));
        if use_force {
            cmd.arg("--force");
        }
        let output = cmd.output().expect("tachikaze auto の起動に失敗した");
        let stderr = String::from_utf8_lossy(&output.stderr);

        let out_path = tmp_dir.join("IN_CMcut.mp4");
        if use_force {
            assert!(
                output.status.success(),
                "--force のため完走するはず: stderr={stderr}"
            );
            assert_eq!(output.status.code(), Some(0));
            assert!(
                stderr.contains("--force"),
                "--force を使って続行した旨のログが無い: {stderr}"
            );
            assert!(out_path.is_file(), "--force 指定時は cut まで進むはず");
        } else {
            assert_eq!(
                output.status.code(),
                Some(3),
                "gate が止めた場合は exit code 3 のはず: stderr={stderr}"
            );
            assert!(
                stderr.contains("gate が疑わしいと判定したため、cut を実行せず停止します"),
                "gate 停止の旨が出ていない: {stderr}"
            );
            assert!(
                stderr.contains("tachikaze cut"),
                "直して cut するコマンド例が出ていない: {stderr}"
            );
            assert!(
                stderr.contains("exit code 3"),
                "exit code 3 で停止した旨が出ていない: {stderr}"
            );
            assert!(
                !out_path.is_file(),
                "gate が止めた場合は cut を実行しないはず"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

/// 完了条件(issue #72):
/// - `analyze -o -` の出力が標準出力に出て、`-o PATH` と同じ内容になる
/// - `cut --trim -` が標準入力から trim を読める
/// - `auto` の標準出力は完全に空（診断はすべて stderr、CLAUDE.md の方針）
#[test]
#[ignore = "tests/fixtures/sample.mp4 と tests/data/sample.dtvi、ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn analyze_and_cut_support_dash_for_stdout_and_stdin() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !ffprobe_available() {
        eprintln!("ffprobe が無いためスキップします。");
        return;
    }

    let tmp_dir = make_tmp_dir("dash-stdio");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let bin_dir = setup_fake_analyze_tools(&tmp_dir);

    // `analyze -o -`: trim.avs を標準出力に書く。
    let stdout_run = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(tmp_dir.join("cache-stdout"))
        .arg("analyze")
        .arg(&input)
        .arg("-o")
        .arg("-")
        .env("PATH", prepend_path(&bin_dir))
        .output()
        .expect("tachikaze analyze -o - の起動に失敗した");
    assert!(
        stdout_run.status.success(),
        "analyze -o - が失敗した: stderr={}",
        String::from_utf8_lossy(&stdout_run.stderr)
    );
    assert_eq!(
        stdout_run.stdout,
        FULL_SUCCESS_TRIM_AVS_CONTENT.as_bytes(),
        "標準出力には trim.avs の中身だけが出るはず"
    );

    // `analyze -o PATH`: 別キャッシュで同じ入力を処理し、明示パスの中身が
    // 標準出力と完全に一致することを確認する（`raw_trim` を経由するため
    // パース→再構成に頼らずバイト一致するはず）。
    let explicit_path = tmp_dir.join("trim_explicit.avs");
    let explicit_run = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(tmp_dir.join("cache-explicit"))
        .arg("analyze")
        .arg(&input)
        .arg("-o")
        .arg(&explicit_path)
        .env("PATH", prepend_path(&bin_dir))
        .output()
        .expect("tachikaze analyze -o PATH の起動に失敗した");
    assert!(
        explicit_run.status.success(),
        "analyze -o PATH が失敗した: stderr={}",
        String::from_utf8_lossy(&explicit_run.stderr)
    );
    assert_eq!(
        stdout_run.stdout,
        std::fs::read(&explicit_path).expect("明示パスの trim.avs を読めること"),
        "-o - と -o PATH の中身が一致しないはず"
    );

    // `cut --trim -`: 上の標準出力をそのまま標準入力へ渡す。
    let out_path = tmp_dir.join("OUT.mp4");
    let mut cut_child = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(tmp_dir.join("cache-cut"))
        .arg("cut")
        .arg(&input)
        .arg("--trim")
        .arg("-")
        .arg("--dtvi")
        .arg(dtvi_path())
        .arg("-o")
        .arg(&out_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("tachikaze cut --trim - の起動に失敗した");
    cut_child
        .stdin
        .take()
        .expect("子プロセスの標準入力を取得できること")
        .write_all(&stdout_run.stdout)
        .expect("trim を標準入力へ書き込めること");
    let cut_output = cut_child
        .wait_with_output()
        .expect("tachikaze cut --trim - の終了を待てること");
    assert!(
        cut_output.status.success(),
        "cut --trim - が失敗した: stderr={}",
        String::from_utf8_lossy(&cut_output.stderr)
    );
    assert!(out_path.is_file(), "本編が出力されているはず");
    assert_eq!(
        video_frame_count(&out_path),
        FULL_SUCCESS_KEPT_PACKET_COUNT,
        "本編の映像フレーム数は120のはず"
    );

    // カレントディレクトリに `-` という名前のファイルが作られていないこと
    // （CLAUDE.md の罠、issue #72「罠」2）。
    assert!(
        !tmp_dir.join("-").exists(),
        "`-` という名前のファイルが作られてはいけない"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

// ---------------------------------------------------------------------
// 2. 外部ツール無しで確認できる配線ロジック（静的な引数検証・スキップ・
//    exit code 1）
// ---------------------------------------------------------------------

fn run_auto(args: &[&str], cache_root: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(cache_root)
        .arg("auto")
        .args(args)
        .output()
        .expect("tachikaze auto の起動に失敗した")
}

/// 外部ツール（dtvindex / chapter_exe / join_logo_scp）を**確実に見つけられない**
/// 状態で `tachikaze auto` を起動する。
///
/// `analyze` が必ず失敗する状態を作るためのもの。以前はこれらのテストが
/// 「この環境には dtvindex が無い」という暗黙の前提の上に立っていて、
/// `docs/toolchain-macos.md` の手順どおりに3ツールを `PATH` へ入れた開発環境
/// （＝本来の想定環境）では `analyze` が成功してしまい落ちていた。
///
/// `resolve_tool` は `PATH` だけを探す（`src/tools.rs` の doc comment）。
/// 子プロセスの `PATH` だけを空にすれば、親（テストプロセス）の状態には
/// 触れずに解決を空振りさせられる。`ffmpeg` も引けなくなるが、これらの
/// テストは elst も字幕も無いフィクスチャを使うので `prepare` は外部
/// プロセスを起動しない。
fn run_auto_without_tools(args: &[&str], cache_root: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(cache_root)
        .arg("auto")
        .args(args)
        .env("PATH", "")
        .output()
        .expect("tachikaze auto の起動に失敗した")
}

/// 完了条件: `auto` は1入力しか取らないため、複数のファイルを渡すと clap の
/// usage error（exit code 2）になる（issue #70「1本1プロセス」。繰り返しは
/// シェルのループに任せる）。
#[test]
fn auto_rejects_multiple_inputs_as_usage_error() {
    let tmp_dir = make_tmp_dir("reject-multi-input");
    let output = run_auto(&["/no/such/a.mp4", "/no/such/b.mp4"], &tmp_dir);
    assert_eq!(output.status.code(), Some(2), "clap の usage error のはず");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument"),
        "clap の usage error であることを確認する: stderr={stderr}"
    );
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件（issue #71）: 存在しないオプションを渡すと clap の usage error で
/// exit code 2 になり、gate 停止の exit code 3
/// （[`auto_force_overrides_gate_stop_but_gate_alone_stops_without_it`]）と
/// 区別できる。
#[test]
fn auto_unknown_option_is_usage_error_exit_code_2() {
    let tmp_dir = make_tmp_dir("unknown-option");
    let output = run_auto(&["--no-such-option", "/no/such/a.mp4"], &tmp_dir);
    assert_eq!(output.status.code(), Some(2), "clap の usage error のはず");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument"),
        "clap の usage error であることを確認する: stderr={stderr}"
    );
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

/// 完了条件: 1本の入力が無ければエラーとして exit code 1 になる。
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
    assert!(stderr.contains("入力がありません"), "stderr={stderr}");
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: 既存の出力（本編）があるときは既定でスキップし、exit code は 0
/// （スキップは失敗でも判定停止でもない、`src/auto.rs` の doc comment参照）。
/// 罠: 再実行で成果物を黙って潰さない — 既存ファイルの中身が変わって
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
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "スキップは成功扱いのはず: {stderr}"
    );
    assert_eq!(output.status.code(), Some(0));
    assert!(stderr.contains("既存の出力があるためスキップ"), "{stderr}");
    assert_eq!(
        std::fs::read(&existing_out).expect("既存出力を読めること"),
        b"stale placeholder",
        "スキップ時に既存出力の中身が変わってはいけない"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--overwrite` を付けるとスキップせず実処理（`prepare` 以降）に進む。
/// [`run_auto_without_tools`] で `analyze` を必ず失敗させるが、それは
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

    let output = run_auto_without_tools(
        &["--overwrite", input.to_str().unwrap()],
        &tmp_dir.join("cache"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    // 外部ツールを引けない子プロセスなので analyze で失敗する = exit code 1。
    assert_eq!(output.status.code(), Some(1), "stderr={stderr}");
    assert!(
        !stderr.contains("既存の出力があるためスキップします"),
        "--overwrite 指定時はスキップしないはず: {stderr}"
    );
    assert!(
        stderr.contains("[auto] prepare"),
        "--overwrite 指定時は prepare まで進むはず: {stderr}"
    );
    assert!(
        stderr.contains("analyze に失敗しました"),
        "analyze で失敗した旨が出ていない: stderr={stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--analyze-only` で `cut` へ進まない。[`run_auto_without_tools`] で
/// analyze を必ず失敗させ、失敗するのが `cut` ではなく `analyze` の段階であること
/// （`prepare` は実行されるが `[auto] cut:` は出ないこと）を確認する。
///
/// 「止めたあと `cut` 単体で続けられる（`trim`/`dtvi` のパスが表示される）」側は
/// analyze が成功しないと確認できないため、実ツールを使う `#[ignore]` 付きの
/// E2E が担当する。
#[test]
fn auto_analyze_only_stops_before_cut() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("analyze-only");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

    let output = run_auto_without_tools(
        &["--analyze-only", input.to_str().unwrap()],
        &tmp_dir.join("cache"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(output.status.code(), Some(1), "stderr={stderr}");
    assert!(
        stderr.contains("[auto] prepare"),
        "prepare は実行されるはず: {stderr}"
    );
    assert!(
        !stderr.contains("[auto] cut:"),
        "--analyze-only では cut を実行しないはず: {stderr}"
    );
    assert!(
        stderr.contains("analyze に失敗しました"),
        "analyze の段階で失敗した旨が出ていない: stderr={stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
