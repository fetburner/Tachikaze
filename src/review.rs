//! `review`: `analyze` のキャッシュから、CM 検出結果を人が確認・修正する画面（`review.html`）
//! を生成する。
//!
//! `analyze` も `prepare` も走らせない。キャッシュの `trim.avs` / `work.mp4.dtvi` /
//! `detail.jls`（と任意で `logoframe.txt`）を読むだけで、mp4 は読まない。フレームの情報は
//! すべて `.dtvi` から取る（`report` / `gate` と同じ）。
//!
//! 出力はキャッシュ内の `review/index.html` と `review/thumbs/00001.jpg`〜（キーフレーム順、
//! 1始まり5桁、幅160px）。画面が出す `cut` のコマンドは既存の `Trim(...)` 形式だけを使う。
//!
//! ## 構成
//!
//! - `build_review_json`: 純粋な関数。`.dtvi`・`trim`・`jls`・ロゴ区間から画面に埋める
//!   データを組み立てる（副作用なし。単体テストで固定する）。
//! - `render_html`: テンプレートの `__REVIEW_DATA__` を JSON に置き換える。
//! - `run`: キャッシュの読み込み、ffmpeg でのサムネイル書き出し、検査、置き換えをまとめる。
//!
//! ## サムネイルとフレーム番号の対応
//!
//! サムネイルは「何枚目か」だけでフレーム番号と対応づける。ffmpeg が書いた枚数が `.dtvi` の
//! キーパケット数と1枚でもずれると、以降のサムネイルがすべて別の位置の映像になる（エラーは
//! 出ない）。そのため [`run`] は枚数を検査し、食い違えば停止する。書き出しは一時ディレクトリに
//! 行い、検査を通ってから `review/` へ置き換える（古いサムネイルと新しい HTML が混ざらないように）。

use std::ffi::{OsStr, OsString};
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};

use crate::analyze::dtvi_frame_count;
use crate::cli::Snap;
use crate::dtvi::{self, Dtvi, DtviFrame};
use crate::errctx::PathContext;
use crate::jls::{self, JlsEntry};
use crate::logo::interval;
use crate::order::DisplayIdx;
use crate::report::missed::find_missed_candidates;
use crate::report::overcut::{find_overcut_candidates, OvercutReason};
use crate::report::time_base;
use crate::trim::TrimList;
use crate::{commands, external, plan, tools, workdir};

/// `review.html` の中で JSON に置き換えるプレースホルダ。
const PLACEHOLDER: &str = "__REVIEW_DATA__";

/// 確認画面のテンプレート（`src/review.html`）。
const TEMPLATE: &str = include_str!("review.html");

/// サムネイルの幅（px）。高さは縦横比を保つ（偶数に丸める）。
const THUMB_WIDTH: u32 = 160;

/// [`build_review_json`] に渡す材料。`input` / `output` / `cache_dir` は絶対パス。
pub struct ReviewSource<'a> {
    pub input: &'a Path,
    pub output: &'a Path,
    pub cache_dir: Option<&'a Path>,
    pub trim: &'a TrimList,
    pub dtvi: &'a Dtvi,
    pub jls: &'a [JlsEntry],
    /// ロゴ表示区間（表示順、半開区間）。ロゴ検出が無い録画は `None`。
    pub logo: Option<&'a [(u32, u32)]>,
}

/// キーフレーム `index`（0始まり）のサムネイルのファイル名（`thumbs/` からの相対パス）。
/// 組み立て（HTML の `thumb`）と書き出し（ffmpeg の連番）で同じ規則を使う。
fn thumb_relative_path(index: usize) -> String {
    format!("thumbs/{:05}.jpg", index + 1)
}

/// `path` を `file://` URL にする。RFC 3986 の非予約文字（英数字と `-._~`）と `/` 以外の
/// バイトを `%XX` にする。`file://` の後の `/` 込みのパスをそのまま使う。
fn file_url(path: &Path) -> String {
    let mut url = String::from("file://");
    for &byte in path.as_os_str().as_bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~' | b'/') {
            url.push(char::from(byte));
        } else {
            url.push_str(&format!("%{byte:02X}"));
        }
    }
    url
}

