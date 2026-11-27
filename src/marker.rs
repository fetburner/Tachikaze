//! Trim リストと `.dtvi` から、CM 位置を記録するための2つの文字列を作る純関数。
//!
//! 録画ファイルは切らず、プレイヤーで CM を飛ばせるように位置だけを書き出す。出力は次の2つ。
//!
//! - **EDL**（Kodi / MPlayer 形式）: CM 区間ごとに `開始秒\t終了秒\t3\n` の1行。
//!   Kodi は動画と同名の `.edl` を読んで commercial break（`3`）を自動で飛ばす。
//! - **ffmetadata**: 先頭に `;FFMETADATA1` を置き、本編 / CM が交互に並ぶ `[CHAPTER]` 区間で
//!   `[0, frame_count)` を隙間なく覆う。後続の処理が ffmpeg に渡して mp4 に埋め込む。
//!
//! ## 規則（後続と揃えるための決定事項）
//!
//! - **フレーム数**は `dtvi.frames.len()`。ヘッダの `frame_count` は見ない（抜粋の `.dtvi` と
//!   食い違うため。行数を正とする）。
//! - **CM 区間**は Trim の補集合。`[0, frame_count)` のうち保持区間以外で、先頭と末尾の区間も
//!   含める（前後の番組も飛ばしたい対象のため）。
//! - **境界の時刻**は time_base 単位の整数で持つ。境界 `b` が `0 < b < frame_count` なら
//!   `frames[b].pts`。`b == 0` なら 0、`b == frame_count` なら最終フレームの `pts + duration`。
//!   **フレーム表の添字は表示順**（`frame_number`）であり、デコード順の `sample_number` で引くと
//!   B フレームの並べ替えぶんずれた時刻になる。
//! - **時刻を秒の浮動小数で持ち回さない。** ffmetadata には pts の整数をそのまま書き、丸め誤差を
//!   持ち込まない。秒に直すのは EDL を書くときだけ（小数点以下3桁）。
//! - **切らないので境界をキーフレームに丸めない。** 境界は指定どおりのフレームの pts になる。
//! - **隣接する保持区間は1つにまとめる。** `Trim(0,4) ++ Trim(5,9)` は本編が途切れない1区間
//!   なので、チャプターも1つにする。本編 / CM の交互列を保つため。
//! - **time_base は `.dtvi` ヘッダから整数で読む。** `crate::report::time_base` は使わない。
//!   あちらはキーが欠けると黙って `1/1` を返し、f64 を返すため整数の ffmetadata に合わないから。
//!
//! 次の場合はエラーにする（黙って別の値を書かない）:
//! - time_base が欠けている、0 以下、または整数としてパースできない
//! - Trim の最終フレーム（両端を含む。trim.avs の `Trim(s,e)` の `e`）が `.dtvi` の範囲を超える
//! - 境界に使うフレームの pts が無効（`has_valid_pts()` が false）

use std::ffi::OsString;
use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::analyze::dtvi_frame_count;
use crate::commands::reject_dash_output;
use crate::dtvi::{self, Dtvi};
use crate::errctx::PathContext;
use crate::trim::TrimList;
use crate::{external, mp4io, tools, workdir};

/// [`markers`] の出力。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Markers {
    /// Kodi / MPlayer 形式の EDL。CM 区間が無ければ空文字列。
    pub edl: String,
    /// ffmpeg の ffmetadata（`;FFMETADATA1` で始まるチャプター列）。
    pub ffmetadata: String,
    /// CM 区間の数（先頭・末尾を含む）。
    pub cm_count: usize,
}

/// マーカー生成に失敗したことを表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkerError {
    /// time_base のヘッダ値が欠けている、0 以下、または整数でない。
    InvalidTimeBase {
        key: &'static str,
        value: Option<String>,
    },
    /// Trim の最終フレーム（両端を含む）が `.dtvi` のフレーム範囲 `0..frame_count` を超えている。
    /// `last_frame` は trim.avs の `Trim(s,e)` の `e` と同じ値。
    TrimBeyondFrames {
        last_frame: usize,
        frame_count: usize,
    },
    /// 境界に使うフレームの pts が無効。`frame_number` は表示順の添字。
    InvalidPts { frame_number: usize },
}

