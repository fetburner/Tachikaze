//! `tachikaze` バイナリのエントリポイント。
//!
//! 引数をパースして [`tachikaze::commands::run`] に渡すだけに保つ。処理の中身は
//! すべてライブラリ側（`src/lib.rs` 以下）にある。理由は crate ルートのドキュメント参照。

use clap::Parser;

use tachikaze::cli::Cli;
use tachikaze::commands::{self, ExitOutcome};

/// exit code は 0=完了 / 1=エラー / 2=判定で停止（`auto` の gate が疑わしいと判定して
/// 停止した場合のみ）の3種類（`commands::ExitOutcome` の doc comment参照）。
///
/// `analyze` / `cut` / `prepare` / `remap-subs` は常に `Ok(ExitOutcome::Success)` か
/// `Err`（`?` で下の `main` から抜け、Rust ランタイムが `Error: {err:?}` を出して
/// exit code 1 にする、従来と同じ挙動）のどちらかしか返さないため、これらの
/// サブコマンドの CLI 挙動はこの変更で一切変わらない。
fn main() -> anyhow::Result<()> {
    match commands::run(Cli::parse())? {
        ExitOutcome::Success => Ok(()),
        ExitOutcome::GateStopped => std::process::exit(2),
    }
}
