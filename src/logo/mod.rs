//! Amatsukaze 形式のロゴデータの読み書きと、ロゴ検出に使うフレーム供給。
//!
//! - [`lgd`]: `.lgd`（Amatsukaze 形式ロゴデータ）の読み込み。
//! - [`frames`]: ffmpeg を使ってロゴ矩形の輝度平面をフレーム順に読む
//!   （E14-5、`.dtvi` とのフレーム数一致検査を含む）。
//! - [`scan`]: `make-logo` サブコマンドの学習アルゴリズムと `.lgd` の書き出し
//!   （E14-6）。
//!
//! 他形式（`.lgs` 等）や、`.lgd` を使った実際のロゴ検出（スコア計算）は別 issue で
//! このファイルに `pub mod` 行が追加される。

pub mod frames;
pub mod lgd;
pub mod scan;
