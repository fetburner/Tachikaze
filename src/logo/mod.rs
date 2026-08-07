//! Amatsukaze 形式のロゴデータの読み書きと、ロゴ検出に使うフレーム供給。
//!
//! - [`lgd`]: `.lgd`（Amatsukaze 形式ロゴデータ）の読み込み。
//! - [`frames`]: ffmpeg を使ってロゴ矩形の輝度平面をフレーム順に読む
//!   （E14-5、`.dtvi` とのフレーム数一致検査を含む）。
//!
//! 書き込みや他形式（`.lgs` 等）、実際のロゴ検出アルゴリズムは別 issue でこの
//! ファイルに `pub mod` 行が追加される。

pub mod frames;
pub mod lgd;