impl fmt::Display for MarkerError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MarkerError::InvalidTimeBase { key, value } => match value {
                Some(v) => write!(f, ".dtvi の {key} が正の整数ではない: {v:?}"),
                None => write!(f, ".dtvi に {key} がない"),
            },
            MarkerError::TrimBeyondFrames {
                last_frame,
                frame_count,
            } => match frame_count.checked_sub(1) {
                Some(max) => write!(
                    f,
                    "Trim の最終フレーム {last_frame} が .dtvi のフレーム範囲 0..={max} を超えている"
                ),
                None => write!(
                    f,
                    "Trim の最終フレーム {last_frame} があるが .dtvi にフレームがない"
                ),
            },
            MarkerError::InvalidPts { frame_number } => {
                write!(f, "表示順 {frame_number} のフレームの pts が無効")
            }
        }
    }
}

impl std::error::Error for MarkerError {}

/// 本編 / CM の1区間。`[start, end)` は表示順のフレーム番号。
struct Segment {
    start: usize,
    end: usize,
    is_cm: bool,
}

/// Trim リストと `.dtvi` から EDL と ffmetadata を作る。
pub fn markers(trim: &TrimList, dtvi: &Dtvi) -> Result<Markers, MarkerError> {
    let (num, den) = time_base(dtvi)?;
    let frame_count = dtvi.frames.len();

    let mut edl = String::new();
    let mut ffmetadata = String::from(";FFMETADATA1\n");
    let mut cm_count = 0;
    for seg in segments(trim, frame_count)? {
        let start = boundary_pts(dtvi, seg.start)?;
        let end = boundary_pts(dtvi, seg.end)?;
        let title = if seg.is_cm {
            cm_count += 1;
            // EDL: 3 は commercial break（自動で飛ばし、巻き戻せば見られる）。
            edl.push_str(&format!(
                "{:.3}\t{:.3}\t3\n",
                seconds(start, num, den),
                seconds(end, num, den)
            ));
            "CM"
        } else {
            "本編"
        };
        ffmetadata.push_str(&format!(
            "[CHAPTER]\nTIMEBASE={num}/{den}\nSTART={start}\nEND={end}\ntitle={title}\n"
        ));
    }

    Ok(Markers {
        edl,
        ffmetadata,
        cm_count,
    })
}

/// `mark` の出力先。`-o` と `--edl-only` は clap の排他グループで、どちらか一方だけが来る。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkTarget {
    /// チャプター付き mp4 を書き、その隣に `.edl` を書く。
    Video(PathBuf),
    /// mp4 は作らず、入力の隣に `<入力の stem>.edl` だけを書く（再生するのは元の録画）。
    EdlOnly,
}

/// [`run`] の入力。`trim` / `dtvi` が `None` のときはキャッシュから解決する。
/// `trim` が `-` のときは標準入力から読む（`cut --trim -` と同じ）。
#[derive(Debug, Clone)]
pub struct MarkConfig {
    pub input: PathBuf,
    pub trim: Option<PathBuf>,
    pub dtvi: Option<PathBuf>,
    pub cache_dir: Option<PathBuf>,
    pub target: MarkTarget,
}

/// [`run`] の結果。`video` は `MarkTarget::Video` のときだけ `Some`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkOutcome {
    pub video: Option<PathBuf>,
    pub edl: PathBuf,
    pub cm_count: usize,
}

