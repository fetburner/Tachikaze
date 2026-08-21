//! `analyze --logo`（E14-8、issue #97）の E2E。
//!
//! ## 3ツールを偽装する理由
//!
//! `dtvindex` / `chapter_exe` / `join_logo_scp` は実バイナリが無い環境でも
//! このテストが通る必要がある（CLAUDE.md「テスト」節）。`tests/auto_e2e.rs`
//! と同じ技法（`common::write_executable_script` でシェルスクリプトの偽ツールを
//! 作り、`PATH` に前置する）を使う。実物が要るのは `ffmpeg`（`make-logo` と
//! ロゴ検出のフレーム供給に使う、`docs/toolchain-macos.md` の通常の開発依存）
//! だけにする。
//!
//! 偽 `dtvindex` は `tests/data/sample_logo.dtvi`（実 `dtvindex build` 出力
//! そのもの。ヘッダ + 全599フレーム、レビュー指摘で先頭40フレームの抜粋から
//! 差し替えた。`tests/common::logo_dtvi_path` の doc comment参照）をそのまま
//! コピーする。ヘッダの `frame_count`/`width`/`height` が `sample_logo.mp4`
//! の実際の値（599 / 640 / 360）と一致していることが、ロゴ検出（実 ffmpeg で
//! 実フレームを流す）の一致検査を通すために必須。
//!
//! 偽 `join_logo_scp` は起動時の引数を1行1引数でファイルに記録する。これにより
//! `-inlogo` が渡ったかどうか・`-set` 群より前に置かれているかを検証できる
//! （issue #97「罠」: `-inlogo` の引数位置）。
//!
//! ## `.lgd` は「常時ロゴがある」クリップから学習する
//!
//! `sample_logo.mp4` はロゴを「本編」区間（0〜8秒・13〜20秒）だけに合成した
//! 検出対象で、`sample_logo_train.mp4` は同じロゴを常時合成した学習専用クリップ
//! （`tests/fixtures/gen.sh` 参照）。`make-logo` は「ロゴが常にある」前提の
//! 回帰なので、本編とCMでロゴの有無が混在するクリップをそのまま学習に使うと
//! 係数が実際の合成関係を表さず検出できない（`gen.sh` の doc comment に実測を
//! 記録済み）。学習専用クリップで作った `.lgd` を検出対象クリップに適用する
//! ことで、実運用（ロゴが安定して見えている範囲で学習し、CM を含む全体に
//! 適用する）に近い構成にしている。

mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use tachikaze::workdir;

/// 偽 `join_logo_scp` が固定で書く `trim.avs` の内容。引数（`-inlogo` の有無）に
/// 関わらず常に同じ内容を書く（このテストが見たいのは実際の CM 判定結果では
/// なく、`join_logo_scp` に渡る引数そのものと、ロゴ検出が完走するかどうか）。
const FAKE_TRIM_CONTENT: &str = "Trim(0,598)";
/// 偽 `join_logo_scp` が固定で書く `detail.jls` の内容（`:CM` を含めず、
/// 見逃し候補・gate 判定に一切引っかからない最小構成。`tests/auto_e2e.rs` と
/// 同じ技法）。
const FAKE_DETAIL_JLS_CONTENT: &str = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 598 20 0 0 :L\n";

/// `common::make_tmp_dir` を呼び、結果を `fs::canonicalize` してから返す。
///
/// `WorkDir::new`（`src/workdir.rs`）は `--cache-dir` を絶対化するだけでなく
/// 実際に使うキャッシュディレクトリを `fs::canonicalize` する（macOS では
/// `/var` → `/private/var` になる）。テスト側が期待するパスをこの関数の
/// 戻り値から組み立てておくことで、実行結果のパス（symlink 解決済み）と
/// 期待値の文字列表現を一致させる。
fn make_tmp_dir(label: &str) -> PathBuf {
    let dir = common::make_tmp_dir(&format!("analyze-logo-e2e-{label}"));
    fs::canonicalize(&dir).expect("一時ディレクトリを canonicalize できること")
}

