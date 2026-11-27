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
//!    確認するテスト**（`-f`/`--force`・静的な引数検証・exit code）。
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

/// `auto_ignore_gate_overrides_gate_stop_but_gate_alone_stops_without_it` 用の Trim。
/// このテストは gate を意図的に止める（見逃し候補ヒューリスティック経由、
/// 上記の「素の幅」問題とは無関係）ことが目的なので、cut 自体が正しく動くことだけ
/// 確認できれば十分で、具体的な保持フレーム数はアサートしない。
const IGNORE_GATE_TEST_TRIM_AVS_CONTENT: &str = "Trim(10,109) ++ Trim(370,469)";

/// `label` に `"auto-e2e-"` を付けて [`common::make_tmp_dir`] を呼ぶ薄いラッパ。
/// 呼び出し側の見た目を変えないためだけに残す（実際のディレクトリ名は元と同じ
/// `tachikaze-auto-e2e-<label>-<pid>`）。
fn make_tmp_dir(label: &str) -> PathBuf {
    common::make_tmp_dir(&format!("auto-e2e-{label}"))
}

/// `dtvindex` / `chapter_exe` / `join_logo_scp` の偽ツール一式を、`make install`
/// と同じ配置（`$PREFIX/bin/join_logo_scp` + `$PREFIX/share/join_logo_scp/JL/`、
/// `docs/toolchain-macos.md`「ビルド後の配置とインストール」節）で用意する。
/// 戻り値は `PATH` に前置するビンディレクトリ（呼び出し側が [`common::prepend_path`] で
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
    // ヘッダ行 + 総フレームを覆う単一の `:L` 行（`:CM` 無し）。
    let detail_jls = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 598 20 0 0 :L\n";
    setup_fake_tools_with(
        tmp_dir,
        &common::dtvi_path(),
        FULL_SUCCESS_TRIM_AVS_CONTENT,
        detail_jls,
    )
}