/// `mark` 本体。Trim と `.dtvi` から [`markers`] で EDL と ffmetadata を作り、ファイルに書く。
///
/// 書く前に次を検査し、満たさなければ何も書かずに止める（CLAUDE.md 罠3: 別の録画の `.dtvi`
/// はフレーム数が違えばエラーにしないと時刻が黙ってずれる）:
/// - `.dtvi` ヘッダの `frame_count` とフレーム表の行数が一致する
/// - 入力 mp4 の映像サンプル数が `.dtvi` の行数と一致する
/// - `-o` が入力と同じファイルではない（ffmpeg は入力を読みながら出力を書くため、同じだと壊す）
///
/// mp4 は `-map 0` で全ストリームを残し（字幕を黙って落とさない）、`-c copy` でチャプターだけを
/// 足す。一時ファイルへ書いてから rename し、失敗した場合は一時ファイルを消す。
pub fn run(config: &MarkConfig) -> Result<MarkOutcome> {
    let cache = config.cache_dir.as_deref();
    let input =
        fs::canonicalize(&config.input).path_ctx("入力ファイルの絶対パス解決", &config.input)?;
    let analyze_cmd = || workdir::analyze_command(cache, &config.input);

    if let MarkTarget::Video(out) = &config.target {
        reject_dash_output(out, "-o")?;
        let out_abs = output_identity(out)?;
        if out_abs == input {
            bail!(
                "-o が入力と同じファイルです（元の録画は上書きしません）: {}",
                out.display()
            );
        }
    }

    let trim_text = match &config.trim {
        Some(path) if path == Path::new("-") => {
            let mut buf = String::new();
            io::stdin()
                .read_to_string(&mut buf)
                .context("trim の標準入力からの読み込みに失敗しました")?;
            buf
        }
        Some(path) => fs::read_to_string(path).path_ctx("trim ファイルの読み込み", path)?,
        None => {
            let cached = workdir::cached_trim_path(cache, &input)
                .path_ctx("`trim.avs` の自動解決", &input)?;
            read_cached(&cached, &analyze_cmd)?
        }
    };
    let trim = TrimList::parse(&trim_text)
        .map_err(|err| anyhow!("trim ファイルのパースに失敗しました: {err}"))?;

    let dtvi_path = match &config.dtvi {
        Some(path) => path.clone(),
        None => workdir::cached_dtvi_path(cache, &input).path_ctx("`.dtvi` の自動解決", &input)?,
    };
    let dtvi_text = match &config.dtvi {
        Some(path) => fs::read_to_string(path).path_ctx("`.dtvi` の読み込み", path)?,
        None => read_cached(&dtvi_path, &analyze_cmd)?,
    };
    let dtvi =
        dtvi::parse(&dtvi_text).map_err(|err| anyhow!("`.dtvi` のパースに失敗しました: {err}"))?;

    let header = dtvi_frame_count(&dtvi)?;
    let rows = dtvi.frames.len() as u64;
    if header != rows {
        bail!(
            "mark: .dtvi のヘッダの frame_count（{header}）とフレーム表の行数（{rows}）が一致しません: {}\n\
             .dtvi が途中で切れている可能性があります。次を実行して作り直してください:\n  {}",
            dtvi_path.display(),
            analyze_cmd()
        );
    }

    let moov = mp4io::read::read_moov(&input).path_ctx("入力 mp4 の読み込み", &input)?;
    let (video_trak, _) = mp4io::read::find_video_track(&moov)
        .ok_or_else(|| anyhow!("入力に映像トラックがありません: {}", input.display()))?;
    let file_len = fs::metadata(&input)
        .path_ctx("入力ファイルのサイズ取得", &input)?
        .len();
    let samples = mp4io::read::samples(&video_trak.mdia.minf.stbl, file_len)?.len();
    if samples != dtvi.frames.len() {
        bail!(
            "mark: 入力 mp4 の映像サンプル数（{samples}）と .dtvi のフレーム数（{}）が一致しません。\
             別の録画の .dtvi を指定していないか確認してください: {}",
            dtvi.frames.len(),
            dtvi_path.display()
        );
    }

    let markers = markers(&trim, &dtvi).map_err(|err| anyhow!("mark: {err}"))?;

    match &config.target {
        MarkTarget::EdlOnly => {
            let edl = input.with_extension("edl");
            fs::write(&edl, &markers.edl).path_ctx("EDL の書き出し", &edl)?;
            Ok(MarkOutcome {
                video: None,
                edl,
                cm_count: markers.cm_count,
            })
        }
        MarkTarget::Video(out) => {
            let out_abs = output_identity(out)?;
            let name = out_abs
                .file_name()
                .ok_or_else(|| anyhow!("-o からファイル名を取れません: {}", out.display()))?;
            let ffmpeg = tools::resolve_tool(tools::FFMPEG)
                .context("mark: チャプターの埋め込みに ffmpeg が必要です")?;

            let chapters = workdir::cached_chapters_path(cache, &input)
                .path_ctx("チャプターのキャッシュパスの解決", &input)?;
            let chapters_dir = chapters.parent().ok_or_else(|| {
                anyhow!(
                    "チャプターのキャッシュパスが不正です: {}",
                    chapters.display()
                )
            })?;
            fs::create_dir_all(chapters_dir)
                .path_ctx("キャッシュディレクトリの作成", chapters_dir)?;
            fs::write(&chapters, &markers.ffmetadata)
                .path_ctx("ffmetadata の書き出し", &chapters)?;

            // 一時ファイルは出力と同じディレクトリに置く（rename を同一ファイルシステム内に保つため）。
            // 拡張子は .mp4 のまま残す（ffmpeg が出力形式を拡張子で決めるため）。
            let parent = out_abs
                .parent()
                .ok_or_else(|| anyhow!("-o の親ディレクトリが不正です: {}", out.display()))?;
            let mut tmp_name = OsString::from(".");
            tmp_name.push(name);
            tmp_name.push(".tmp.mp4");
            let tmp = parent.join(tmp_name);

            let mut args: Vec<OsString> = ["-hide_banner", "-loglevel", "error", "-y", "-i"]
                .iter()
                .map(OsString::from)
                .collect();
            args.push(input.clone().into_os_string());
            args.extend(["-f", "ffmetadata", "-i"].iter().map(OsString::from));
            args.push(chapters.clone().into_os_string());
            args.extend(
                [
                    "-map",
                    "0",
                    "-map_metadata",
                    "0",
                    "-map_chapters",
                    "1",
                    "-c",
                    "copy",
                ]
                .iter()
                .map(OsString::from),
            );
            args.push(tmp.clone().into_os_string());
            let args_ref: Vec<&std::ffi::OsStr> = args.iter().map(AsRef::as_ref).collect();

            let result = external::run(&ffmpeg, &args_ref, chapters_dir).and_then(|_| {
                fs::rename(&tmp, &out_abs).path_ctx("出力 mp4 への rename", &out_abs)
            });
            if let Err(err) = result {
                let _ = fs::remove_file(&tmp);
                return Err(err);
            }

            let edl = out_abs.with_extension("edl");
            fs::write(&edl, &markers.edl).path_ctx("EDL の書き出し", &edl)?;
            Ok(MarkOutcome {
                video: Some(out_abs),
                edl,
                cm_count: markers.cm_count,
            })
        }
    }
}

