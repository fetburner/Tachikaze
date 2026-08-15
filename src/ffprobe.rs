//! ffprobe を起動して CSV 出力を読む処理の共通実装。
//!
//! CLAUDE.md の罠2（無劣化の検証に md5 を使わず、ffprobe の `-show_data_hash CRC32`
//! でパケット単位に比較する）に関わる `-v error -select_streams X -show_entries Y
//! -show_data_hash CRC32 -of csv=p=0` という CRC32 クエリの引数列は、無劣化検証を
//! 行う複数の呼び出し元にほぼ同じ形でコピーされていた。呼び出しごとに微妙に
//! 引数の順序や有無が変わると罠2の実効性が静かに損なわれるため、
//! `-show_data_hash` を付ける組み立ては [`csv_rows`] の1か所に集約している
//! （`args.push("-show_data_hash")` を呼ぶのはリポジトリ全体でここだけ）。
//!
//! ただし `-show_data_hash` を伴わない一般の CSV クエリ（例: `frame=pts`）まで
//! すべてここに集約したわけではない。罠2に関わらないため、`tests/` 側の手書きの
//! 呼び出しがそのまま残っている箇所がある。

use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// `csv_rows` が実際に `ffprobe` へ渡す引数列を組み立てる（`-of csv=p=0` 系）。
///
/// `-show_data_hash CRC32` を付けるかどうかは、独立した `bool` 引数ではなく
/// `entries` に `"data_hash"` が含まれるかどうかから導出する。
///
/// # なぜ独立した `bool` 引数にしないか
///
/// 以前は `csv_rows(ffprobe, target, stream, entries, data_hash: bool)` のように
/// `entries` と `data_hash` が別々の引数だった。呼び出し側が `entries` に
/// `"data_hash"` を含めつつ `data_hash: false` を渡す（またはその逆）というズレを
/// 起こしても、コンパイラは検出できない。
///
/// ## 実測: `-show_data_hash CRC32` を落として `entries=packet=data_hash` だけを渡すと何が起きるか
///
/// ```console
/// $ ffprobe -v error -select_streams a:0 -show_entries packet=data_hash -of csv=p=0 tests/fixtures/sample.mp4 > out.txt
/// exit=0   バイト数=1000   改行数=1000   非空行数=0   stderr=0バイト
/// 先頭5バイト: \n \n \n \n \n   （すべて改行のみ）
/// 同じファイルの音声パケット総数=1000
/// ```
///
/// ffprobe は終了コード0・stderr 空のまま、**音声パケット1つにつき空行1本**を
/// 返す（1000パケットで1000行。「出力0行」ではない）。`csv_rows` は各行を trim
/// して空行を除くため、この1000行はすべて畳まれて最終的に `Ok(vec![])` になる
/// （結論自体は変わらないが、途中で何行返ってきているかは別の観測値）。
///
/// ## 構造的な危険（実際に起きたわけではない）
///
/// `entries` の文字列と `data_hash: bool` が独立した引数だったことは、上の
/// ズレをコンパイラが検出できない構造だった。もしズレていれば `csv_rows` の
/// 戻り値が空になり、`verify_audio_packets_with_ffprobe` の集合比較
/// （`HashSet::difference` が空集合同士）も dts の単調増加チェック
/// （`windows(2)` が空列に対して常に真）も両方素通りするため、検証が
/// エラーを出さずに黙って無効化されていたはずだった。
///
/// **ただし** この修正の時点で `csv_rows` を呼んでいた7箇所はすべて `entries` と
/// `data_hash` が正しく揃っており、このズレが実際に起きたことは無い。
/// `entries` から導出する形にしたのは、起きた事故を直すためではなく、この危険を
/// 型で表現不可能にするため。
fn build_csv_args<'a>(stream: &'a str, entries: &'a str) -> Vec<&'a str> {
    let mut args: Vec<&str> = vec![
        "-v",
        "error",
        "-select_streams",
        stream,
        "-show_entries",
        entries,
    ];
    if entries.contains("data_hash") {
        args.push("-show_data_hash");
        args.push("CRC32");
    }
    args.push("-of");
    args.push("csv=p=0");
    args
}

