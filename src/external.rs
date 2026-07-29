//! 外部プロセス（dtvindex / chapter_exe / join_logo_scp）を起動する共通基盤。
//!
//! いずれのツールも進捗を大量に stdout/stderr に出すため、素通しにせず
//! `std::process::Command::output()` で溜め込んでから扱う。終了コードが
//! 0 以外の場合は、再現に必要な情報（コマンドライン全体・作業ディレクトリ・
//! stderr の末尾）を含めてエラーを返す。

use std::fmt::Write as _;
use std::path::Path;
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
pub fn run(program: &str, args: &[&str], work_dir: &Path) -> anyhow::Result<ExternalOutput> {
    let cmdline = command_line(program, args);
    let started = Instant::now();

    let output = Command::new(program)
        .args(args)
        .current_dir(work_dir)
        .output()
        .with_context(|| {
            format!(
                "外部プロセスの起動に失敗しました: `{cmdline}` (work_dir: {})",
                work_dir.display()
            )
        })?;

    let elapsed = started.elapsed();
    eprintln!(
        "[external] `{cmdline}` を実行 (work_dir: {}, 所要時間: {:.3}秒)",
        work_dir.display(),
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