/// `-o` の比較用の絶対パス。存在するなら正規化し、無ければ親ディレクトリを正規化してファイル名を足す。
fn output_identity(out: &Path) -> Result<PathBuf> {
    if out.exists() {
        return fs::canonicalize(out).path_ctx("-o の絶対パス解決", out);
    }
    let parent = match out.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let parent = fs::canonicalize(parent).path_ctx("-o の親ディレクトリの解決", parent)?;
    let name = out
        .file_name()
        .ok_or_else(|| anyhow!("-o からファイル名を取れません: {}", out.display()))?;
    Ok(parent.join(name))
}

/// キャッシュのファイルを読む。無ければ `analyze` のコマンド例を添えて止める。
fn read_cached(path: &Path, analyze_cmd: &dyn Fn() -> String) -> Result<String> {
    if !path.is_file() {
        bail!(
            "キャッシュに必要なファイルが見つかりませんでした: {}\n\
             先に次を実行してください:\n  {}",
            path.display(),
            analyze_cmd()
        );
    }
    fs::read_to_string(path).path_ctx("キャッシュファイルの読み込み", path)
}

/// `.dtvi` ヘッダから time_base を整数で読む。0 以下は不正とする。
fn time_base(dtvi: &Dtvi) -> Result<(i64, i64), MarkerError> {
    let read = |key: &'static str| {
        let raw = dtvi.header_value(key);
        raw.and_then(|v| v.trim().parse::<i64>().ok())
            .filter(|&v| v > 0)
            .ok_or_else(|| MarkerError::InvalidTimeBase {
                key,
                value: raw.map(str::to_owned),
            })
    };
    Ok((read("time_base_num")?, read("time_base_den")?))
}

/// Trim の補集合まで含めて `[0, frame_count)` を隙間なく覆う区間列を作る。
fn segments(trim: &TrimList, frame_count: usize) -> Result<Vec<Segment>, MarkerError> {
    let mut out = Vec::new();
    let mut cursor = 0;
    for range in trim.ranges() {
        let start = range.start().0 as usize;
        let end = range.end().0 as usize;
        if end > frame_count {
            // end は半開なので end >= 1（start < end）。最終フレームは end - 1。
            return Err(MarkerError::TrimBeyondFrames {
                last_frame: end - 1,
                frame_count,
            });
        }
        if start > cursor {
            out.push(Segment {
                start: cursor,
                end: start,
                is_cm: true,
            });
        }
        match out.last_mut() {
            // 隣接する本編は1区間に統合する（直前が本編で、ちょうど続いている場合）。
            Some(last) if !last.is_cm && last.end == start => last.end = end,
            _ => out.push(Segment {
                start,
                end,
                is_cm: false,
            }),
        }
        cursor = end;
    }
    if cursor < frame_count {
        out.push(Segment {
            start: cursor,
            end: frame_count,
            is_cm: true,
        });
    }
    Ok(out)
}

