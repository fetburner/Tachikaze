//! Amatsukaze 形式のロゴデータの読み書き、フレーム供給、相関スコアの計算。
//!
//! - [`lgd`][]: `.lgd`（Amatsukaze 形式ロゴデータ）の読み込み。
//! - [`frames`][]: ffmpeg を使ってロゴ矩形の輝度平面をフレーム順に読む
//!   （E14-5、`.dtvi` とのフレーム数一致検査を含む）。
//! - [`score`][]: ロゴマスク生成と相関スコア（`corr0`/`corr1`）の計算。
//! - [`interval`][]: `(corr0, corr1)` の列からロゴ表示区間を判定し、
//!   logoframe 形式のテキストを書く。
//!
//! 書き込み（`make-logo`）は別 issue でこのファイルに `pub mod` 行が追加される。

pub mod frames;
pub mod interval;
pub mod lgd;
pub mod score;