/// `--cache-dir cache_dir` で `input` を処理したときに `analyze` が使う、
/// 入力ごとのキャッシュディレクトリ（`work.mp4.dtvi`/`scp.txt`/`trim.avs`/
/// `detail.jls`/`logoframe.txt` が置かれる場所）を返す。
/// `workdir::cached_dtvi_path` の親ディレクトリとして計算する
/// （`src/workdir.rs` の「パス規則を1か所に集約する」方針に合わせ、ここでも
/// パス名を直接組み立てない）。
fn cache_work_dir(cache_dir: &Path, input: &Path) -> PathBuf {
    workdir::cached_dtvi_path(Some(cache_dir), input)
        .expect("cached dtvi path を計算できるはず")
        .parent()
        .expect("親ディレクトリがあるはず")
        .to_path_buf()
}

/// このテストファイル用に、`--logo` 無しの実行と同じ条件で使う早期returnの
/// スキップ判定。フィクスチャ（`sample_logo.mp4`/`sample_logo_train.mp4`）と
/// `ffmpeg` が要る（3ツールは偽装するため要らない、モジュール doc comment参照）。
fn skip_if_prerequisites_missing() -> bool {
    common::skip_if_fixture_missing_at(&common::logo_fixture_path())
        || common::skip_if_fixture_missing_at(&common::logo_train_fixture_path())
        || common::skip_if_missing("ffmpeg")
}

/// `dtvindex` / `chapter_exe` / `join_logo_scp` の偽ツール一式を作る。
///
/// - `dtvindex`: `-o` の次の引数へ `dtvi_source` をそのままコピーする（正常系は
///   `common::logo_dtvi_path()`、フレーム数不一致を作るテストは書き換えたコピー
///   を渡す）。
/// - `chapter_exe`: `-o` の次の引数へダミーの `scp.txt` を書く（偽の
///   `join_logo_scp` しか読まないので内容は問わない）。
/// - `join_logo_scp`: 起動時の全引数を1行1引数で `captured_args_path` に記録し
///   （毎回上書き、最後に起動されたときの引数だけが残る）、`-o`/`-oscp` の次の
///   引数へ固定内容を書く。加えて、[`invocation_count_path`] が指す方には
///   毎回1行**追記**する（自動推定で複数回起動されうる、issue #135）。
///
/// 戻り値は `PATH` に前置するビンディレクトリ。
fn setup_fake_tools(tmp_dir: &Path, dtvi_source: &Path, captured_args_path: &Path) -> PathBuf {
    let bin_dir = tmp_dir.join("tools").join("bin");
    fs::create_dir_all(&bin_dir).expect("bin_dir を作れること");

    common::write_executable_script(
        &bin_dir.join("dtvindex"),
        &format!(
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
            dtvi_source.display()
        ),
    );

    common::write_executable_script(
        &bin_dir.join("chapter_exe"),
        "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
    );

    common::write_executable_script(
        &bin_dir.join("join_logo_scp"),
        &format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" > '{captured}'\nprintf '1\\n' >> '{invocations}'\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{trim}' > \"$a\" ;;\n    -oscp) printf '{jls}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
            captured = captured_args_path.display(),
            invocations = invocation_count_path(captured_args_path).display(),
            trim = FAKE_TRIM_CONTENT,
            jls = FAKE_DETAIL_JLS_CONTENT,
        ),
    );

    bin_dir
}

/// [`setup_fake_tools`] の偽 `join_logo_scp` が起動されるたびに1行追記する
/// ファイルのパス（`captured_args_path` から導出、`captured_args_path` 自体は
/// 毎回上書きされ最後の呼び出しの引数しか残らないため、起動回数を別ファイルで
/// 数える）。
fn invocation_count_path(captured_args_path: &Path) -> PathBuf {
    let mut os = captured_args_path.as_os_str().to_os_string();
    os.push(".invocations");
    PathBuf::from(os)
}

/// [`invocation_count_path`] の行数（＝偽 `join_logo_scp` が起動された回数）を返す。
/// ファイルが無ければ0回。
fn join_logo_scp_invocation_count(captured_args_path: &Path) -> usize {
    let path = invocation_count_path(captured_args_path);
    match fs::read_to_string(&path) {
        Ok(content) => content.lines().count(),
        Err(_) => 0,
    }
}

