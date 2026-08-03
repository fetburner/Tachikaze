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

/// `ffprobe` を1回起動し、`target` に対して `-v error -select_streams stream
/// -show_entries entries [-show_data_hash CRC32] -of csv=p=0` を実行して、空行を
/// 除いた行の列を返す。
///
/// - `stream` はストリーム指定（`"v:0"` / `"a:0"`）。
/// - `entries` は `-show_entries` にそのまま渡す値（例: `"packet=dts"`、
///   `"packet=data_hash"`）。
/// - `data_hash` が `true` なら `-show_data_hash CRC32` を付ける（CLAUDE.md の罠2:
///   無劣化の検証に md5 を使わず、パケット単位の CRC32 で比較するための引数）。
///
/// 行の順序は ffprobe の格納順（映像はデコード順、音声もコンテナ格納順）のまま返す。
/// 集合として使いたい呼び出し元は自分で `HashSet` に集める（順序・重複を保つ必要が
/// ある呼び出し元と、集合比較で十分な呼び出し元の両方があるため、ここでは畳み込まない）。
pub fn csv_rows(
    ffprobe: &Path,
    target: &Path,
    stream: &str,
    entries: &str,
    data_hash: bool,
) -> anyhow::Result<Vec<String>> {
    let mut args: Vec<&str> = vec![
        "-v",
        "error",
        "-select_streams",
        stream,
        "-show_entries",
        entries,
    ];
    if data_hash {
        args.push("-show_data_hash");
        args.push("CRC32");
    }
    args.push("-of");
    args.push("csv=p=0");

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
/// `stream=...` のような1値しか返らないエントリ（例: `stream=time_base`）向け。
pub fn scalar_entry(
    ffprobe: &Path,
    target: &Path,
    stream: &str,
    entries: &str,
) -> anyhow::Result<String> {
    let text = run(
        ffprobe,
        target,
        &[
            "-v",
            "error",
            "-select_streams",
            stream,
            "-show_entries",
            entries,
            "-of",
            "default=nk=1:nw=1",
        ],
    )?;
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
