//! 外部プロセス（dtvindex / chapter_exe / join_logo_scp / ffmpeg）を起動する共通基盤。
//!
//! [`run`] は、対象ツールが進捗を大量に stdout/stderr に出すため、素通しにせず
//! `std::process::Command::output()` で溜め込んでから扱う。終了コードが
//! 0 以外の場合は、再現に必要な情報（コマンドライン全体・作業ディレクトリ・
//! stderr の末尾）を含めてエラーを返す。
//!
//! [`spawn_streaming`] は `run` とは別の用途（E14-5、ロゴ矩形の輝度平面を ffmpeg の
//! rawvideo 出力から読む）のために追加した。stdout をまるごと溜め込む `run` とは
//! 異なり、stdout を呼び出し側へ逐次渡す（大きな rawvideo 出力を全部メモリに
//! 溜めないため）。終了コード・stderr の扱いは `run` と同じ流儀に揃える。

use std::env;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::thread::{self, JoinHandle};
use std::time::Instant;

use anyhow::{bail, Context};

use crate::errctx::PathContext;

/// 起動に成功した外部プロセスの実行結果。
///
/// 終了コードが 0 の場合のみこの型が返る（0 以外は `run` がエラーにする）。
#[derive(Debug, Clone)]
pub struct ExternalOutput {
    pub stdout: String,
    pub stderr: String,
}

/// エラーメッセージ用に、シェルにそのまま貼って再現できる形のコマンドラインを作る。
///
/// 空白や特殊文字を含む引数は単純に無視し、スペース区切りで連結するだけの簡易実装。
/// 表示目的のみでシェルへの実際の入力には使わない。
fn command_line(program: &Path, args: &[&OsStr]) -> String {
    let mut line = program.to_string_lossy().into_owned();
    for arg in args {
        let _ = write!(line, " {}", arg.to_string_lossy());
    }
    line
}

/// stderr の末尾 `max_lines` 行を返す（エラーメッセージに含めるため）。
fn tail_lines(text: &str, max_lines: usize) -> String {
    let lines: Vec<&str> = text.lines().collect();
    let start = lines.len().saturating_sub(max_lines);
    lines[start..].join("\n")
}

/// 外部プロセスを1つ起動し、完了まで待つ。
///
/// - `program`: 実行ファイル（PATH 解決に従う。絶対パスも可）
/// - `args`: コマンドライン引数
/// - `cwd`: 作業ディレクトリ（カレントディレクトリとして設定する）
///
/// stdout / stderr は逐次転送せず、`Command::output()` により文字列として
/// 丸ごと回収する。終了コードが 0 以外、または起動自体に失敗した場合はエラーを返す。
/// 実行時間は `eprintln!` でログに出す（解析全体で数秒〜十数秒が正常）。
///
/// `program` と `cwd` が相対パスのとき、呼び出し元のカレントディレクトリを
/// 基準に絶対パスへ直してから起動する。`Command::current_dir` は相対の実行
/// ファイルパスも新しい cwd 基準で解決するため、`PATH` に `tools` のような
/// 相対エントリが含まれる場合、そのまま起動すると `cwd` 配下の
/// `tools/...` を探しにいって `No such file or directory` になる。
pub fn run(program: &Path, args: &[&OsStr], cwd: &Path) -> anyhow::Result<ExternalOutput> {
    let absolute_program = absolutize_program(program)?;
    let absolute_cwd = absolutize_path(cwd).path_ctx("作業ディレクトリの絶対パス解決", cwd)?;
    let cmdline = command_line(&absolute_program, args);
    let started = Instant::now();

    let output = Command::new(&absolute_program)
        .args(args)
        .current_dir(&absolute_cwd)
        .output()
        .with_context(|| {
            format!(
                "外部プロセスの起動に失敗しました: `{cmdline}` (cwd: {})",
                absolute_cwd.display()
            )
        })?;

    let elapsed = started.elapsed();
    eprintln!(
        "[external] `{cmdline}` を実行 (cwd: {}, 所要時間: {:.3}秒)",
        absolute_cwd.display(),
        elapsed.as_secs_f64()
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let stderr_tail = tail_lines(&stderr, 20);
        bail!(
            "外部プロセスが失敗しました: `{cmdline}`\n  cwd: {}\n  終了コード: {}\n  stderr (末尾20行):\n{}",
            cwd.display(),
            output
                .status
                .code()
                .map(|c| c.to_string())
                .unwrap_or_else(|| "不明（シグナル終了）".to_string()),
            stderr_tail,
        );
    }

    Ok(ExternalOutput { stdout, stderr })
}