/// 境界値が「キーフレームの表示順番号」か「総フレーム数」のどちらかであることを確かめる。
/// 画面は `keep` を添字へ直接変換するので、外れた値を渡すと初期状態が黙ってずれる。
fn check_boundary(value: u32, keyframes: &[u32], total: u32) -> Result<()> {
    if value == total || keyframes.binary_search(&value).is_ok() {
        return Ok(());
    }
    bail!(
        "review: 保持区間の境界 {value} がキーフレームでも総フレーム数（{total}）でもありません。\
         `trim.avs` と `.dtvi` が同じ録画のものか確認してください"
    )
}

/// ヘッダの `frame_count` とフレーム表の行数が一致するかを検査する。
///
/// フレーム表が途中で切れていると、行数を総フレーム数とみなす `review` は、後ろの区間を
/// 黙って欠いた確認画面を作るか、原因の分からない重なりエラーで止まる。`analyze` の
/// 階層化ロゴ検出（手順0）と同じ検査を `review` の前にも行う。
fn check_frame_count(dtvi: &Dtvi, dtvi_path: &Path, analyze_cmd: &str) -> Result<()> {
    let header = dtvi_frame_count(dtvi)?;
    let rows = dtvi.frames.len() as u64;
    if header != rows {
        bail!(
            "review: .dtvi のヘッダの frame_count（{header}）とフレーム表の行数（{rows}）が一致しません: {}\n\
             .dtvi が途中で切れている可能性があります。次を実行して作り直してください:\n  {analyze_cmd}",
            dtvi_path.display()
        );
    }
    Ok(())
}

/// 確認画面に埋める JSON を組み立てる（純粋な関数）。
///
/// 区間はすべて表示順フレーム番号の半開区間 `[start, end)`。
pub fn build_review_json(src: &ReviewSource) -> Result<Value> {
    let total =
        u32::try_from(src.dtvi.frames.len()).context("総フレーム数が u32 に収まりません")?;
    let fps = commands::fps_from_dtvi(src.dtvi);
    let (tb_num, tb_den) = time_base(src.dtvi);

    let keyframe_frames: Vec<&DtviFrame> = src
        .dtvi
        .frames
        .iter()
        .filter(|f| f.is_key_packet())
        .collect();
    let keyframes: Vec<u32> = keyframe_frames.iter().map(|f| f.frame_number.0).collect();

    let keyframe_json = keyframe_frames
        .iter()
        .enumerate()
        .map(|(index, frame)| {
            if !frame.has_valid_pts() {
                bail!(
                    "review: キーフレーム {} の PTS が無効です（.dtvi の FLAG_VALID_PTS）。時刻を出せません",
                    frame.frame_number.0
                );
            }
            Ok(json!({
                "frame": frame.frame_number.0,
                "time": frame.pts as f64 * tb_num / tb_den,
                "thumb": thumb_relative_path(index),
            }))
        })
        .collect::<Result<Vec<Value>>>()?;

    // 保持区間は analyze の trim.avs を outward にスナップしたもの（cut と同じ規則）。
    let sync: Vec<DisplayIdx> = keyframe_frames.iter().map(|f| f.frame_number).collect();
    let snapped = plan::snap(src.trim, &sync, total, Snap::Outward)?;
    let mut keep = Vec::with_capacity(snapped.len());
    for range in &snapped {
        let (start, end) = (range.start.snapped.0, range.end.snapped.0);
        check_boundary(start, &keyframes, total)?;
        check_boundary(end, &keyframes, total)?;
        keep.push(json!([start, end]));
    }

    // 見逃し候補は両端を含むので、半開区間に直す（スナップはしない）。
    let missed: Vec<Value> = find_missed_candidates(src.trim, src.jls)
        .iter()
        .map(|c| json!([c.start, c.end + 1]))
        .collect();

    let over: Vec<Value> = find_overcut_candidates(src.trim, src.jls, src.logo, total, fps)
        .iter()
        .map(|c| {
            let reasons: Vec<&str> = c
                .reasons
                .iter()
                .map(|r| match r {
                    OvercutReason::Logo => "logo",
                    OvercutReason::Grid => "grid",
                })
                .collect();
            json!({ "range": [c.start, c.end], "reasons": reasons })
        })
        .collect();

    let logo = match src.logo {
        Some(intervals) => {
            let list: Vec<Value> = intervals.iter().map(|&(s, e)| json!([s, e])).collect();
            Value::Array(list)
        }
        None => Value::Null,
    };

    Ok(json!({
        "input": src.input.display().to_string(),
        "output": src.output.display().to_string(),
        "cache_dir": src.cache_dir.map(|p| p.display().to_string()),
        "video": file_url(src.input),
        "total_frames": total,
        "keyframes": keyframe_json,
        "keep": keep,
        "missed": missed,
        "over": over,
        "logo": logo,
    }))
}

