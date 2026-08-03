//! `tachikaze` バイナリのエントリポイント。
//!
//! 引数をパースして [`tachikaze::commands::run`] に渡すだけに保つ。処理の中身は
//! すべてライブラリ側（`src/lib.rs` 以下）にある。理由は crate ルートのドキュメント参照。

use clap::Parser;

use tachikaze::cli::Cli;
use tachikaze::commands::{self, ExitOutcome};

/// Rust ランタイムは起動時に `SIGPIPE` を `SIG_IGN` にする（`write` が `EPIPE` を
/// 返すだけになり、パイプの片方が落ちたことがプロセス終了に直結しない設計）。
/// これを戻さないと、`tachikaze analyze IN.mp4 -o - | head -1` のように途中で
/// パイプが閉じたとき、標準出力への書き込みが `EPIPE` エラーとなり、それを
/// 包む `std::io::Write` の呼び出しが panic する（CLAUDE.md の罠、`-o -` の
/// 追加とセットで直す必要がある）。UNIX の一般的な CLI と同じ挙動
/// （SIGPIPE で黙って終了する）に戻すため、起動直後に `SIG_DFL` へ戻す。
///
/// `libc` クレートを依存に足さず `extern "C"` で `signal(2)` を直接呼ぶ
/// （このリポジトリは FNV ハッシュを自前実装するなど、既に標準ライブラリに
/// 依存を持たない方針を採ってきた。`SIGPIPE`（13）は macOS / Linux の両方で
/// 同じ値であり、`docs/tech-stack.md` の前提どおり開発・実行は macOS arm64
/// に絞っているため、`libc::SIGPIPE` 相当の値をここで直接書いても問題ない）。
#[cfg(unix)]
fn reset_sigpipe_to_default() {
    const SIGPIPE: i32 = 13;
    const SIG_DFL: usize = 0;

    extern "C" {
        fn signal(signum: i32, handler: usize) -> usize;
    }

    unsafe {
        signal(SIGPIPE, SIG_DFL);
    }
}

/// exit code は 0=完了 / 1=エラー / 2=引数の誤り（clap の既定。`Cli::parse()` が
/// 自前で `std::process::exit` するため、この関数の中には出てこない） /
/// 3=判定で停止（`auto` の gate が疑わしいと判定して停止した場合のみ）の4種類
/// （`commands::ExitOutcome` の doc comment参照）。
///
/// **なぜ 2 ではなく 3 なのか**: clap は usage error（未知のオプション・引数の
/// 数の不一致など）を exit code 2 で終了させる（実測: `tachikaze --bogus` →
/// exit 2）。この 2 と衝突しない最小の番号が 3 なので、gate 停止には 3 を
/// 割り当てている。`std::process::exit(2)` を自前で使わないこと（clap の
/// usage error と区別できなくなる）。
///
/// `analyze` / `cut` / `prepare` / `remap-subs` は常に `Ok(ExitOutcome::Success)` か
/// `Err`（`?` で下の `main` から抜け、Rust ランタイムが `Error: {err:?}` を出して
/// exit code 1 にする、従来と同じ挙動）のどちらかしか返さないため、これらの
/// サブコマンドの CLI 挙動はこの変更で一切変わらない。
fn main() -> anyhow::Result<()> {
    #[cfg(unix)]
    reset_sigpipe_to_default();

    match commands::run(Cli::parse())? {
        ExitOutcome::Success => Ok(()),
        ExitOutcome::GateStopped => {
            eprintln!("[auto] exit code 3 で停止しました。");
            std::process::exit(3)
        }
    }
}