/// [`spawn_streaming`] が返す、起動済みだが完了は待っていない外部プロセス。
///
/// stdout は呼び出し側が [`Self::stdout`] で受け取って好きなだけ読み進める。
/// 読み終えたら必ず [`Self::wait`] を呼び、終了コードを確認すること
/// （呼ばないと子プロセスが reap されず残る）。
pub struct StreamingChild {
    child: Child,
    cmdline: String,
    cwd: PathBuf,
    stdout: ChildStdout,
    /// 子プロセスの stderr を読み切るスレッド。呼び出し側が stdout を逐次読んでいる
    /// 間、stderr パイプを溜めたままにするとデッドロックしうるため（下記
    /// `spawn_streaming` の doc comment参照）、起動直後に読み切りを始めておく。
    stderr_reader: JoinHandle<String>,
}

impl StreamingChild {
    /// 子プロセスの標準出力へのハンドル。
    pub fn stdout(&mut self) -> &mut ChildStdout {
        &mut self.stdout
    }

    /// 子プロセスの終了を待つ。終了コードが 0 以外なら、`run` と同じ形式
    /// （コマンドライン・cwd・終了コード・stderr の末尾20行）でエラーを返す。
    pub fn wait(mut self) -> anyhow::Result<()> {
        let status = self
            .child
            .wait()
            .with_context(|| format!("外部プロセスの終了待ちに失敗しました: `{}`", self.cmdline))?;

        let stderr = self.stderr_reader.join().unwrap_or_default();

        if !status.success() {
            let stderr_tail = tail_lines(&stderr, 20);
            bail!(
                "外部プロセスが失敗しました: `{}`\n  cwd: {}\n  終了コード: {}\n  stderr (末尾20行):\n{}",
                self.cmdline,
                self.cwd.display(),
                status
                    .code()
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "不明（シグナル終了）".to_string()),
                stderr_tail,
            );
        }
        Ok(())
    }
}

/// 外部プロセスを1つ起動し、完了を待たずに返す（stdout を逐次読みたい呼び出し用）。
///
/// `run` との違いは stdout の扱いのみ。`program`/`args`/`cwd` の解釈（絶対パス化、
/// PATH 解決）は `run` と同じ。
///
/// **デッドロック回避**: 呼び出し側が stdout を逐次読んでいる間、子プロセスの
/// stderr パイプが OS のバッファ上限まで溜まると、子プロセスは stderr への書き込みで
/// ブロックし、stdout の生成も止まる（呼び出し側は stdout を待ち続けているので
/// 双方が止まる）。`run` はこれを `Command::output()` に任せている（内部で stdout/
/// stderr を並行に読む）が、ここでは呼び出し側が stdout を手動で読むため、自分で
/// 対策する必要がある。起動直後に専用スレッドを立てて stderr を最後まで読み切る
/// ことで、パイプが詰まらないようにする。
pub fn spawn_streaming(
    program: &Path,
    args: &[&OsStr],
    cwd: &Path,
) -> anyhow::Result<StreamingChild> {
    let absolute_program = absolutize_program(program)?;
    let absolute_cwd = absolutize_path(cwd).path_ctx("作業ディレクトリの絶対パス解決", cwd)?;
    let cmdline = command_line(&absolute_program, args);

    let mut child = Command::new(&absolute_program)
        .args(args)
        .current_dir(&absolute_cwd)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "外部プロセスの起動に失敗しました: `{cmdline}` (cwd: {})",
                absolute_cwd.display()
            )
        })?;

    let stdout = child
        .stdout
        .take()
        .expect("stdout は Stdio::piped() で設定済み");
    let mut stderr = child
        .stderr
        .take()
        .expect("stderr は Stdio::piped() で設定済み");
    let stderr_reader = thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });

    Ok(StreamingChild {
        child,
        cmdline,
        cwd: absolute_cwd,
        stdout,
        stderr_reader,
    })
}

/// 呼び出し元 cwd 基準で `path` を絶対パスにする。
///
/// 存在するパスなら `canonicalize` で symlink を解消する。まだ存在しない
/// パス（これから作る作業ディレクトリ等）は、cwd を join するだけに留める。
fn absolutize_path(path: &Path) -> anyhow::Result<PathBuf> {
    if path.is_absolute() {
        match path.canonicalize() {
            Ok(canonical) => Ok(canonical),
            Err(_) => Ok(path.to_path_buf()),
        }
    } else {
        let joined = env::current_dir()
            .context("カレントディレクトリの取得に失敗しました")?
            .join(path);
        match joined.canonicalize() {
            Ok(canonical) => Ok(canonical),
            Err(_) => Ok(joined),
        }
    }
}