/// `captured_args_path`（偽 `join_logo_scp` が書いた、1行1引数のファイル）を
/// `Vec<String>` として読む。ファイルが無ければ「起動されなかった」ことを表す
/// 空の `Vec` を返す（呼び出し側が `is_file()` で区別したい場合は別に確認する）。
fn read_captured_args(captured_args_path: &Path) -> Vec<String> {
    if !captured_args_path.is_file() {
        return Vec::new();
    }
    fs::read_to_string(captured_args_path)
        .expect("captured args ファイルを読めること")
        .lines()
        .map(str::to_string)
        .collect()
}

/// `--jl-file` に渡すダミーのプレースホルダファイルを作り、そのパスを返す。
/// `tools::default_jl_command_file` の `make install` 配置探索を経由せずに
/// 済ませるため（偽 `join_logo_scp` の実体パスに `share/join_logo_scp/JL/` を
/// 用意する手間を省く）。
fn write_placeholder_jl_file(tmp_dir: &Path) -> PathBuf {
    let path = tmp_dir.join("JL_placeholder.txt");
    fs::write(&path, "placeholder\n").expect("JL プレースホルダを書けること");
    path
}

/// `tachikaze make-logo` を実行し、`.lgd` を作る。`sample_logo_train.mp4`
/// （ロゴを常時合成したクリップ）に対して実行する想定。
fn run_make_logo(input: &Path, rect: &str, output: &Path) {
    let status = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .arg("make-logo")
        .arg(input)
        .arg("--rect")
        .arg(rect)
        .arg("-o")
        .arg(output)
        .status()
        .expect("tachikaze make-logo を起動できるはず");
    assert!(status.success(), "make-logo が失敗しました: {status:?}");
}

/// `tachikaze analyze` を実行する。`logo` が `Some` なら `--logo` を付ける。
/// `no_logo` が `true` なら `--no-logo` を付ける（E18-5 以降、`--logo`/
/// `--no-logo` をどちらも省略すると自動推定が既定で走るため、このテスト
/// ファイルの「ロゴ検出そのものを対象にしない」比較には `--no-logo` で
/// 明示的に無効化する必要がある）。
fn run_analyze(
    bin_dir: &Path,
    cache_dir: &Path,
    input: &Path,
    output: &Path,
    jl_file: &Path,
    logo: Option<&Path>,
    no_logo: bool,
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_tachikaze"));
    cmd.env("PATH", common::prepend_path(bin_dir))
        .arg("--cache-dir")
        .arg(cache_dir)
        .arg("analyze")
        .arg(input)
        .arg("-o")
        .arg(output)
        .arg("--jl-file")
        .arg(jl_file);
    if let Some(logo) = logo {
        cmd.arg("--logo").arg(logo);
    }
    if no_logo {
        cmd.arg("--no-logo");
    }
    cmd.output().expect("tachikaze analyze を起動できるはず")
}

/// このロゴ矩形は `tests/fixtures/gen.sh` の `sample_logo.mp4`/
/// `sample_logo_train.mp4` 生成コマンドと一致させること。
const LOGO_RECT: &str = "616,4,16,16";

/// 完了条件: `--logo` 付きの `analyze` が完走し、キャッシュに logoframe ファイルが
/// できる。`-inlogo` が `-set` 群より前に置かれることも確認する（issue #97「罠」）。
#[test]
#[ignore = "tests/fixtures/sample_logo.mp4・sample_logo_train.mp4 と ffmpeg が必要。\
            tests/fixtures/gen.sh を先に実行すること"]
