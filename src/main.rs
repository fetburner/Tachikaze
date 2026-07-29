//! `tachikaze` バイナリのエントリポイント。
//!
//! 引数をパースして [`tachikaze::commands::run`] に渡すだけに保つ。処理の中身は
//! すべてライブラリ側（`src/lib.rs` 以下）にある。理由は crate ルートのドキュメント参照。

use clap::Parser;

use tachikaze::cli::Cli;
use tachikaze::commands;

fn main() -> anyhow::Result<()> {
    commands::run(Cli::parse())
}