/// 実行ファイルパスを絶対パスにする。
///
/// パス区切りを含まない名前（`ffprobe` など）は PATH 解決に任せるため、
/// そのまま返す。相対パス（`tools/dtvindex`）だけを絶対化する。
fn absolutize_program(program: &Path) -> anyhow::Result<PathBuf> {
    if program.is_absolute() || program.components().count() == 1 {
        return Ok(program.to_path_buf());
    }
    absolutize_path(program).path_ctx("実行ファイルの絶対パス解決", program)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// cwd を書き換えるテストを直列化するためのロック。
    ///
    /// **このロックは書き換える側どうししか直列化できない。** cwd はプロセス全体で
    /// 共有されるため、ロックを取らずに相対パスでファイルを開くテストが並行して
    /// 走っていると、そちらが巻き添えで失敗する。実際に
    /// `mp4io::order_map::tests` がフィクスチャを相対パス
    /// `tests/fixtures/sample.mp4` で開いていて、この下の
    /// `run_resolves_relative_program_against_caller_cwd_not_new_cwd` と同時に
    /// 走ったときだけ「No such file or directory」で落ちた。しかも
    /// `skip_if_fixture_missing` 系のヘルパは存在確認も相対パスで行うため、
    /// **落ちずに無言でスキップして緑になる**経路もあった。
    ///
    /// 対処として `src/` 側のフィクスチャ参照はすべて
    /// `concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/...")` の絶対パスに
    /// 統一した（`tests/*.rs` の E2E は元から同じ方式）。**フィクスチャを相対パスで
    /// 参照するテストを新しく足さないこと。**
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn run_captures_stdout() {
        let cwd = std::env::temp_dir();
        let result =
            run(Path::new("/bin/echo"), &[OsStr::new("hello")], &cwd).expect("echo should succeed");
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[test]
    fn run_fails_with_nonzero_exit_code() {
        let cwd = std::env::temp_dir();
        let err = run(
            Path::new("/bin/sh"),
            &[OsStr::new("-c"), OsStr::new("exit 3")],
            &cwd,
        )
        .expect_err("exit code 3 should be an error");
        let message = err.to_string();
        assert!(
            message.contains("終了コード: 3") || message.contains("3"),
            "エラーメッセージに終了コードが含まれていない: {message}"
        );
        assert!(
            message.contains("/bin/sh -c exit 3"),
            "エラーメッセージにコマンドラインが含まれていない: {message}"
        );
    }

    #[test]
    fn run_reports_missing_program_clearly() {
        let cwd = std::env::temp_dir();
        let err = run(Path::new("this-binary-does-not-exist-tachikaze"), &[], &cwd)
            .expect_err("missing binary should be an error");
        let message = err.to_string();
        assert!(
            message.contains("this-binary-does-not-exist-tachikaze"),
            "エラーメッセージにプログラム名が含まれていない: {message}"
        );
    }

    #[test]
    fn run_resolves_relative_program_against_caller_cwd_not_new_cwd() {
        // `tools/dummy_tool` のような相対パスを、`current_dir(work)` の後でも
        // 呼び出し元 cwd 基準で解決できることを確認する。
        let _guard = CWD_LOCK.lock().unwrap();

        let root =
            std::env::temp_dir().join(format!("tachikaze-external-rel-{}", std::process::id()));
        let relative_bin_dir = root.join("tools");
        let cwd = root.join("work");
        fs::create_dir_all(&relative_bin_dir).unwrap();
        fs::create_dir_all(&cwd).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = relative_bin_dir.join("dummy_tool");
            fs::write(&script, "#!/bin/sh\necho ok-from-relative-tool\n").unwrap();
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }

        let original_cwd = env::current_dir().unwrap();
        env::set_current_dir(&root).unwrap();

        let result = run(Path::new("tools/dummy_tool"), &[], Path::new("work"));

        env::set_current_dir(original_cwd).unwrap();
        let _ = fs::remove_dir_all(&root);

        let output = result.expect("relative tool path should resolve against caller cwd");
        assert_eq!(output.stdout.trim(), "ok-from-relative-tool");
    }
}