fn analyze_logo_completes_and_writes_logoframe_when_detection_succeeds() {
    if skip_if_prerequisites_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("happy-path");
    let lgd_path = tmp_dir.join("logo.lgd");
    run_make_logo(&common::logo_train_fixture_path(), LOGO_RECT, &lgd_path);

    let captured_args_path = tmp_dir.join("join_logo_scp_args.txt");
    let bin_dir = setup_fake_tools(&tmp_dir, &common::logo_dtvi_path(), &captured_args_path);
    let jl_file = write_placeholder_jl_file(&tmp_dir);

    let cache_dir = tmp_dir.join("cache");
    let input = common::logo_fixture_path();
    let output = tmp_dir.join("trim.avs");

    let result = run_analyze(
        &bin_dir,
        &cache_dir,
        &input,
        &output,
        &jl_file,
        Some(&lgd_path),
        false,
    );
    assert!(
        result.status.success(),
        "analyze --logo が失敗しました: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("logoframe を書き出しました"),
        "検出成功のログが出るはず: stderr={stderr}"
    );

    let logoframe_path = cache_work_dir(&cache_dir, &input).join("logoframe.txt");
    assert!(
        logoframe_path.is_file(),
        "logoframe ファイルがキャッシュにできているはず: {}",
        logoframe_path.display()
    );
    let logoframe_content =
        fs::read_to_string(&logoframe_path).expect("logoframe ファイルを読めること");
    assert!(
        !logoframe_content.trim().is_empty(),
        "logoframe の内容が空であってはいけない"
    );

    let args = read_captured_args(&captured_args_path);
    assert!(!args.is_empty(), "join_logo_scp が起動されているはず");
    let inlogo_index = args
        .iter()
        .position(|a| a == "-inlogo")
        .expect("-inlogo が渡っているはず");
    assert_eq!(
        args.get(inlogo_index + 1).map(String::as_str),
        Some(logoframe_path.to_str().unwrap()),
        "-inlogo の次の引数は logoframe ファイルのパスのはず"
    );
    let set_index = args
        .iter()
        .position(|a| a == "-set")
        .expect("-set が渡っているはず");
    assert!(
        inlogo_index < set_index,
        "-inlogo は -set 群より前に置くはず（issue #97「罠」）: args={args:?}"
    );
}

/// 完了条件: `--no-logo` を付けた `analyze` は、`join_logo_scp` に渡す引数の形が
/// この issue（#135）の変更前（＝ `--logo` を省略したときの旧来の唯一の挙動）と
/// 同じになる（`-inlogo` を含まない、join_logo_scp は1回だけ起動される）。
/// E18-5 以降、`--logo`/`--no-logo` を両方省略すると自動推定が既定で走るため、
/// 「変更前と同じ挙動」を再現するには `--no-logo` を明示する必要がある。
#[test]
#[ignore = "tests/fixtures/sample_logo.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn analyze_no_logo_passes_unchanged_arguments_to_join_logo_scp() {
    if skip_if_prerequisites_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("no-logo");
    let captured_args_path = tmp_dir.join("join_logo_scp_args.txt");
    let bin_dir = setup_fake_tools(&tmp_dir, &common::logo_dtvi_path(), &captured_args_path);
    let jl_file = write_placeholder_jl_file(&tmp_dir);

    let cache_dir = tmp_dir.join("cache");
    let input = common::logo_fixture_path();
    let output = tmp_dir.join("trim.avs");

    let result = run_analyze(&bin_dir, &cache_dir, &input, &output, &jl_file, None, true);
    assert!(
        result.status.success(),
        "analyze が失敗しました: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    assert_eq!(
        fs::read_to_string(&output).expect("trim.avs を読めること"),
        FAKE_TRIM_CONTENT
    );

    let args = read_captured_args(&captured_args_path);
    assert!(
        !args.contains(&"-inlogo".to_string()),
        "--no-logo のときは -inlogo を渡さないはず: args={args:?}"
    );
    // 引数の形そのもの（`-inscp`/`-incmd`/`-o`/`-oscp`/`-set` の並び）が
    // この issue の変更前と一致することを確認する。`-o`/`-oscp` は
    // `join_logo_scp` 自身の出力先なので、CLI の `-o`（`output`、キャッシュから
    // 明示パスへコピーする「先」）ではなく、キャッシュ内の `trim.avs`/
    // `detail.jls` になる。
    let work_dir = cache_work_dir(&cache_dir, &input);
    assert_eq!(
        args,
        vec![
            "-inscp".to_string(),
            work_dir.join("scp.txt").to_str().unwrap().to_string(),
            "-incmd".to_string(),
            jl_file.to_str().unwrap().to_string(),
            "-o".to_string(),
            work_dir.join("trim.avs").to_str().unwrap().to_string(),
            "-oscp".to_string(),
            work_dir.join("detail.jls").to_str().unwrap().to_string(),
            "-set".to_string(),
            "autocm_sub".to_string(),
            "11".to_string(),
            "-set".to_string(),
            "param_cuttr".to_string(),
            "1".to_string(),
        ]
    );
}