/// `ffprobe` を1回起動し、`target` に対して `-v error -select_streams stream
/// -show_entries entries [-show_data_hash CRC32] -of csv=p=0` を実行して、空行を
/// 除いた行の列を返す。
///
/// - `stream` はストリーム指定（`"v:0"` / `"a:0"`）。
/// - `entries` は `-show_entries` にそのまま渡す値（例: `"packet=dts"`、
///   `"packet=data_hash"`）。`entries` に `"data_hash"` が含まれるときだけ
///   `-show_data_hash CRC32` を付ける（`build_csv_args` の doc comment参照。
///   独立した `bool` 引数にしない理由もそちらに書いてある）。
///
/// 行の順序は ffprobe の格納順（映像はデコード順、音声もコンテナ格納順）のまま返す。
/// 集合として使いたい呼び出し元は自分で `HashSet` に集める（順序・重複を保つ必要が
/// ある呼び出し元と、集合比較で十分な呼び出し元の両方があるため、ここでは畳み込まない）。
pub fn csv_rows(
    ffprobe: &Path,
    target: &Path,
    stream: &str,
    entries: &str,
) -> anyhow::Result<Vec<String>> {
    let args = build_csv_args(stream, entries);
    let text = run(ffprobe, target, &args)?;
    Ok(text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(str::to_string)
        .collect())
}

/// `ffprobe` を起動して標準出力を文字列で返す。終了コードが 0 以外、または起動自体に
/// 失敗した場合はエラーを返す。
fn run(ffprobe: &Path, target: &Path, args: &[&str]) -> anyhow::Result<String> {
    let output = Command::new(ffprobe)
        .args(args)
        .arg(target)
        .output()
        .with_context(|| {
            format!(
                "ffprobe({}) の起動に失敗しました(対象: {})",
                ffprobe.display(),
                target.display()
            )
        })?;
    anyhow::ensure!(
        output.status.success(),
        "ffprobe({}) が失敗しました(対象: {}): {}",
        ffprobe.display(),
        target.display(),
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).context("ffprobe の出力が utf-8 ではありません")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_csv_args_includes_show_data_hash_when_entries_has_data_hash() {
        let args = build_csv_args("a:0", "packet=data_hash");
        assert!(args.contains(&"-show_data_hash"), "args={args:?}");

        // 順序: -v error -> -select_streams -> -show_entries -> -show_data_hash CRC32
        // -> -of csv=p=0
        assert_eq!(
            args,
            vec![
                "-v",
                "error",
                "-select_streams",
                "a:0",
                "-show_entries",
                "packet=data_hash",
                "-show_data_hash",
                "CRC32",
                "-of",
                "csv=p=0",
            ]
        );
    }

    #[test]
    fn build_csv_args_omits_show_data_hash_when_entries_has_no_data_hash() {
        let args = build_csv_args("v:0", "packet=dts");
        assert!(!args.contains(&"-show_data_hash"), "args={args:?}");
        assert_eq!(
            args,
            vec![
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=dts",
                "-of",
                "csv=p=0",
            ]
        );
    }

    #[test]
    fn build_csv_args_includes_show_data_hash_when_entries_combines_size_and_data_hash() {
        // src/verify.rs::video_packet_crc32_in_decode_order と src/mp4io/write.rs が
        // 実際に使う形 (`"packet=size,data_hash"`)。部分文字列判定で正しく検出できる
        // ことを確認する。
        let args = build_csv_args("v:0", "packet=size,data_hash");
        assert_eq!(
            args,
            vec![
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "packet=size,data_hash",
                "-show_data_hash",
                "CRC32",
                "-of",
                "csv=p=0",
            ]
        );
    }
}
