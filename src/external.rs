//! 外部プロセス（dtvindex / chapter_exe / join_logo_scp）を起動する共通基盤。
//!
//! いずれのツールも進捗を大量に stdout/stderr に出すため、素通しにせず
//! `std::process::Command::output()` で溜め込んでから扱う。終了コードが
//! 0 以外の場合は、再現に必要な情報（コマンドライン全体・作業ディレクトリ・
//! stderr の末尾）を含めてエラーを返す。

use std::env;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use anyhow::{bail, Context};

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
fn command_line(program: &str, args: &[&str]) -> String {
    let mut line = String::from(program);
    for arg in args {
        let _ = write!(line, " {arg}");
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
/// - `work_dir`: 作業ディレクトリ（カレントディレクトリとして設定する）
///
/// stdout / stderr は逐次転送せず、`Command::output()` により文字列として
/// 丸ごと回収する。終了コードが 0 以外、または起動自体に失敗した場合はエラーを返す。
/// 実行時間は `eprintln!` でログに出す（解析全体で数秒〜十数秒が正常）。
///
/// `program` と `work_dir` が相対パスのとき、呼び出し元のカレントディレクトリを
/// 基準に絶対パスへ直してから起動する。`Command::current_dir` は相対の実行
/// ファイルパスも新しい cwd 基準で解決するため、`--tool-dir tools` のような
/// 相対指定のままだと `work_dir` 配下の `tools/...` を探しにいって
/// `No such file or directory` になる。
pub fn run(program: &str, args: &[&str], work_dir: &Path) -> anyhow::Result<ExternalOutput> {
    let absolute_program = absolutize_program(program)?;
    let absolute_work_dir = absolutize_path(work_dir).with_context(|| {
        format!(
            "作業ディレクトリの絶対パス解決に失敗しました: {}",
            work_dir.display()
        )
    })?;
    let program_display = absolute_program.to_string_lossy();
    let cmdline = command_line(&program_display, args);
    let started = Instant::now();

    let output = Command::new(&absolute_program)
        .args(args)
        .current_dir(&absolute_work_dir)
        .output()
        .with_context(|| {
            format!(
                "外部プロセスの起動に失敗しました: `{cmdline}` (work_dir: {})",
                absolute_work_dir.display()
            )
        })?;

    let elapsed = started.elapsed();
    eprintln!(
        "[external] `{cmdline}` を実行 (work_dir: {}, 所要時間: {:.3}秒)",
        absolute_work_dir.display(),
        elapsed.as_secs_f64()
    );

    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&output.stderr).into_owned();

    if !output.status.success() {
        let stderr_tail = tail_lines(&stderr, 20);
        bail!(
            "外部プロセスが失敗しました: `{cmdline}`\n  work_dir: {}\n  終了コード: {}\n  stderr (末尾20行):\n{}",
            work_dir.display(),
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
fn absolutize_program(program: &str) -> anyhow::Result<PathBuf> {
    let path = Path::new(program);
    if path.is_absolute() || path.components().count() == 1 {
        return Ok(path.to_path_buf());
    }
    absolutize_path(path).with_context(|| {
        format!("実行ファイルの絶対パス解決に失敗しました: {program}")
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// cwd を書き換えるテストを直列化するためのロック。
    static CWD_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn run_captures_stdout() {
        let work_dir = std::env::temp_dir();
        let result = run("/bin/echo", &["hello"], &work_dir).expect("echo should succeed");
        assert_eq!(result.stdout.trim(), "hello");
    }

    #[test]
    fn run_fails_with_nonzero_exit_code() {
        let work_dir = std::env::temp_dir();
        let err = run("/bin/sh", &["-c", "exit 3"], &work_dir)
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
        let work_dir = std::env::temp_dir();
        let err = run("this-binary-does-not-exist-tachikaze", &[], &work_dir)
            .expect_err("missing binary should be an error");
        let message = err.to_string();
        assert!(
            message.contains("this-binary-does-not-exist-tachikaze"),
            "エラーメッセージにプログラム名が含まれていない: {message}"
        );
    }

    #[test]
    fn run_resolves_relative_program_against_caller_cwd_not_work_dir() {
        // `--tool-dir tools` のような相対パスを、`current_dir(work)` の後でも
        // 呼び出し元 cwd 基準で解決できることを確認する。
        let _guard = CWD_LOCK.lock().unwrap();

        let root = std::env::temp_dir().join(format!(
            "tachikaze-external-rel-{}",
            std::process::id()
        ));
        let tool_dir = root.join("tools");
        let work_dir = root.join("work");
        fs::create_dir_all(&tool_dir).unwrap();
        fs::create_dir_all(&work_dir).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let script = tool_dir.join("dummy_tool");
            fs::write(&script, "#!/bin/sh\necho ok-from-relative-tool\n").unwrap();
            let mut perms = fs::metadata(&script).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&script, perms).unwrap();
        }

        let original_cwd = env::current_dir().unwrap();
        env::set_current_dir(&root).unwrap();

        let result = run("tools/dummy_tool", &[], Path::new("work"));

        env::set_current_dir(original_cwd).unwrap();
        let _ = fs::remove_dir_all(&root);

        let output = result.expect("relative tool path should resolve against caller cwd");
        assert_eq!(output.stdout.trim(), "ok-from-relative-tool");
    }
}