/// 完了条件: 検出フレーム割合が閾値未満のとき `-inlogo` を渡さず、Trim が
/// `--no-logo` と一致する。`sample_logo_train.mp4` で学習した `.lgd`
/// （ロゴの位置 616,4,16,16 前提）を、そのロゴが一切合成されていない
/// `sample.mp4`（既存フィクスチャ、`tests/fixtures/gen.sh`）に適用し、
/// 検出割合を確実に閾値未満にする。
#[test]
#[ignore = "tests/fixtures/sample.mp4・sample_logo_train.mp4 と ffmpeg が必要。\
            tests/fixtures/gen.sh を先に実行すること"]
fn analyze_logo_below_threshold_falls_back_to_no_inlogo_and_matches_omitted_logo() {
    if skip_if_prerequisites_missing() || common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("fallback");
    let lgd_path = tmp_dir.join("logo.lgd");
    run_make_logo(&common::logo_train_fixture_path(), LOGO_RECT, &lgd_path);

    // `sample.mp4` 専用の完全な `.dtvi`（`tests/data/sample_no_logo.dtvi`、
    // `common::no_logo_dtvi_path` の doc comment参照）を偽 dtvindex にコピー
    // させる。`tests/data/sample.dtvi`（40フレームの抜粋、他の多くのテストが
    // 共有するため変更しない）や `sample_logo.dtvi`（別ファイルの `.dtvi`。
    // 実測で判明: 構造的なパラメータが完全に一致していても、ffmpeg の `-ss`
    // シークの着地がファイルごとに微妙にずれることがあり、末尾GOPのフレーム数
    // 検査（blocker3）で不一致になった）はどちらも使えない。
    // フォールバック実行と `--no-logo` 実行の両方に**同じキャッシュディレクトリ・
    // 同じ入力**を使う（`workdir` はキャッシュを実害なく上書きする設計、
    // `src/workdir.rs` の doc comment参照）。こうすることで join_logo_scp に
    // 渡る引数の絶対パスまで含めて一致比較できる（別ディレクトリだと絶対パス
    // 自体が異なり、意味のある比較にならない）。
    let jl_file = write_placeholder_jl_file(&tmp_dir);
    let cache_dir = tmp_dir.join("cache");
    let input = common::fixture_path();

    let captured_args_fallback = tmp_dir.join("join_logo_scp_args_fallback.txt");
    let bin_dir_fallback = setup_fake_tools(
        &tmp_dir,
        &common::no_logo_dtvi_path(),
        &captured_args_fallback,
    );
    let output_fallback = tmp_dir.join("trim-fallback.avs");
    let result = run_analyze(
        &bin_dir_fallback,
        &cache_dir,
        &input,
        &output_fallback,
        &jl_file,
        Some(&lgd_path),
        false,
    );
    assert!(
        result.status.success(),
        "analyze --logo（閾値未満）が失敗しました: stderr={}",
        String::from_utf8_lossy(&result.stderr)
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("閾値未満のため -inlogo を渡しません"),
        "フォールバックしたことが stderr に出るはず: stderr={stderr}"
    );

    let logoframe_path = cache_work_dir(&cache_dir, &input).join("logoframe.txt");
    assert!(
        !logoframe_path.exists(),
        "検出割合が閾値未満のとき logoframe ファイルを書いてはいけない"
    );

    let args_fallback = read_captured_args(&captured_args_fallback);
    assert!(
        !args_fallback.contains(&"-inlogo".to_string()),
        "閾値未満のときは -inlogo を渡さないはず: args={args_fallback:?}"
    );

    // --no-logo と比較する（同じキャッシュディレクトリを再利用する）。
    let captured_args_omitted = tmp_dir.join("join_logo_scp_args_omitted.txt");
    let bin_dir_omitted = setup_fake_tools(&tmp_dir, &common::dtvi_path(), &captured_args_omitted);
    let output_omitted = tmp_dir.join("trim-omitted.avs");
    let result_omitted = run_analyze(
        &bin_dir_omitted,
        &cache_dir,
        &input,
        &output_omitted,
        &jl_file,
        None,
        true,
    );
    assert!(result_omitted.status.success());

    assert_eq!(
        fs::read_to_string(&output_fallback).unwrap(),
        fs::read_to_string(&output_omitted).unwrap(),
        "閾値未満のフォールバック時の Trim は --no-logo と一致するはず"
    );
    assert_eq!(
        read_captured_args(&captured_args_omitted),
        args_fallback,
        "閾値未満のフォールバック時の join_logo_scp 引数は --no-logo と一致するはず"
    );
}