/// 表示順の境界 `b` の時刻（time_base 単位）。`b == frame_count` は最終フレームの末端。
fn boundary_pts(dtvi: &Dtvi, b: usize) -> Result<i64, MarkerError> {
    if b == 0 {
        return Ok(0);
    }
    let n = dtvi.frames.len();
    let idx = b.min(n - 1);
    let frame = &dtvi.frames[idx];
    if !frame.has_valid_pts() {
        return Err(MarkerError::InvalidPts { frame_number: idx });
    }
    Ok(if b == n {
        frame.pts + frame.duration
    } else {
        frame.pts
    })
}

fn seconds(t: i64, num: i64, den: i64) -> f64 {
    t as f64 * num as f64 / den as f64
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dtvi::{self, DtviFrame, FLAG_KEY_PACKET, FLAG_VALID_PTS};
    use crate::order::{DecodeIdx, DisplayIdx};
    use std::collections::HashMap;

    /// 実際の `.dtvi` の抜粋（表示順40フレーム、time_base 1/30000）。
    const SAMPLE: &str = include_str!("../tests/data/sample.dtvi");

    fn sample() -> Dtvi {
        dtvi::parse(SAMPLE).expect("実データの抜粋をパースできるはず")
    }

    fn frame(n: u32, pts: i64, duration: i64) -> DtviFrame {
        DtviFrame {
            frame_number: DisplayIdx(n),
            sample_number: DecodeIdx(n),
            random_access_sample: DecodeIdx(0),
            file_offset: 0,
            pts,
            dts: pts,
            duration,
            flags: FLAG_KEY_PACKET | FLAG_VALID_PTS,
        }
    }

    /// `frames` と time_base `1/1000` を持つ手組みの `.dtvi`。
    fn synthetic(frames: Vec<DtviFrame>) -> Dtvi {
        let header = HashMap::from([
            ("time_base_num".to_string(), "1".to_string()),
            ("time_base_den".to_string(), "1000".to_string()),
        ]);
        Dtvi {
            format_version: 1,
            header,
            frames,
        }
    }

    /// 表示順 `i` のフレームが pts `i*10`、duration 10 の10フレーム。
    fn ten_frames() -> Dtvi {
        synthetic((0..10).map(|i| frame(i, i as i64 * 10, 10)).collect())
    }

    fn trim(s: &str) -> TrimList {
        TrimList::parse(s).expect("手書きの Trim はパースできるはず")
    }

    #[test]
    fn boundaries_use_display_order_not_decode_order() {
        // 表示順1はデコード順2、表示順3はデコード順1。デコード順で引くと pts がずれる。
        let dtvi = sample();
        assert_eq!(dtvi.frames[1].sample_number, DecodeIdx(2));
        let m = markers(&trim("Trim(1,9)"), &dtvi).unwrap();

        // 保持は表示順 [1, 10)。CM は [0,1) と [10,40)。
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/30000\nSTART=0\nEND=1001\ntitle=CM\n\
             [CHAPTER]\nTIMEBASE=1/30000\nSTART=1001\nEND=10010\ntitle=本編\n\
             [CHAPTER]\nTIMEBASE=1/30000\nSTART=10010\nEND=40040\ntitle=CM\n"
        );
        assert_eq!(m.cm_count, 2);
    }

    #[test]
    fn edl_lists_only_cm_sections_with_seconds() {
        let m = markers(&trim("Trim(1,9)"), &sample()).unwrap();
        // 1001/30000 = 0.03337 → 0.033、10010/30000 = 0.33367 → 0.334、40040/30000 → 1.335
        assert_eq!(m.edl, "0.000\t0.033\t3\n0.334\t1.335\t3\n");
    }

    #[test]
    fn no_cm_when_trim_covers_everything() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(0,9)"), &dtvi).unwrap();
        assert_eq!(m.edl, "");
        assert_eq!(m.cm_count, 0);
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=100\ntitle=本編\n"
        );
    }

    #[test]
    fn trim_starting_at_zero_has_no_leading_cm() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(0,4)"), &dtvi).unwrap();
        assert_eq!(m.cm_count, 1);
        assert_eq!(m.edl, "0.050\t0.100\t3\n");
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=50\ntitle=本編\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=50\nEND=100\ntitle=CM\n"
        );
    }

    #[test]
    fn adjacent_trims_merge_into_one_section() {
        // Trim(0,4) = [0,5), Trim(5,9) = [5,10) は途切れない1区間として扱う。
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(0,4) ++ Trim(5,9)"), &dtvi).unwrap();
        assert_eq!(m.cm_count, 0);
        assert_eq!(m.edl, "");
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=100\ntitle=本編\n"
        );
    }

    #[test]
    fn adjacent_trims_after_leading_cm_merge_into_one_section() {
        // CM [0,2), 本編 [2,5)+[5,7) = [2,7), CM [7,10)
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(2,4) ++ Trim(5,6)"), &dtvi).unwrap();
        assert_eq!(m.cm_count, 2);
        assert_eq!(m.edl, "0.000\t0.020\t3\n0.070\t0.100\t3\n");
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=20\ntitle=CM\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=20\nEND=70\ntitle=本編\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=70\nEND=100\ntitle=CM\n"
        );
    }

    #[test]
    fn trim_running_to_last_frame_ends_at_pts_plus_duration() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(3,9)"), &dtvi).unwrap();
        assert_eq!(m.cm_count, 1);
        assert_eq!(m.edl, "0.000\t0.030\t3\n");
        assert_eq!(
            m.ffmetadata,
            ";FFMETADATA1\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=30\ntitle=CM\n\
             [CHAPTER]\nTIMEBASE=1/1000\nSTART=30\nEND=100\ntitle=本編\n"
        );
    }

    #[test]
    fn multiple_trims_alternate_sections() {
        let dtvi = ten_frames();
        let m = markers(&trim("Trim(1,2) ++ Trim(5,6)"), &dtvi).unwrap();
        // CM [0,1) 本編 [1,3) CM [3,5) 本編 [5,7) CM [7,10)
        assert_eq!(m.cm_count, 3);
        assert_eq!(m.edl, "0.000\t0.010\t3\n0.030\t0.050\t3\n0.070\t0.100\t3\n");
        let titles: Vec<&str> = m
            .ffmetadata
            .lines()
            .filter_map(|l| l.strip_prefix("title="))
            .collect();
        assert_eq!(titles, ["CM", "本編", "CM", "本編", "CM"]);
    }

    #[test]
    fn missing_time_base_is_an_error() {
        let mut dtvi = ten_frames();
        dtvi.header.remove("time_base_den");
        assert!(matches!(
            markers(&trim("Trim(0,4)"), &dtvi),
            Err(MarkerError::InvalidTimeBase {
                key: "time_base_den",
                value: None
            })
        ));
    }

    #[test]
    fn zero_or_non_integer_time_base_is_an_error() {
        for bad in ["0", "1.5", "abc"] {
            let mut dtvi = ten_frames();
            dtvi.header.insert("time_base_num".into(), bad.into());
            assert!(
                matches!(
                    markers(&trim("Trim(0,4)"), &dtvi),
                    Err(MarkerError::InvalidTimeBase {
                        key: "time_base_num",
                        ..
                    })
                ),
                "{bad} は拒否されるべき"
            );
        }
    }

    #[test]
    fn trim_beyond_frame_count_is_an_error() {
        let dtvi = ten_frames();
        assert_eq!(
            markers(&trim("Trim(5,10)"), &dtvi),
            Err(MarkerError::TrimBeyondFrames {
                last_frame: 10,
                frame_count: 10
            })
        );
    }

    #[test]
    fn trim_beyond_frame_count_message_uses_trim_avs_notation() {
        let err = markers(&trim("Trim(5,10)"), &ten_frames()).unwrap_err();
        assert_eq!(
            err.to_string(),
            "Trim の最終フレーム 10 が .dtvi のフレーム範囲 0..=9 を超えている"
        );
    }

    #[test]
    fn invalid_pts_on_boundary_frame_is_an_error() {
        let mut dtvi = ten_frames();
        dtvi.frames[3].flags &= !FLAG_VALID_PTS;
        assert_eq!(
            markers(&trim("Trim(3,9)"), &dtvi),
            Err(MarkerError::InvalidPts { frame_number: 3 })
        );
    }
}
