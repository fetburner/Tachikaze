//! `Result` に「<操作>に失敗しました: <パス>」の文脈を1行で付けるための拡張。
//!
//! `anyhow` の `.with_context(|| format!("...に失敗しました: {}", path.display()))`
//! という同じ形が `src/` 全体に散っており、rustfmt が毎回5〜6行に展開するため、
//! 文脈付与だけで数百行を占めていた。文言の体裁（「〜に失敗しました: <パス>」）を
//! この1か所に固定することで、書き手ごとの揺れも防ぐ。
//!
//! 適用できるのは「操作の説明が静的な文言」かつ「埋め込む値がパス1個だけ」の場合に
//! 限る。パスが2個必要なもの（rename の from/to）、固定文言が前後に付くもの、
//! 埋め込む値がパスでないものは、素の `with_context` のまま残してある
//! （無理に通そうとするとエラー文言が変わるため）。
//!
//! `impl<T, E> ... for Result<T, E> where Result<T, E>: anyhow::Context<T, E>` という
//! 形にしているのは、`impl<T, E, R> ... for R where R: anyhow::Context<T, E>` だと
//! `E` が self 型に現れず E0207（未制約の型パラメータ）になるため。この形なら
//! `Result<T, io::Error>` と `anyhow::Result<T>` の両方に一度で効く
//! （`anyhow` は `Context` を `E: ext::StdError` に対して実装しており、
//! `ext::StdError` は `anyhow::Error` にも実装されている）。

use std::path::Path;

use anyhow::Context as _;

pub(crate) trait PathContext<T> {
    /// `what` に「に失敗しました: <path>」を付けた文脈を付与する。
    fn path_ctx(self, what: &str, path: &Path) -> anyhow::Result<T>;
}

impl<T, E> PathContext<T> for Result<T, E>
where
    Result<T, E>: anyhow::Context<T, E>,
{
    fn path_ctx(self, what: &str, path: &Path) -> anyhow::Result<T> {
        self.with_context(|| format!("{what}に失敗しました: {}", path.display()))
    }
}