/// 完了条件: `.dtvi` の `frame_count` と読み取ったフレーム数が食い違うと中断する。
/// この中断は `join_logo_scp` を起動する前に効く（issue #97「罠」の主目的、
/// CLAUDE.md 罠3）。
#[test]
#[ignore = "tests/fixtures/sample_logo.mp4・sample_logo_train.mp4 と ffmpeg が必要。\
            tests/fixtures/gen.sh を先に実行すること"]
fn analyze_logo_aborts_before_join_logo_scp_on_frame_count_mismatch() {
    if skip_if_prerequisites_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("frame-count-mismatch");
    let lgd_path = tmp_dir.join("logo.lgd");
    run_make_logo(&common::logo_train_fixture_path(), LOGO_RECT, &lgd_path);

    // 実際の `sample_logo.dtvi`（frame_count=599）を、わざと食い違う値に
    // 書き換えたコピーを用意する。
    let real_dtvi = fs::read_to_string(common::logo_dtvi_path()).expect(".dtvi を読めること");
    assert!(
        real_dtvi.contains("frame_count\t599"),
        "sample_logo.dtvi の frame_count は599のはず（食い違わせる前提が崩れている）"
    );
    let corrupted_dtvi = real_dtvi.replace("frame_count\t599", "frame_count\t500");
    let corrupted_dtvi_path = tmp_dir.join("sample_logo_corrupted.dtvi");
    fs::write(&corrupted_dtvi_path, corrupted_dtvi).expect("書き換えた .dtvi を書けること");

    let captured_args_path = tmp_dir.join("join_logo_scp_args.txt");
    let bin_dir = setup_fake_tools(&tmp_dir, &corrupted_dtvi_path, &captured_args_path);
    let jl_file = write_placeholder_jl_file(&tmp_dir);

    let cache_dir = tmp_dir.join("cache");
    let input = common::logo_fixture_path();
    let output = tmp_dir.join("trim.avs");

    let result = run_analyze(
        &bin_dir,
        &cache_dir,
        &input,
        &output,
        &jl_file,
        Some(&lgd_path),
        false,
    );
    assert!(
        !result.status.success(),
        "frame_count が食い違うので失敗するはず"
    );
    let stderr = String::from_utf8_lossy(&result.stderr);
    assert!(
        stderr.contains("599"),
        "実際のフレーム数(599)が出るはず: stderr={stderr}"
    );
    assert!(
        stderr.contains("500"),
        "食い違わせた frame_count(500)が出るはず: stderr={stderr}"
    );

    assert!(
        !captured_args_path.is_file(),
        "join_logo_scp はフレーム数不一致の後に起動されてはいけない"
    );
    assert!(!output.exists(), "失敗時に trim.avs を書き出してはいけない");
}