/// [`setup_fake_analyze_tools`] の一般化。`dtvindex` が書く `.dtvi` の元ファイル、
/// `join_logo_scp` が書く trim と `detail.jls` を指定できる（`--mark` のテストは
/// 全599フレームの `.dtvi` と、gate を止める `detail.jls` を使うため）。
fn setup_fake_tools_with(
    tmp_dir: &Path,
    dtvi_src: &Path,
    trim_avs: &str,
    detail_jls: &str,
) -> PathBuf {
    let bin_dir = tmp_dir.join("tools").join("bin");
    std::fs::create_dir_all(&bin_dir).expect("bin_dir を作れること");

    common::write_executable_script(
        &bin_dir.join("dtvindex"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
            dtvi_src.display()
        ),
    );

    common::write_executable_script(
        &bin_dir.join("chapter_exe"),
        "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
    );

    common::write_executable_script(
        &bin_dir.join("join_logo_scp"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
            trim_avs, detail_jls
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
/// - `tachikaze auto IN.mp4 -o OUT.mp4 --cm-output OUT_CM.mp4` で本編 + CM側まで
///   通る（字幕トラックが無いので字幕の張り替え部分はこのテストの対象外。
///   上記モジュール doc comment参照）
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
        common::skip("ffprobe が無いためスキップします。");
        return;
    }

    let tmp_dir = make_tmp_dir("full-success");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

    let bin_dir = setup_fake_analyze_tools(&tmp_dir);
    let cache_root = tmp_dir.join("cache");
    let out_path = tmp_dir.join("OUT.mp4");
    let cm_path = tmp_dir.join("OUT_CM.mp4");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache_root)
        .arg("auto")
        .arg(&input)
        .arg("-o")
        .arg(&out_path)
        .arg("--cm-output")
        .arg(&cm_path)
        // このテストはロゴ検出を確認するものではない。`--logo`/`--no-logo` 省略時は
        // E18-5 以降ロゴの自動推定（ffmpeg 起動・ロゴ辞書の読み書き）が既定で
        // 走るため、無関係な依存を増やさないよう明示的に無効化する
        // （ロゴ関連の挙動は tests/analyze_logo_e2e.rs が個別に確認する）。
        .arg("--no-logo")
        .env("PATH", common::prepend_path(&bin_dir))
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

/// 完了条件: gate が疑わしいと判定しても `--ignore-gate` で cut まで進める。
/// join_logo_scp の偽ツールに `:CM` ブロックが複数の同じ長さで揃った
/// `detail.jls` を書かせ、`report::missed::find_missed_candidates` が見逃し候補を
/// 検出する状況を作る（見逃し候補が1件以上あると gate は必ず止める、
/// `src/gate.rs` の doc comment参照）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と tests/data/sample.dtvi、ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn auto_ignore_gate_overrides_gate_stop_but_gate_alone_stops_without_it() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !ffprobe_available() {
        common::skip("ffprobe が無いためスキップします。");
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

    for (label, ignore_gate) in [("without-ignore-gate", false), ("with-ignore-gate", true)] {
        let tmp_dir = make_tmp_dir(&format!("gate-{label}"));
        // 空白と `'` を含む名前にして、案内のコマンド例の引用符付けも通す。
        let input = tmp_dir.join("IN rec's.mp4");
        std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

        let bin_dir = tmp_dir.join("tools").join("bin");
        std::fs::create_dir_all(&bin_dir).expect("bin_dir を作れること");
        let dtvi_src = common::dtvi_path();
        common::write_executable_script(
            &bin_dir.join("dtvindex"),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
                dtvi_src.display()
            ),
        );
        common::write_executable_script(
            &bin_dir.join("chapter_exe"),
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
        );
        common::write_executable_script(
            &bin_dir.join("join_logo_scp"),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
                IGNORE_GATE_TEST_TRIM_AVS_CONTENT, detail_jls
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
        let out_path = tmp_dir.join("OUT.mp4");
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
        cmd.arg("--cache-dir")
            .arg(&cache_root)
            .arg("auto")
            .arg(&input)
            .arg("-o")
            .arg(&out_path)
            // ロゴ検出は本テストの対象外（上記 `--no-logo` のコメントと同じ理由）。
            .arg("--no-logo")
            .env("PATH", common::prepend_path(&bin_dir));
        if ignore_gate {
            cmd.arg("--ignore-gate");
        }
        let output = cmd.output().expect("tachikaze auto の起動に失敗した");
        let stderr = String::from_utf8_lossy(&output.stderr);

        if ignore_gate {
            assert!(
                output.status.success(),
                "--ignore-gate のため完走するはず: stderr={stderr}"
            );
            assert_eq!(output.status.code(), Some(0));
            assert!(
                stderr.contains("--ignore-gate"),
                "--ignore-gate を使って続行した旨のログが無い: {stderr}"
            );
            assert!(
                out_path.is_file(),
                "--ignore-gate 指定時は cut まで進むはず"
            );
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
            // review の例と cut の例を、引用符つきのトークン（中の `'\''` も含む）のまま
            // 比べる。引数の順序は review が `入力 --cache-dir DIR -o OUT`、cut が
            // `入力 --trim T --dtvi D -o OUT`。
            let quote = |p: &Path| format!("'{}'", p.display().to_string().replace('\'', r"'\''"));
            let review_line = stderr
                .lines()
                .find(|l| l.contains("tachikaze review "))
                .expect("review のコマンド例の行が無い");
            let review_rest = review_line
                .split("tachikaze review ")
                .nth(1)
                .expect("review の引数を取り出せること");
            let review_arg = review_rest
                .split(" --cache-dir ")
                .next()
                .expect("review の入力を取り出せること");
            let review_out = review_rest
                .split(" -o ")
                .nth(1)
                .map(|s| s.trim_end_matches(")\""))
                .expect("review の -o を取り出せること");
            let cut_line = stderr
                .lines()
                .find(|l| l.contains("tachikaze cut "))
                .expect("cut のコマンド例の行が無い");
            let cut_arg = cut_line
                .split("tachikaze cut ")
                .nth(1)
                .and_then(|rest| rest.split(" --trim ").next())
                .expect("cut の入力を取り出せること");
            let cut_out = cut_line
                .split(" -o ")
                .nth(1)
                .expect("cut の -o を取り出せること");
            assert_eq!(
                review_arg, cut_arg,
                "review と cut の入力が一致しない: {stderr}"
            );
            assert_eq!(
                review_out, cut_out,
                "review と cut の -o が一致しない: {stderr}"
            );
            assert!(
                review_line.contains(&format!(" --cache-dir {}", quote(&cache_root))),
                "auto が使った --cache-dir を review の行に渡していない: {stderr}"
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
        common::skip("ffprobe が無いためスキップします。");
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
        // ロゴ検出は本テストの対象外（`--no-logo` の理由は上記コメント参照）。
        .arg("--no-logo")
        .env("PATH", common::prepend_path(&bin_dir))
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
        .arg("--no-logo")
        .env("PATH", common::prepend_path(&bin_dir))
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
        .arg(common::dtvi_path())
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
/// （[`auto_ignore_gate_overrides_gate_stop_but_gate_alone_stops_without_it`]）と
/// 区別できる。
#[test]
fn auto_unknown_option_is_usage_error_exit_code_2() {
    let tmp_dir = make_tmp_dir("unknown-option");
    let output = run_auto(
        &[
            "--no-such-option",
            "/no/such/a.mp4",
            "-o",
            tmp_dir.join("OUT.mp4").to_str().unwrap(),
        ],
        &tmp_dir,
    );
    assert_eq!(output.status.code(), Some(2), "clap の usage error のはず");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unexpected argument"),
        "clap の usage error であることを確認する: stderr={stderr}"
    );
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--cm-output` を明示したときの `--snap inward` との併用を拒否する
/// （issue #73「やること」4。既定では CM 側出力を作らないため、`--cm-output` を
/// 指定しない限りこの検証には抵触しない）。
#[test]
fn auto_rejects_snap_inward_with_explicit_cm_output() {
    let tmp_dir = make_tmp_dir("reject-snap-inward");
    let output = run_auto(
        &[
            "--snap",
            "inward",
            "--cm-output",
            tmp_dir.join("CM.mp4").to_str().unwrap(),
            "/no/such/a.mp4",
            "-o",
            tmp_dir.join("OUT.mp4").to_str().unwrap(),
        ],
        &tmp_dir,
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--snap inward"), "stderr={stderr}");
    assert!(stderr.contains("--cm-output"), "stderr={stderr}");
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--cm-output` を指定しなければ `--snap inward` は拒否されない
/// （既定では CM 側出力を作らないため）。入力自体が無いので別のエラー
/// （"入力がありません"）にはなるが、`--snap inward` を理由にした事前拒否では
/// ないことを確認する。
#[test]
fn auto_allows_snap_inward_without_cm_output() {
    let tmp_dir = make_tmp_dir("allow-snap-inward");
    let output = run_auto(
        &[
            "--snap",
            "inward",
            "/no/such/a.mp4",
            "-o",
            tmp_dir.join("OUT.mp4").to_str().unwrap(),
        ],
        &tmp_dir,
    );
    assert_eq!(output.status.code(), Some(1));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("入力がありません"),
        "「--snap inward」を理由にした事前拒否ではないはず: {stderr}"
    );
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: 1本の入力が無ければエラーとして exit code 1 になる。
#[test]
fn auto_fails_for_missing_input_with_exit_code_1() {
    let tmp_dir = make_tmp_dir("missing-input");
    let output = run_auto(
        &[
            "/no/such/input-for-auto-test.mp4",
            "-o",
            tmp_dir.join("OUT.mp4").to_str().unwrap(),
        ],
        &tmp_dir,
    );
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

/// 完了条件: `auto` は `-o` を必須にする（issue #73「やること」3）ため、
/// 省略すると clap の usage error（exit code 2）になる。
#[test]
fn auto_requires_output_as_usage_error() {
    let tmp_dir = make_tmp_dir("require-output");
    let output = run_auto(&["/no/such/a.mp4"], &tmp_dir);
    assert_eq!(output.status.code(), Some(2), "clap の usage error のはず");
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件(issue #73「やること」5): 字幕サイドカーの既存判定が `-o` の stem
/// （入力の stem ではなく）から導出されることを固定する。`-o OUT.mp4` に対して
/// `OUT.ass`（`-o` の stem）を先に置くとスキップし、入力の stem 由来のパス
/// （`IN.ass`）は無関係であることを確認する。
///
/// [`run_auto_without_tools`] を使う: もし字幕サイドカーの既存判定が入力の stem
/// から導出する古い実装に戻ってしまっていたら、`OUT.ass` を見つけられずスキップ
/// せず実処理へ進む。外部ツールが引けない環境では実処理は必ず失敗する（exit 1）
/// ため、リグレッションが「実ツールがあるので黙って成功する」という偽陰性に
/// ならない。
#[test]
fn auto_detects_existing_subtitle_sidecar_from_output_stem() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("subs-sidecar-stem");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let out_path = tmp_dir.join("OUT.mp4");
    let existing_ass = tmp_dir.join("OUT.ass");
    std::fs::write(&existing_ass, b"stale ass").expect("既存字幕を書けること");

    let output = run_auto_without_tools(
        &[input.to_str().unwrap(), "-o", out_path.to_str().unwrap()],
        &tmp_dir.join("cache"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert!(
        output.status.success(),
        "OUT.ass の存在だけでスキップして exit 0 になるはず: {stderr}"
    );
    assert!(
        stderr.contains(&existing_ass.display().to_string()),
        "-o の stem から導出した字幕サイドカーが既存出力として検出されるはず: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: 既存の出力（本編）があるときは既定でスキップし、exit code は 0
/// （スキップは失敗でも判定停止でもない、`src/auto.rs` の doc comment参照）。
/// 罠: 再実行で成果物を黙って潰さない — 既存ファイルの中身が変わって
/// いないことも確認する。
#[test]
fn auto_skips_existing_output_without_force() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("skip-existing");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let existing_out = tmp_dir.join("OUT.mp4");
    std::fs::write(&existing_out, b"stale placeholder").expect("既存出力を書けること");

    let output = run_auto(
        &[
            input.to_str().unwrap(),
            "-o",
            existing_out.to_str().unwrap(),
        ],
        &tmp_dir.join("cache"),
    );
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

/// 完了条件: `-f`/`--force` を付けるとスキップせず実処理（`prepare` 以降）に進む。
/// [`run_auto_without_tools`] で `analyze` を必ず失敗させるが、それは
/// 「スキップした」のではなく「実際に処理を試みて失敗した」ことの証拠になる
/// （スキップのメッセージが出ないこと、`prepare` の実行ログが出ることで確認する）。
#[test]
fn auto_force_bypasses_skip_and_reaches_analyze() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("force-bypass");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let existing_out = tmp_dir.join("OUT.mp4");
    std::fs::write(&existing_out, b"stale placeholder").expect("既存出力を書けること");

    let output = run_auto_without_tools(
        &[
            "-f",
            input.to_str().unwrap(),
            "-o",
            existing_out.to_str().unwrap(),
        ],
        &tmp_dir.join("cache"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    // 外部ツールを引けない子プロセスなので analyze で失敗する = exit code 1。
    assert_eq!(output.status.code(), Some(1), "stderr={stderr}");
    assert!(
        !stderr.contains("既存の出力があるためスキップします"),
        "-f 指定時はスキップしないはず: {stderr}"
    );
    assert!(
        stderr.contains("[auto] prepare"),
        "-f 指定時は prepare まで進むはず: {stderr}"
    );
    assert!(
        stderr.contains("analyze に失敗しました"),
        "analyze で失敗した旨が出ていない: stderr={stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件（issue #73「罠」）: `--ignore-gate` が無視するのは gate の判定だけで、
/// `.dtvi` 必須（CLAUDE.md 罠3）や自己検証は緩めない。`--ignore-gate` を付けても
/// 外部ツールが引けず `.dtvi` を生成できない状態では、gate に到達する前の
/// `analyze` の段階で失敗する（[`run_auto_without_tools`] と同じ手法）。
#[test]
fn auto_ignore_gate_does_not_bypass_dtvi_requirement() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("ignore-gate-no-dtvi");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");

    let output = run_auto_without_tools(
        &[
            "--ignore-gate",
            input.to_str().unwrap(),
            "-o",
            tmp_dir.join("OUT.mp4").to_str().unwrap(),
        ],
        &tmp_dir.join("cache"),
    );
    let stderr = String::from_utf8_lossy(&output.stderr);

    assert_eq!(
        output.status.code(),
        Some(1),
        "--ignore-gate があっても .dtvi を生成できなければ失敗するはず: stderr={stderr}"
    );
    assert!(
        stderr.contains("analyze に失敗しました"),
        "gate に到達する前に analyze の段階で失敗するはず: stderr={stderr}"
    );
    assert!(
        !tmp_dir.join("OUT.mp4").is_file(),
        "cut まで進んでいないはず"
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
        &[
            "--analyze-only",
            input.to_str().unwrap(),
            "-o",
            tmp_dir.join("OUT.mp4").to_str().unwrap(),
        ],
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

// ---------------------------------------------------------------------
// 3. `--mark`（切らずに CM 位置を記録する、issue #216）
// ---------------------------------------------------------------------

/// `--mark` のテストに使う Trim。`[0,20)` が本編、`[20,599)` が CM。
const MARK_TRIM_AVS_CONTENT: &str = "Trim(0,19)";

/// 全フレームを `:L`（未カット）として覆う `detail.jls`（`:CM` 無し、gate は止めない）。
const DETAIL_JLS_ALL_L: &str = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 598 20 0 0 :L\n";

/// gate を止めるための Trim。除去区間が 100 フレームずつ2つ（同じ長さ帯が2個以上ないと
/// 「既知の CM 長」にならない、`report/missed.rs` の `known_cm_lengths`）。
const MARK_GATE_TRIM_AVS_CONTENT: &str = "Trim(0,99) ++ Trim(200,299) ++ Trim(400,499)";

/// gate を止める `detail.jls`。保持区間 `[0,100)` に、既知の除去区間と同じ長さ（100 フレーム）の
/// `:L` 区間があるため見逃し候補になる（`report::missed::find_missed_candidates`）。
const DETAIL_JLS_WITH_MISSED_CANDIDATE: &str = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n\
     0 99 5 0 0 :L\n\
     100 199 5 0 0 :CM\n\
     200 299 5 0 0 :L\n\
     300 399 5 0 0 :CM\n\
     400 499 5 0 0 :L\n\
     500 598 10 0 0 :CM\n";

/// チャプターの JSON（`ffprobe -show_chapters`）。出力の mp4 同士を比べるために使う。
fn chapters_json(path: &Path) -> String {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_chapters", "-of", "json"])
        .arg(path)
        .output()
        .expect("ffprobe を起動できること");
    assert!(
        output.status.success(),
        "ffprobe が失敗した: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// 完了条件: `--edl-only`（`--mark` 無し）・`--mark --edl-only -o`・`--mark` の
/// `--cm-output` / `--snap inward` / `--no-subtitles` / `--verify` との併用・`--mark` だけ
/// （`-o` も `--edl-only` も無い）は clap の usage error（exit code 2）。clap が実処理より前に
/// 止めるので、入力も外部ツールも要らない。
#[test]
fn auto_mark_flag_combinations_are_usage_errors() {
    let tmp_dir = make_tmp_dir("mark-usage");
    let cases: &[&[&str]] = &[
        &["/no/such/a.mp4", "--edl-only"],
        &[
            "/no/such/a.mp4",
            "--mark",
            "--edl-only",
            "-o",
            "/no/such/o.mp4",
        ],
        &[
            "/no/such/a.mp4",
            "--mark",
            "-o",
            "/no/such/o.mp4",
            "--cm-output",
            "/no/such/cm.mp4",
        ],
        &["/no/such/a.mp4", "--mark"],
        &[
            "/no/such/a.mp4",
            "--mark",
            "-o",
            "/no/such/o.mp4",
            "--snap",
            "inward",
        ],
        &[
            "/no/such/a.mp4",
            "--mark",
            "-o",
            "/no/such/o.mp4",
            "--no-subtitles",
        ],
        &[
            "/no/such/a.mp4",
            "--mark",
            "-o",
            "/no/such/o.mp4",
            "--verify",
        ],
    ];
    for args in cases {
        let output = run_auto(args, &tmp_dir);
        assert_eq!(
            output.status.code(),
            Some(2),
            "usage error のはず: {args:?}: stderr={}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&tmp_dir);
}

/// 完了条件: `--mark` で gate が止める入力は exit code 3 で止まり、`mark` のコマンド例を出し、
/// mp4 も `.edl` も書かない（`cut` の例は出さない）。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と tests/data/sample_no_logo.dtvi、ffprobe が必要。tests/fixtures/gen.sh を先に実行すること"]
fn auto_mark_gate_stop_exits_3_and_writes_nothing() {
    if common::skip_if_fixture_missing() {
        return;
    }
    if !ffprobe_available() {
        common::skip("ffprobe が無いためスキップします。");
        return;
    }

    for edl_only in [false, true] {
        let label = if edl_only { "edl-only" } else { "video" };
        let tmp_dir = make_tmp_dir(&format!("mark-gate-{label}"));
        let input = tmp_dir.join("IN.mp4");
        std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
        let bin_dir = setup_fake_tools_with(
            &tmp_dir,
            &common::no_logo_dtvi_path(),
            MARK_GATE_TRIM_AVS_CONTENT,
            DETAIL_JLS_WITH_MISSED_CANDIDATE,
        );
        let out_path = tmp_dir.join("OUT.mp4");

        let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
        cmd.arg("--cache-dir")
            .arg(tmp_dir.join("cache"))
            .arg("auto")
            .arg(&input)
            .arg("--mark")
            .arg("--no-logo")
            .env("PATH", common::prepend_path(&bin_dir));
        if edl_only {
            cmd.arg("--edl-only");
        } else {
            cmd.arg("-o").arg(&out_path);
        }
        let output = cmd.output().expect("tachikaze auto の起動に失敗した");
        let stderr = String::from_utf8_lossy(&output.stderr);

        assert_eq!(
            output.status.code(),
            Some(3),
            "gate 停止は exit 3 のはず: {stderr}"
        );
        assert!(
            stderr.contains("tachikaze mark "),
            "mark のコマンド例が出るはず: {stderr}"
        );
        assert!(
            !stderr.contains("tachikaze cut "),
            "--mark では cut の例を出さないはず: {stderr}"
        );
        assert!(!out_path.exists(), "mp4 を書かないはず");
        assert!(!tmp_dir.join("OUT.edl").exists(), "EDL を書かないはず");
        assert!(!tmp_dir.join("IN.edl").exists(), "EDL を書かないはず");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}

/// 完了条件: `--mark -o OUT.mp4` の出力（チャプターと `.edl`）が、同じ Trim と `.dtvi` で
/// `tachikaze mark <元の録画> -o` を実行した結果と同じ。
///
/// 入力は elst 付きの `sample_elst.mp4`。elst 無しの入力では prepare 後のファイルが元の
/// 録画と同じパスになり、「prepare 後のファイルを渡す」誤りを検出できないため。
#[test]
#[ignore = "tests/fixtures/sample_elst.mp4 と tests/data/sample_no_logo.dtvi、ffprobe（と ffmpeg）が必要。tests/fixtures/gen.sh を先に実行すること"]
fn auto_mark_video_matches_mark_subcommand() {
    let fixture = common::fixture_path_named("sample_elst.mp4");
    if common::skip_if_fixture_missing_at(&fixture) {
        return;
    }
    if !ffprobe_available() {
        common::skip("ffprobe が無いためスキップします。");
        return;
    }

    let tmp_dir = make_tmp_dir("mark-video-match");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(&fixture, &input).expect("フィクスチャをコピーできること");
    let bin_dir = setup_fake_tools_with(
        &tmp_dir,
        &common::no_logo_dtvi_path(),
        MARK_TRIM_AVS_CONTENT,
        DETAIL_JLS_ALL_L,
    );
    let auto_out = tmp_dir.join("AUTO.mp4");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(tmp_dir.join("cache"))
        .arg("auto")
        .arg(&input)
        .arg("--mark")
        .arg("--no-logo")
        .arg("-o")
        .arg(&auto_out)
        .env("PATH", common::prepend_path(&bin_dir))
        .output()
        .expect("tachikaze auto の起動に失敗した");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "完走するはず: stderr={stderr}");
    assert!(
        stderr.contains("[auto] prepare 完了"),
        "elst 付きなので prepare が別ファイルを作るはず（この経路を通すのが目的）: {stderr}"
    );

    // 参照: 同じ入力・同じ Trim・同じ .dtvi で mark サブコマンドを直接実行する。
    // 別ディレクトリに置き、EDL の書き出し先が衝突しないようにする。
    let ref_dir = make_tmp_dir("mark-video-match-ref");
    let ref_input = ref_dir.join("IN.mp4");
    std::fs::copy(&fixture, &ref_input).expect("フィクスチャをコピーできること");
    let trim = ref_dir.join("trim.avs");
    std::fs::write(&trim, MARK_TRIM_AVS_CONTENT).expect("trim を書けること");
    let ref_out = ref_dir.join("REF.mp4");
    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(ref_dir.join("cache"))
        .arg("mark")
        .arg(&ref_input)
        .arg("--trim")
        .arg(&trim)
        .arg("--dtvi")
        .arg(common::no_logo_dtvi_path())
        .arg("-o")
        .arg(&ref_out)
        .output()
        .expect("tachikaze mark の起動に失敗した");
    assert!(
        output.status.success(),
        "mark が完走するはず: stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );

    let auto_edl = std::fs::read_to_string(tmp_dir.join("AUTO.edl")).expect("AUTO.edl があること");
    let ref_edl = std::fs::read_to_string(ref_dir.join("REF.edl")).expect("REF.edl があること");
    assert!(auto_edl.contains("\t3\n"), "CM 区間があるはず: {auto_edl}");
    assert_eq!(auto_edl, ref_edl, "EDL が mark サブコマンドと一致するはず");
    assert_eq!(
        chapters_json(&auto_out),
        chapters_json(&ref_out),
        "チャプターが mark サブコマンドと一致するはず"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&ref_dir);
}

/// 完了条件: `--mark --edl-only` の `.edl` は、キャッシュではなく元の録画の隣にできる。
/// elst 付きフィクスチャで `prepare` が別ファイル（キャッシュ内の媒体）を作る経路を通す。
/// 中身は同じ Trim と `.dtvi` で `mark --edl-only` を元の録画に対して実行した結果と一致する。
#[test]
#[ignore = "tests/fixtures/sample_elst.mp4 と tests/data/sample_no_logo.dtvi、ffprobe（と ffmpeg）が必要。tests/fixtures/gen.sh を先に実行すること"]
fn auto_mark_edl_only_writes_next_to_original_input_with_elst() {
    let fixture = common::fixture_path_named("sample_elst.mp4");
    if common::skip_if_fixture_missing_at(&fixture) {
        return;
    }
    if !ffprobe_available() {
        common::skip("ffprobe が無いためスキップします。");
        return;
    }

    let tmp_dir = make_tmp_dir("mark-edl-only-elst");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(&fixture, &input).expect("フィクスチャをコピーできること");
    let bin_dir = setup_fake_tools_with(
        &tmp_dir,
        &common::no_logo_dtvi_path(),
        MARK_TRIM_AVS_CONTENT,
        DETAIL_JLS_ALL_L,
    );
    let cache = tmp_dir.join("cache");
    // 既存の EDL を置いておき、`-f` で上書きされることを確かめる（無しだと skip する）。
    std::fs::write(tmp_dir.join("IN.edl"), b"stale edl").expect("既存の EDL を書けること");

    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(&cache)
        .arg("auto")
        .arg(&input)
        .arg("--mark")
        .arg("--edl-only")
        .arg("--no-logo")
        .arg("-f")
        .env("PATH", common::prepend_path(&bin_dir))
        .output()
        .expect("tachikaze auto の起動に失敗した");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "完走するはず: {stderr}");
    assert!(
        stderr.contains("[auto] prepare 完了"),
        "elst 付きなので prepare が別ファイルを作るはず（この経路を通すのが目的）: {stderr}"
    );

    let edl = tmp_dir.join("IN.edl");
    assert!(edl.is_file(), "EDL は元の録画の隣に書かれるはず");
    let found_in_cache = Command::new("find")
        .arg(&cache)
        .args(["-name", "*.edl"])
        .output()
        .expect("find を起動できること");
    assert!(
        String::from_utf8_lossy(&found_in_cache.stdout)
            .trim()
            .is_empty(),
        "EDL をキャッシュ内に書かないはず"
    );

    // 参照: 同じ Trim と .dtvi で mark --edl-only を元の録画に対して直接実行する。
    let ref_dir = make_tmp_dir("mark-edl-only-elst-ref");
    let ref_input = ref_dir.join("IN.mp4");
    std::fs::copy(&fixture, &ref_input).expect("フィクスチャをコピーできること");
    let trim = ref_dir.join("trim.avs");
    std::fs::write(&trim, MARK_TRIM_AVS_CONTENT).expect("trim を書けること");
    let output = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("--cache-dir")
        .arg(ref_dir.join("cache"))
        .arg("mark")
        .arg(&ref_input)
        .arg("--trim")
        .arg(&trim)
        .arg("--dtvi")
        .arg(common::no_logo_dtvi_path())
        .arg("--edl-only")
        .output()
        .expect("tachikaze mark の起動に失敗した");
    assert!(output.status.success(), "mark が完走するはず");

    assert_eq!(
        std::fs::read_to_string(&edl).expect("IN.edl を読めること"),
        std::fs::read_to_string(ref_dir.join("IN.edl")).expect("参照の IN.edl を読めること"),
        "EDL が mark --edl-only と一致するはず"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
    let _ = std::fs::remove_dir_all(&ref_dir);
}

/// 完了条件: `--mark` で既存の出力（`OUT.edl` / `<入力の stem>.edl`）があればスキップし、
/// `-f` で実処理に進む。字幕サイドカーは判定の対象外。
/// [`run_auto_without_tools`] を使うので、スキップされなかった場合は `analyze` が必ず失敗する。
#[test]
fn auto_mark_skips_existing_outputs_without_force() {
    if common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("mark-skip");
    let input = tmp_dir.join("IN.mp4");
    std::fs::copy(common::fixture_path(), &input).expect("フィクスチャをコピーできること");
    let input_arg = input.to_str().unwrap();
    let out_arg = tmp_dir.join("OUT.mp4");
    let out_arg = out_arg.to_str().unwrap();
    let cache = tmp_dir.join("cache");

    // (a) `-o OUT.mp4` で、mp4 は無く `OUT.edl` だけがある → スキップ。
    let existing_edl = tmp_dir.join("OUT.edl");
    std::fs::write(&existing_edl, b"stale edl").expect("既存の EDL を書けること");
    let out = run_auto_without_tools(&[input_arg, "--mark", "--no-logo", "-o", out_arg], &cache);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "スキップは成功扱いのはず: {stderr}");
    assert!(stderr.contains("既存の出力があるためスキップ"), "{stderr}");
    assert_eq!(std::fs::read(&existing_edl).unwrap(), b"stale edl");

    // (b) `--edl-only` は `<入力の stem>.edl` を見る。
    let existing_in_edl = tmp_dir.join("IN.edl");
    std::fs::write(&existing_in_edl, b"stale edl").expect("既存の EDL を書けること");
    let out = run_auto_without_tools(&[input_arg, "--mark", "--no-logo", "--edl-only"], &cache);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(out.status.success(), "スキップは成功扱いのはず: {stderr}");
    assert!(stderr.contains("既存の出力があるためスキップ"), "{stderr}");
    assert_eq!(std::fs::read(&existing_in_edl).unwrap(), b"stale edl");

    // (c) `-f` を付けるとスキップせず analyze まで進み、外部ツールが無いので失敗する。
    let out = run_auto_without_tools(
        &[input_arg, "--mark", "--no-logo", "--edl-only", "-f"],
        &cache,
    );
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(
        out.status.code(),
        Some(1),
        "実処理に進んで失敗するはず: {stderr}"
    );
    assert!(!stderr.contains("既存の出力があるためスキップ"), "{stderr}");
    assert!(
        stderr.contains("[auto] analyze:"),
        "analyze まで進むはず: {stderr}"
    );

    let _ = std::fs::remove_dir_all(&tmp_dir);
}
