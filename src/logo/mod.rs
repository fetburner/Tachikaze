//! Amatsukaze 形式のロゴデータの読み書き、フレーム供給、相関スコアの計算。
//!
//! - [`dict`][]: 学習済み `.lgd` を辞書ディレクトリに蓄積し、解像度が一致する
//!   候補をスコアで自動選択する（E18-4）。
//! - [`lgd`][]: `.lgd`（Amatsukaze 形式ロゴデータ）の読み込み。
//! - [`estimate`][]: 入力自身からロゴ矩形の候補列を推定する（E18-2）。
//! - [`frames`][]: ffmpeg を使ってロゴ矩形の輝度平面をフレーム順に読む
//!   （E14-5、`.dtvi` とのフレーム数一致検査を含む）。
//! - [`score`][]: ロゴマスク生成と相関スコア（`corr0`/`corr1`）の計算。
//! - [`scan`][]: `make-logo` サブコマンドの学習アルゴリズムと `.lgd` の書き出し
//!   （E14-6）。
//! - [`interval`][]: `(corr0, corr1)` の列からロゴ表示区間を判定し、
//!   logoframe 形式のテキストを書く。
//!
//! 他形式（`.lgs` 等）は別 issue でこのファイルに `pub mod` 行が追加される。

pub mod dict;
pub mod estimate;
pub mod frames;
pub mod interval;
pub mod lgd;
pub mod scan;
pub mod score;