/// テンプレートの `__REVIEW_DATA__` を JSON に置き換える。
///
/// JSON の中の `<` は `\u003c` にする。`</script>` がファイル名に含まれていても、
/// `<script>` の中で閉じタグとして解釈されないようにするため（`</` だけでなく `<!--` も
/// 同時に防げる）。`\u003c` は JSON としても JavaScript の文字列としても同じ値になる。
pub fn render_html(template: &str, data: &Value) -> Result<String> {
    let json = serde_json::to_string(data).context("確認画面のデータを JSON にできません")?;
    let escaped = json.replace('<', "\\u003c");
    Ok(template.replacen(PLACEHOLDER, &escaped, 1))
}

/// `review` の入力（ユーザーが渡した mp4 と `--cache-dir`）。
pub struct ReviewConfig {
    pub input: PathBuf,
    /// `-o`。`None` なら [`output_path`] の既定。
    pub output: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
}

/// `dir/thumbs/` にキーフレーム数ぶんの JPEG があるかを検査する。
///
/// 枚数が一致しないと以降のサムネイルが別の位置の映像になるため、1枚でも不足・過剰なら
/// 両方の数を出して停止する。
fn check_thumbnails(thumbs_dir: &Path, expected: usize) -> Result<()> {
    let actual = fs::read_dir(thumbs_dir)
        .path_ctx("サムネイルの一覧取得", thumbs_dir)?
        .count();
    if actual != expected {
        bail!(
            "review: 書き出されたサムネイルが {actual} 枚でした。`.dtvi` のキーフレームは {expected} 枚です。\
             フレーム番号とサムネイルの対応が取れないため、確認画面を作りません"
        );
    }
    for index in 0..expected {
        let path = thumbs_dir.join(format!("{:05}.jpg", index + 1));
        if !path.is_file() {
            bail!("review: サムネイルがありません: {}", path.display());
        }
    }
    Ok(())
}

