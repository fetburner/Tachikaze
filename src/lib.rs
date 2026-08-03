//! Tachikaze: mp4 に変換済みの録画ファイルを、再エンコードせずに CM カットする。
//!
//! CM 検出は既存ツール（chapter_exe → join_logo_scp）に任せ、本クレートは
//! 「Trim リスト → ロスレス出力」だけを担う。
//!
//! # なぜライブラリターゲットがあるのか
//!
//! 当初はバイナリのみのクレートだったが、次の 2 点のためライブラリを分けている。
//!
//! 1. **`compile_fail` の doc test が実行されない。** バイナリクレートでは
//!    doctest が収集されないため、[`order`] の「`DisplayIdx` と `DecodeIdx` を
//!    取り違えるとコンパイルエラーになる」ことを検証する doctest が一度も
//!    走らない状態になっていた。表示順とデコード順の混同はこのプロジェクト
//!    唯一の重大バグ源なので、その防御が効いていることは実際に検証したい。
//! 2. **統合テストから `src` の関数を直接呼べない。** `tests/` は別クレート
//!    扱いなので、ライブラリが無いとテストは CLI バイナリを起動するか
//!    `#[path]` で `src` のファイルを取り込むしかない。前者は CLI の
//!    オプション変更に追随し損なう脆さがあり、後者はコードが二重にコンパイル
//!    される。
//!
//! バイナリ側（`src/main.rs`）は引数をパースして [`commands::run`] を呼ぶだけの
//! 薄いエントリポイントに保つ。

pub mod analyze;
pub mod audio;
pub mod auto;
pub mod cli;
pub mod commands;
pub mod dtvi;
pub(crate) mod errctx;
pub mod external;
pub mod ffprobe;
pub mod gate;
pub mod jls;
pub mod mp4io;
pub mod order;
pub mod plan;
pub mod prepare;
pub mod report;
pub mod segmap;
pub mod subtitle;
pub mod tools;
pub mod trim;
pub mod verify;
pub mod workdir;

#[cfg(test)]
mod tests {
    use mp4_atom::{Encode, Ftyp};

    /// 依存クレート `mp4-atom` が実際に使えることの確認（#11 の完了条件）。
    #[test]
    fn ftyp_encodes() -> std::result::Result<(), Box<dyn std::error::Error>> {
        let ftyp = Ftyp {
            major_brand: b"isom".into(),
            minor_version: 512,
            compatible_brands: vec![
                b"isom".into(),
                b"iso2".into(),
                b"avc1".into(),
                b"mp41".into(),
            ],
        };

        let mut buf = Vec::new();
        ftyp.encode(&mut buf)?;

        assert!(!buf.is_empty());
        Ok(())
    }
}
