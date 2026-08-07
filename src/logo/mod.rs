//! Amatsukaze 形式のロゴデータの読み書きと、それを使ったロゴ相関スコアの計算。
//!
//! - [`lgd`][]: `.lgd`（Amatsukaze 形式ロゴデータ）の読み込み
//! - [`score`][]: ロゴマスク生成と相関スコア（`corr0`/`corr1`）の計算
//!
//! 書き込みや他形式（`.lgs` 等）、フレーム供給・区間判定は別 issue（E14-5 以降）
//! でこのファイルに `pub mod` 行が追加される。

pub mod lgd;
pub mod score;