/// キャッシュ内の確認画面（`review/`）を作り、`index.html` の絶対パスを返す。
///
/// 必須の入力（`trim.avs` / `work.mp4.dtvi` / `detail.jls`）が無ければ、`analyze` の
/// 実行例を添えて停止する。`analyze` は走らせない。
pub fn run(config: &ReviewConfig) -> Result<PathBuf> {
    let cache = config.cache_dir.as_deref();
    let input =
        fs::canonicalize(&config.input).path_ctx("入力ファイルの絶対パス解決", &config.input)?;

    let trim_path =
        workdir::cached_trim_path(cache, &input).path_ctx("`trim.avs` の自動解決", &input)?;
    let dtvi_path =
        workdir::cached_dtvi_path(cache, &input).path_ctx("`.dtvi` の自動解決", &input)?;
    let jls_path = workdir::cached_detail_jls_path(cache, &input)
        .path_ctx("`detail.jls` の自動解決", &input)?;
    let logo_path =
        workdir::cached_logoframe_path(cache, &input).path_ctx("logoframe の自動解決", &input)?;
    let review_dir =
        workdir::cached_review_dir(cache, &input).path_ctx("確認画面の出力先", &input)?;

    for required in [&trim_path, &dtvi_path, &jls_path] {
        if !required.is_file() {
            bail!(
                "キャッシュに必要なファイルが見つかりませんでした: {}\n\
                 先に次を実行してください:\n  {}",
                required.display(),
                workdir::analyze_command(cache, &config.input)
            );
        }
    }

    let trim = TrimList::parse(&read(&trim_path)?)
        .map_err(|err| anyhow!("trim ファイルのパースに失敗しました: {err}"))?;
    let dtvi = dtvi::parse(&read(&dtvi_path)?)
        .map_err(|err| anyhow!("`.dtvi` のパースに失敗しました: {err}"))?;
    check_frame_count(
        &dtvi,
        &dtvi_path,
        &workdir::analyze_command(cache, &config.input),
    )?;
    let jls_entries = jls::parse(&read(&jls_path)?)
        .map_err(|err| anyhow!("`detail.jls` のパースに失敗しました: {err}"))?;
    let logo = if logo_path.is_file() {
        let intervals = interval::parse_text(&read(&logo_path)?)
            .map_err(|err| anyhow!("logoframe テキストのパースに失敗しました: {err}"))?;
        Some(intervals)
    } else {
        None
    };

    // 画面のコマンドはどのディレクトリで実行されるか分からないので、相対パスは絶対パスにする。
    let output = match &config.output {
        Some(path) => std::path::absolute(path).path_ctx("-o の絶対パス解決", path)?,
        None => output_path(&input)?,
    };
    let cache_dir_abs = cache
        .map(|dir| std::path::absolute(dir).path_ctx("--cache-dir の絶対パス解決", dir))
        .transpose()?;
    let data = build_review_json(&ReviewSource {
        input: &input,
        output: &output,
        cache_dir: cache_dir_abs.as_deref(),
        trim: &trim,
        dtvi: &dtvi,
        jls: &jls_entries,
        logo: logo.as_deref(),
    })?;

    let cache_for_input = review_dir.parent().ok_or_else(|| {
        anyhow!(
            "確認画面の出力先の親ディレクトリがありません: {}",
            review_dir.display()
        )
    })?;
    let staging = cache_for_input.join(format!("review.tmp-{}", std::process::id()));
    if let Err(err) = build_staging(&staging, &input, &data, &dtvi) {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    // 検査を通った一時ディレクトリだけを `review/` にする。古い `review/` は残さない。
    // 置き換えのどこで失敗しても、一時ディレクトリは消してからエラーを返す。
    let place = || -> Result<()> {
        if review_dir.exists() {
            fs::remove_dir_all(&review_dir).path_ctx("古い確認画面の削除", &review_dir)?;
        }
        fs::rename(&staging, &review_dir).path_ctx("確認画面の配置", &review_dir)?;
        Ok(())
    };
    if let Err(err) = place() {
        let _ = fs::remove_dir_all(&staging);
        return Err(err);
    }

    std::path::absolute(review_dir.join("index.html"))
        .context("index.html の絶対パス解決に失敗しました")
}

/// 一時ディレクトリにサムネイルと `index.html` を書き出す。`index.html` は最後に書く。
fn build_staging(staging: &Path, input: &Path, data: &Value, dtvi: &Dtvi) -> Result<()> {
    if staging.exists() {
        fs::remove_dir_all(staging).path_ctx("前回の一時ディレクトリの削除", staging)?;
    }
    let thumbs_dir = staging.join("thumbs");
    fs::create_dir_all(&thumbs_dir).path_ctx("サムネイル用ディレクトリの作成", &thumbs_dir)?;

    let keyframe_count = dtvi.frames.iter().filter(|f| f.is_key_packet()).count();
    extract_keyframe_thumbnails(input, staging, &thumbs_dir)?;
    check_thumbnails(&thumbs_dir, keyframe_count)?;

    let html = render_html(TEMPLATE, data)?;
    let index = staging.join("index.html");
    fs::write(&index, html).path_ctx("index.html の書き出し", &index)?;
    Ok(())
}

/// キーフレームだけを読み、幅 [`THUMB_WIDTH`] の連番 JPEG にする。
///
/// `-skip_frame nokey` は `-i` より前（デコーダ側の入力オプション）に置く。後ろに置くと
/// 出力側のオプションとして解釈され、全フレームが書き出されてしまう。`-fps_mode passthrough`
/// でフレームを間引いたり複製したりしない（枚数の検査はこれを前提にしている）。
fn extract_keyframe_thumbnails(input: &Path, cwd: &Path, thumbs_dir: &Path) -> Result<()> {
    let ffmpeg = tools::resolve_tool(tools::FFMPEG)?;
    let filter = format!("scale={THUMB_WIDTH}:-2");
    let pattern = thumbs_dir.join("%05d.jpg");
    let args: Vec<&OsStr> = vec![
        OsStr::new("-hide_banner"),
        OsStr::new("-loglevel"),
        OsStr::new("error"),
        OsStr::new("-skip_frame"),
        OsStr::new("nokey"),
        OsStr::new("-i"),
        input.as_os_str(),
        OsStr::new("-map"),
        OsStr::new("0:v:0"),
        OsStr::new("-an"),
        OsStr::new("-sn"),
        OsStr::new("-vf"),
        OsStr::new(&filter),
        OsStr::new("-fps_mode"),
        OsStr::new("passthrough"),
        OsStr::new("-q:v"),
        OsStr::new("4"),
        OsStr::new("-f"),
        OsStr::new("image2"),
        pattern.as_os_str(),
    ];
    external::run(&ffmpeg, &args, cwd)?;
    Ok(())
}

/// 入力と同じディレクトリの `<stem>_CMcut.mp4`。
pub(crate) fn output_path(input: &Path) -> Result<PathBuf> {
    let stem = input
        .file_stem()
        .ok_or_else(|| anyhow!("入力ファイル名から出力名を作れません: {}", input.display()))?;
    let mut name = OsString::from(stem);
    name.push("_CMcut.mp4");
    Ok(input.with_file_name(name))
}

fn read(path: &Path) -> Result<String> {
    fs::read_to_string(path).path_ctx("キャッシュファイルの読み込み", path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtvi::{FLAG_KEY_PACKET, FLAG_VALID_PTS};
    use crate::order::DecodeIdx;
    use std::collections::HashMap;

    /// `total` フレームで、`key_every` フレームごとにキーパケットを置いた `.dtvi` を作る。
    /// time_base は 1/30000、pts はフレーム番号 × 1001。
    fn dtvi_with_keyframes(total: u32, key_every: u32) -> Dtvi {
        let frames = (0..total)
            .map(|n| DtviFrame {
                frame_number: DisplayIdx(n),
                sample_number: DecodeIdx(n),
                random_access_sample: DecodeIdx(n - n % key_every),
                file_offset: 0,
                pts: i64::from(n) * 1001,
                dts: i64::from(n) * 1001,
                duration: 1001,
                flags: FLAG_VALID_PTS
                    | if n % key_every == 0 {
                        FLAG_KEY_PACKET
                    } else {
                        0
                    },
            })
            .collect();
        let header = HashMap::from([
            ("time_base_num".to_string(), "1".to_string()),
            ("time_base_den".to_string(), "30000".to_string()),
            ("frame_count".to_string(), total.to_string()),
        ]);
        Dtvi {
            format_version: 1,
            header,
            frames,
        }
    }

    fn trim(text: &str) -> TrimList {
        TrimList::parse(text).expect("trim のパース")
    }

    fn build(
        input: &Path,
        trim: &TrimList,
        dtvi: &Dtvi,
        jls: &[JlsEntry],
        logo: Option<&[(u32, u32)]>,
    ) -> Value {
        build_review_json(&ReviewSource {
            input,
            output: &input.with_file_name("IN_CMcut.mp4"),
            cache_dir: None,
            trim,
            dtvi,
            jls,
            logo,
        })
        .expect("JSON の組み立て")
    }

    /// 完了条件: `keep` は同じ trim を outward スナップした結果と一致し、境界は
    /// キーフレームか総フレーム数に一致する。
    #[test]
    fn keep_matches_outward_snap_and_lands_on_keyframes() {
        // 4000フレーム、120フレームごとにキーパケット（最終 GOP は 3960..3999）。
        let dtvi = dtvi_with_keyframes(4000, 120);
        let trim = trim("Trim(0,999) ++ Trim(1500,2499) ++ Trim(3000,3999)");
        let json = build(Path::new("/rec/IN.mp4"), &trim, &dtvi, &[], None);

        let sync: Vec<DisplayIdx> = (0..4000).step_by(120).map(DisplayIdx).collect();
        let expected: Vec<[u32; 2]> = plan::snap(&trim, &sync, 4000, Snap::Outward)
            .expect("snap")
            .iter()
            .map(|r| [r.start.snapped.0, r.end.snapped.0])
            .collect();
        assert_eq!(json["keep"], json!(expected));
        assert_eq!(json["keep"], json!([[0, 1080], [1440, 2520], [3000, 4000]]));

        let keyframes: Vec<u64> = json["keyframes"]
            .as_array()
            .expect("配列")
            .iter()
            .map(|k| k["frame"].as_u64().expect("frame"))
            .collect();
        for pair in json["keep"].as_array().expect("配列") {
            for v in pair.as_array().expect("区間") {
                let v = v.as_u64().expect("整数");
                assert!(
                    v == 4000 || keyframes.contains(&v),
                    "境界 {v} がキーフレームでない"
                );
            }
        }
    }

    /// 完了条件: 見逃し候補は両端を含む `MissedCandidate` を半開区間に直して出す。
    /// 境界ちょうど（保持区間の端に接するエントリ）を含めて固定する。
    #[test]
    fn missed_candidates_are_half_open() {
        let dtvi = dtvi_with_keyframes(4000, 120);
        // ギャップ 500 フレームが2つ → 既知の CM 長は 500。
        let trim = trim("Trim(0,999) ++ Trim(1500,2499) ++ Trim(3000,3999)");
        // 保持区間 [0,1000) の内側に、長さ 500 のエントリ（両端含む [100,599]）。
        // 保持区間の末尾に接する [500,999] も長さ 500 なので候補になる。
        let jls = vec![jls_entry(100, 599), jls_entry(500, 999)];
        let json = build(Path::new("/rec/IN.mp4"), &trim, &dtvi, &jls, None);
        assert_eq!(json["missed"], json!([[100, 600], [500, 1000]]));
    }

    fn jls_entry(start: u32, end: u32) -> JlsEntry {
        JlsEntry {
            start,
            end,
            duration_sec: 0,
            error_frames: 0,
            logo_sec: 0,
            label: ":CM".to_string(),
        }
    }

    /// 完了条件: `logoframe.txt` が無いとき `logo` は `null`。
    #[test]
    fn logo_is_null_without_logoframe() {
        let dtvi = dtvi_with_keyframes(4000, 120);
        let trim = trim("Trim(0,999)");
        let json = build(Path::new("/rec/IN.mp4"), &trim, &dtvi, &[], None);
        assert_eq!(json["logo"], Value::Null);
    }

    /// カット区間 [1000,1500) にロゴが 300 フレーム出ていれば切りすぎ候補（logo）になる。
    /// 区間は半開のまま出る。
    #[test]
    fn overcut_reports_half_open_range_with_reason() {
        let dtvi = dtvi_with_keyframes(4000, 120);
        let trim = trim("Trim(0,999) ++ Trim(1500,2499) ++ Trim(3000,3999)");
        let logo = [(1100u32, 1400u32)];
        let json = build(Path::new("/rec/IN.mp4"), &trim, &dtvi, &[], Some(&logo));
        assert_eq!(
            json["over"],
            json!([{"range": [1000, 1500], "reasons": ["logo"]}])
        );
        assert_eq!(json["logo"], json!([[1100, 1400]]));
    }

    /// 完了条件: ファイル名に `</script>` を含んでも、生成 HTML に生の `</script>` が
    /// JSON 部分に現れない。
    #[test]
    fn script_breaking_file_name_is_escaped() {
        let dtvi = dtvi_with_keyframes(4000, 120);
        let trim = trim("Trim(0,999)");
        let input = Path::new("/rec/</script><script>alert(1)//.mp4");
        let json = build(input, &trim, &dtvi, &[], None);
        let html =
            render_html("<script>const DATA = __REVIEW_DATA__;</script>", &json).expect("描画");
        // テンプレート自身の閉じタグ1つだけが残る。
        assert_eq!(html.matches("</script>").count(), 1, "{html}");
        assert!(html.contains("\\u003c/script>"));
    }

    /// 完了条件: 空白・日本語を含むパスの `video` がパーセントエンコードされる。
    #[test]
    fn video_url_is_percent_encoded() {
        let url = file_url(Path::new("/Users/a/録画 1'x.mp4"));
        let encoded_ja: String = "録画".bytes().map(|b| format!("%{b:02X}")).collect();
        assert_eq!(url, format!("file:///Users/a/{encoded_ja}%201%27x.mp4"));
    }

    /// 境界が無効（キーフレームでも総フレーム数でもない）なら黙らずに止まる。
    #[test]
    fn check_boundary_rejects_non_keyframe() {
        assert!(check_boundary(120, &[0, 120], 4000).is_ok());
        assert!(check_boundary(4000, &[0, 120], 4000).is_ok());
        assert!(check_boundary(60, &[0, 120], 4000).is_err());
    }

    /// `render_html` はプレースホルダを1つだけ置き換える（テンプレートに本物が1つある前提）。
    #[test]
    fn real_template_has_exactly_one_placeholder() {
        assert_eq!(TEMPLATE.matches(PLACEHOLDER).count(), 1);
    }

    /// フレーム表だけ切れた `.dtvi`（ヘッダは599、行は40）が止まり、両方の値が出ること。
    #[test]
    fn frame_count_mismatch_is_rejected_with_both_values() {
        let mut dtvi = dtvi_with_keyframes(40, 120);
        dtvi.header
            .insert("frame_count".to_string(), "599".to_string());
        let err = check_frame_count(
            &dtvi,
            Path::new("/cache/work.mp4.dtvi"),
            "tachikaze analyze",
        )
        .expect_err("食い違いは止まること");
        let msg = err.to_string();
        assert!(msg.contains("（599）") && msg.contains("（40）"), "{msg}");
        assert!(msg.contains("/cache/work.mp4.dtvi"), "{msg}");
        assert!(msg.contains("tachikaze analyze"), "{msg}");
    }

    #[test]
    fn frame_count_match_is_accepted() {
        let dtvi = dtvi_with_keyframes(40, 120);
        check_frame_count(
            &dtvi,
            Path::new("/cache/work.mp4.dtvi"),
            "tachikaze analyze",
        )
        .expect("一致すれば通ること");
    }

    /// `check_thumbnails` の検査用に、テスト名ごとに一意な一時ディレクトリへ空の JPEG 名を置く。
    /// 同じプロセス内で並列に走るので、`label` はテストごとに変える。
    fn thumbs_dir_with(label: &str, names: &[&str]) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "tachikaze-review-thumbs-{label}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("一時ディレクトリの作成");
        for name in names {
            fs::write(dir.join(name), b"").expect("サムネイルの作成");
        }
        dir
    }

    /// 枚数が足りないとき、書き出した枚数と期待する枚数の両方を示して止まる。
    #[test]
    fn check_thumbnails_rejects_too_few_and_reports_both_counts() {
        let dir = thumbs_dir_with(
            "too-few",
            &["00001.jpg", "00002.jpg", "00003.jpg", "00004.jpg"],
        );
        let result = check_thumbnails(&dir, 5);
        let _ = fs::remove_dir_all(&dir);
        let msg = result.expect_err("枚数不足は止まること").to_string();
        assert!(msg.contains("4 枚"), "書き出した枚数が無い: {msg}");
        assert!(msg.contains("5 枚"), "期待する枚数が無い: {msg}");
    }

    /// 枚数は合っていても連番に穴があれば、欠けている番号を示して止まる。
    #[test]
    fn check_thumbnails_rejects_gap_and_names_missing_file() {
        let dir = thumbs_dir_with("gap", &["00001.jpg", "00002.jpg", "00004.jpg"]);
        let result = check_thumbnails(&dir, 3);
        let _ = fs::remove_dir_all(&dir);
        let msg = result.expect_err("連番の穴は止まること").to_string();
        assert!(msg.contains("00003.jpg"), "欠けた番号が無い: {msg}");
    }

    /// 枚数が多すぎるときも止まる（フレーム番号との対応が取れないため）。
    #[test]
    fn check_thumbnails_rejects_too_many() {
        let names = [
            "00001.jpg",
            "00002.jpg",
            "00003.jpg",
            "00004.jpg",
            "00005.jpg",
            "00006.jpg",
        ];
        let dir = thumbs_dir_with("too-many", &names);
        let result = check_thumbnails(&dir, 5);
        let _ = fs::remove_dir_all(&dir);
        assert!(result.is_err(), "過剰な枚数は止まること");
    }

    /// 枚数と連番が揃っていれば通る。
    #[test]
    fn check_thumbnails_accepts_exact_match() {
        let names = [
            "00001.jpg",
            "00002.jpg",
            "00003.jpg",
            "00004.jpg",
            "00005.jpg",
        ];
        let dir = thumbs_dir_with("exact", &names);
        let result = check_thumbnails(&dir, 5);
        let _ = fs::remove_dir_all(&dir);
        result.expect("一致すれば通ること");
    }
}
