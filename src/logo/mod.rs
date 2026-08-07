//! Amatsukaze 形式のロゴデータの読み書き、フレーム供給、相関スコアの計算。
//!
//! - [`lgd`][]: `.lgd`（Amatsukaze 形式ロゴデータ）の読み込み。
//! - [`frames`][]: ffmpeg を使ってロゴ矩形の輝度平面をフレーム順に読む
//!   （E14-5、`.dtvi` とのフレーム数一致検査を含む）。
//! - [`score`][]: ロゴマスク生成と相関スコア（`corr0`/`corr1`）の計算。
//! - [`scan`][]: `make-logo` サブコマンドの学習アルゴリズムと `.lgd` の書き出し
//!   （E14-6）。
//!
//! 他形式（`.lgs` 等）や区間判定は別 issue でこのファイルに `pub mod` 行が
//! 追加される。

pub mod frames;
pub mod lgd;
pub mod scan;
pub mod score;
