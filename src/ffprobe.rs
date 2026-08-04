//! ffprobe を起動して CSV 出力を読む処理の共通実装。
//!
//! `-v error -select_streams X -show_entries Y [-show_data_hash CRC32] -of csv=p=0`
//! という引数列は、`src/verify.rs` の実装（映像/音声パケットの検証）と
//! `tests/audio_e2e.rs` のテスト側オラクルの双方に、合計8個以上ほぼ同じ形で
//! コピーされていた。呼び出しごとに微妙に引数の順序や有無が変わると
//! CLAUDE.md の罠2（無劣化の検証に md5 を使わず、ffprobe の `-show_data_hash CRC32`
//! でパケット単位に比較する）の実効性が静かに損なわれるため、引数列の組み立てを
//! [`csv_rows`] の1箇所に集約する。
//!
//! [`csv_rows`] は `-of csv=p=0` 系のクエリを、[`scalar_entry`] は
//! `-of default=nk=1:nw=1` で単一のスカラー値を取るクエリをそれぞれ担う。

use std::path::Path;
use std::process::Command;

use anyhow::Context;

/// `csv_rows` が実際に `ffprobe` へ渡す引数列を組み立てる（`-of csv=p=0` 系）。
///
/// `-show_data_hash CRC32` を付けるかどうかは、独立した `bool` 引数ではなく
/// `entries` に `"data_hash"` が含まれるかどうかから導出する。
///
/// # なぜ独立した `bool` 引数にしないか（実測済みの静かな失敗）
///
/// 以前は `csv_rows(ffprobe, target, stream, entries, data_hash: bool)` のように
/// `entries` と `data_hash` が別々の引数だった。呼び出し側が `entries` に
/// `"data_hash"` を含めつつ `data_hash: false` を渡す（またはその逆）というズレを
/// 起こしても、コンパイラは検出できない。そして実際に `data_hash: false` を渡すと
/// `-show_data_hash CRC32` が付かないだけでなく、`entries=packet=data_hash` は
/// **ffprobe が終了コード0のまま出力0行を返す**（実測済み）:
///
/// ```console
/// $ ffprobe -v error -select_streams a:0 -show_entries packet=data_hash -of csv=p=0 IN.mp4
/// （終了コード 0、出力 0 行、stderr 空）
/// ```
///
/// この結果、[`crate::verify::audio_packet_crc32_set`]（当時の実装）が空集合を
/// 返し、`--verify` の音声パケット集合比較（`HashSet::difference`）も dts の
/// 単調増加チェック（`windows(2)`）も、どちらも空集合・空列に対して常に成功する
/// ため、検証が**エラーを出さずに黙って無効化される**。これは CLAUDE.md の罠2
/// （無劣化の検証を実効的に行う）の実装そのものが機能しなくなる問題であり、
/// `entries` から導出する形にすることでこのズレ自体を表現不可能にする。
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

/// `scalar_entry` が実際に `ffprobe` へ渡す引数列を組み立てる
/// （`-of default=nk=1:nw=1` 系）。
fn build_scalar_args<'a>(stream: &'a str, entries: &'a str) -> Vec<&'a str> {
    vec![
        "-v",
        "error",
        "-select_streams",
        stream,
        "-show_entries",
        entries,
        "-of",
        "default=nk=1:nw=1",
    ]
}

/// `ffprobe` を1回起動し、`target` に対して `-v error -select_streams stream
/// -show_entries entries [-show_data_hash CRC32] -of csv=p=0` を実行して、空行を
/// 除いた行の列を返す。
///
/// - `stream` はストリーム指定（`"v:0"` / `"a:0"`）。
/// - `entries` は `-show_entries` にそのまま渡す値（例: `"packet=dts"`、
///   `"packet=data_hash"`）。`entries` に `"data_hash"` が含まれるときだけ
///   `-show_data_hash CRC32` を付ける（[`build_csv_args`] の doc comment参照。
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

/// `ffprobe` を1回起動し、`-v error -select_streams stream -show_entries entries
/// -of default=nk=1:nw=1` の出力を単一のスカラー値（前後の空白を trim した文字列）
/// として返す。
///
/// `stream=...` のような1値しか返らないエントリ(例: `stream=time_base`)向け。
pub fn scalar_entry(
    ffprobe: &Path,
    target: &Path,
    stream: &str,
    entries: &str,
) -> anyhow::Result<String> {
    let args = build_scalar_args(stream, entries);
    let text = run(ffprobe, target, &args)?;
    Ok(text.trim().to_string())
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
        // src/verify.rs::video_packet_crc32_in_decode_order が実際に使う形
        // (`"packet=size,data_hash"`)。部分文字列判定で正しく検出できることを確認する。
        let args = build_csv_args("v:0", "packet=size,data_hash");
        assert!(args.contains(&"-show_data_hash"));
        assert!(args.contains(&"CRC32"));
    }

    #[test]
    fn build_scalar_args_uses_default_nk1_nw1_format() {
        let args = build_scalar_args("v:0", "stream=time_base");
        assert_eq!(
            args,
            vec![
                "-v",
                "error",
                "-select_streams",
                "v:0",
                "-show_entries",
                "stream=time_base",
                "-of",
                "default=nk=1:nw=1",
            ]
        );
    }
}