/// 完了条件（issue #135）: 自動推定（`--logo`/`--no-logo` を両方省略、既定）が
/// 「ロゴ無し」と判定する入力で、`trim.avs` が `--no-logo` の場合とバイト単位で
/// 一致する。join_logo_scp が1回しか起動されていないことも確認する（issue
/// #135「罠」: 2回目を走らせると「ロゴ無しの現状」と一致しなくなる恐れがある）。
///
/// `sample.mp4`（既存フィクスチャ、599フレーム・キーフレーム約5枚）は
/// `logo::estimate` の候補列生成に必要な有効ブロック数（下限8個、`estimate.rs`
/// の `MIN_VALID_BLOCKS`）に届かないほど短いため、確実に「候補なし→ロゴ無し」
/// に倒れる。ロゴ辞書は空の一時ディレクトリ（`--logo-dir`）を使い、開発者の
/// 実際の辞書（既定 `~/.local/share/tachikaze/logos`）を一切読み書きしない。
#[test]
#[ignore = "tests/fixtures/sample.mp4 と ffmpeg が必要。tests/fixtures/gen.sh を先に実行すること"]
fn analyze_auto_detect_falls_back_to_no_logo_and_matches_no_logo_flag() {
    if common::skip_if_missing("ffmpeg") || common::skip_if_fixture_missing() {
        return;
    }

    let tmp_dir = make_tmp_dir("auto-detect-fallback");
    let jl_file = write_placeholder_jl_file(&tmp_dir);
    let input = common::fixture_path();

    // 自動推定（--logo/--no-logo 両方省略）。
    let captured_args_auto = tmp_dir.join("join_logo_scp_args_auto.txt");
    let bin_dir_auto = setup_fake_tools(&tmp_dir, &common::dtvi_path(), &captured_args_auto);
    let logo_dir_auto = tmp_dir.join("logo-dict-auto");
    let cache_dir_auto = tmp_dir.join("cache-auto");
    let output_auto = tmp_dir.join("trim-auto.avs");
    let result_auto = Command::new(env!("CARGO_BIN_EXE_tachikaze"))
        .env("PATH", common::prepend_path(&bin_dir_auto))
        .arg("--cache-dir")
        .arg(&cache_dir_auto)
        .arg("analyze")
        .arg(&input)
        .arg("-o")
        .arg(&output_auto)
        .arg("--jl-file")
        .arg(&jl_file)
        .arg("--logo-dir")
        .arg(&logo_dir_auto)
        .output()
        .expect("tachikaze analyze を起動できるはず");
    assert!(
        result_auto.status.success(),
        "自動推定の analyze が失敗しました: stderr={}",
        String::from_utf8_lossy(&result_auto.stderr)
    );

    assert_eq!(
        join_logo_scp_invocation_count(&captured_args_auto),
        1,
        "ロゴが見つからない入力では join_logo_scp は1回しか起動されないはず"
    );
    let args_auto = read_captured_args(&captured_args_auto);
    assert!(
        !args_auto.contains(&"-inlogo".to_string()),
        "ロゴが見つからなければ -inlogo を渡さないはず: args={args_auto:?}"
    );

    // --no-logo と比較する（trim.avs の内容自体はキャッシュディレクトリの
    // パスに依存しないため、絶対パスを含む引数の完全一致までは求めない）。
    let captured_args_no_logo = tmp_dir.join("join_logo_scp_args_no_logo.txt");
    let bin_dir_no_logo = setup_fake_tools(&tmp_dir, &common::dtvi_path(), &captured_args_no_logo);
    let cache_dir_no_logo = tmp_dir.join("cache-no-logo");
    let output_no_logo = tmp_dir.join("trim-no-logo.avs");
    let result_no_logo = run_analyze(
        &bin_dir_no_logo,
        &cache_dir_no_logo,
        &input,
        &output_no_logo,
        &jl_file,
        None,
        true,
    );
    assert!(
        result_no_logo.status.success(),
        "--no-logo の analyze が失敗しました: stderr={}",
        String::from_utf8_lossy(&result_no_logo.stderr)
    );
    assert_eq!(
        join_logo_scp_invocation_count(&captured_args_no_logo),
        1,
        "--no-logo でも join_logo_scp は1回しか起動されないはず"
    );

    assert_eq!(
        fs::read_to_string(&output_auto).unwrap(),
        fs::read_to_string(&output_no_logo).unwrap(),
        "自動推定でロゴ無しと判定された結果は --no-logo と一致するはず"
    );

    let _ = fs::remove_dir_all(&tmp_dir);
}
