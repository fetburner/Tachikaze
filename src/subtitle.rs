//! `remap-subs` サブコマンド: 字幕サイドカーのタイムスタンプを区間マップ
//! （[`crate::segmap`]）で cut 後のタイムラインへ張り替える。
//!
//! ## 何が問題か
//!
//! `cut` は snap 後の Trim 区間だけを連結して出力する。区間マップと字幕サイドカー
//! （[`crate::prepare`]）はどちらも既にあるが、両者を繋ぐものが無かった。
//! 字幕の時刻を元ファイルのまま出力に付けると、「そこまでに除去した CM の累積時間」
//! ぶんずれる（末尾に行くほどずれが大きくなり、先頭だけ合っているので一見動いている
//! ように見える）。
//!
//! ## 写像の式（CLAUDE.md 罠4はここにも効く）
//!
//! 区間マップの各区間 `k` について:
//!
//! ```text
//! output_t = output_start_k + (source_t - source_start_dts_k)
//! ```
//!
//! `source_start_dts_k` は [`crate::segmap::Segment::source_start_dts`]（映像 timescale
//! 単位、**DTS**。[`crate::commands::segment_video_source_starts`] の doc comment 参照）。
//! `output_start_k` は同区間の [`crate::segmap::Segment::output_start`]。この式は
//! 「区間内は無劣化コピーで再エンコードしない、つまりソースと出力で時間の進み方が
//! 1:1」という前提から成り立つ（区間ごとに定数シフトするだけ）。字幕の時刻を
//! 一度も合成時刻（PTS）に変換しない・区間マップの `source_start_dts` を PTS に
//! 戻さないこと（cts_offset ぶんずれる、CLAUDE.md 罠4と同型の誤り）。
//!
//! ## イベントの分類（3種）
//!
//! 各字幕イベント（開始・終了時刻の組）を、それが指す区間マップ上の位置で分類する:
//!
//! - **シフト**: 保持区間 1 個に完全に含まれる → 上の式でそのままシフト
//! - **破棄**: どの保持区間とも重ならない（除去区間＝CMに完全に含まれる） →
//!   出力しない。**シフトして残すと CM の字幕が本編に混ざる**ので、重ならない
//!   イベントは必ず捨てる
//! - **クリップ**: 保持区間の境界を跨ぐ（開始または終了が除去区間側にはみ出す）
//!   → 破棄せず、はみ出した側を保持区間の境界にクランプしてからシフトする
//!
//! ### 除去区間を挟んで複数の保持区間に跨る場合（クリップ、分割はしない）
//!
//! イベントが除去区間をまたいで 2 つ以上の保持区間に重なる場合（CM 跨ぎの字幕）は、
//! **先頭（最初に重なった）保持区間にクリップする**方針にした。分割（区間ごとに
//! イベントを複製し、それぞれの重なり部分だけを残す）も検討したが採らなかった理由:
//!
//! 1. **相対タイミングタグとの相性が悪い**（下記「罠」参照）。分割後の2つ目以降の
//!    イベントは開始時刻が変わるため、`\move` / `\t` / `\fad` / `\k` の基準時刻が
//!    実質的に破綻する。クリップなら基準時刻（開始）は変わらない場合が多い
//!    （outward snap では保持区間が Trim の上位集合なので、開始側のクリップ自体が
//!    稀。下記「罠」参照）
//! 2. **outward snap では実際に起きにくい**（保持区間が Trim の上位集合になるため、
//!    CM を跨ぐ長さの字幕がある場合のみ発生する稀なケース）
//!
//! 稀なケースなので黙って処理を続けるのではなく、この場合は必ず警告を出す
//! （[`RemapStats::warnings`]、`spans_multiple_segments` 相当）。
//!
//! ## 罠: 相対タイミングタグと開始側クリップ
//!
//! ASS のイベント内の相対タイミングタグ（`\move` / `\t` / `\fad` / `\k` 系）は
//! **イベントの開始時刻を基準**にする。開始側をクリップする（開始時刻を後ろへ
//! ずらす）と、タグが指す相対時刻の意味がずれる。この実装では次の方針にした:
//! **クランプはする（クリップ自体は行う。破棄すると余計に情報を失う）。ただし
//! 相対タイミングタグを含むイベントの開始側をクリップした場合だけ警告する**
//! （クランプして良いかどうかをタグの種類ごとに正しく補正する完全な実装は、
//! ASS のオーバーライドタグ全体を解釈するエンジンが要るため対象外）。
//! **outward snap（既定）では保持区間が Trim の上位集合になるため、開始側の
//! クリップ自体が稀**（docs/lossless-cut.md「補集合は追加のスナップを
//! 必要としない」節）。稀だからこそ黙って進めず、起きたときは必ず警告する。
//!
//! ## 罠: ASS の時刻の丸め方向
//!
//! ASS の時刻は `h:mm:ss.cc`（10ms = センチ秒量子化）。区間マップの ticks
//! （映像 timescale 単位）はセンチ秒の整数倍とは限らないため、書き出し時に
//! 丸めが要る。**開始は floor、終了は ceil** にする（このモジュールの
//! `ticks_to_units_floor` / `ticks_to_units_ceil`）。逆にすると、量子化により
//! 実際より短く表示される（1 フレーム未満の欠けが生じる）ことがある。floor/ceil
//! なら常に「元の区間を覆う」側に丸まるので欠けない。SRT（ミリ秒）も同じ方向で
//! 丸める。
//!
//! ## 罠: `source_t` は区間マップの timescale 単位。fps を仮定しない
//!
//! ASS/SRT の時刻（センチ秒・ミリ秒）は区間マップのヘッダの `video_timescale`
//! （`.dtvi` の実測値、例 90000 や 30000）でticksへ変換する。`30000/1001fps` 等の
//! フレームレートから frame 番号経由で変換する実装は書かない（区間マップの
//! ticks は時刻そのものであり、フレーム境界と必ずしも一致しない）。
//!
//! ## 罠: `Format:` 行の列順は固定ではない
//!
//! ASS の `[Events]` セクションの `Format:` 行がフィールドの列順を定義する
//! （`Layer, Start, End, Style, ...` のような並びは配布元により異なる）。
//! `Start` / `End` の列位置は必ず `Format:` 行から読み、決め打ちしない。
//!
//! ## 出力が空になる場合
//!
//! 全イベントが除去区間（CM）に完全に含まれ、シフト・クリップが 1 件も無い
//! 場合でも、**ファイルは書く**（ヘッダだけの ASS、または空の SRT）。理由:
//! 呼び出し側（`commands::run_remap_subs`）が「常に決まったパスにファイルを
//! 書く」という単純な契約を保てる方が、`-o PATH` の有無で存在しないことがある
//! 出力を扱わせるより事故が少ない。件数が 0 件であることは
//! [`RemapStats`]（シフト/破棄/クリップの内訳）で必ずログに出るため、
//! 「黙って空になる」わけではない。
//!
//! ## バイト列の保持
//!
//! 差分が時刻の行だけになるよう、次を保つ: 文字コード（UTF-8 BOM の有無）、
//! 改行コード（行ごとに元のものを再利用。CRLF/LF 混在ファイルでも行単位で保つ）、
//! 時刻フィールド以外のフィールド・行はそのまま素通しする（`Format:` 行や
//! スタイル定義、`[Script Info]` 等の他セクションも含む）。

use std::fmt;
use std::path::Path;

use crate::segmap::Segment;

/// 字幕サイドカーの形式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SubsFormat {
    Ass,
    Srt,
}

impl SubsFormat {
    /// サイドカーファイルの拡張子（`.` なし）。`workdir::subs_path` の
    /// 呼び出し規則（拡張子を渡す）と対称にしてある。
    pub fn extension(self) -> &'static str {
        match self {
            SubsFormat::Ass => "ass",
            SubsFormat::Srt => "srt",
        }
    }

    /// ファイルの拡張子から形式を判定する。大文字小文字を区別しない。`ssa`
    /// （ASS の前身、同じ `Dialogue:`/`Format:` 構造）は ASS として扱う。
    pub fn from_extension(ext: &str) -> Option<SubsFormat> {
        match ext.to_ascii_lowercase().as_str() {
            "ass" | "ssa" => Some(SubsFormat::Ass),
            "srt" => Some(SubsFormat::Srt),
            _ => None,
        }
    }

    /// パスの拡張子から形式を判定する。
    pub fn from_path(path: &Path) -> Option<SubsFormat> {
        let ext = path.extension()?.to_str()?;
        SubsFormat::from_extension(ext)
    }
}

/// 字幕リマップの失敗を表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubtitleError {
    /// 区間マップの値がこのモジュールの前提を満たさない（例: `video_timescale`
    /// が 0 で ticks⇔秒の変換が定義できない）。
    InvalidSegmentMap(String),
    /// 字幕ファイル中の時刻フィールドがパースできない。`context` に該当行の
    /// 特定に使える情報（`Dialogue:` 行の内容や SRT のタイミング行）を入れる。
    InvalidTimestamp { context: String, value: String },
}

impl fmt::Display for SubtitleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SubtitleError::InvalidSegmentMap(msg) => write!(f, "区間マップが不正です: {msg}"),
            SubtitleError::InvalidTimestamp { context, value } => {
                write!(f, "時刻をパースできません（{context}）: {value:?}")
            }
        }
    }
}

impl std::error::Error for SubtitleError {}

/// シフト/破棄/クリップの件数と、処理中に出た警告。
///
/// 「何件シフト/破棄/クリップしたか」を必ずログに出すための集計。呼び出し側
/// （`commands::run_remap_subs`）がこれを見て標準出力/標準エラーに出す。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RemapStats {
    pub shifted: usize,
    pub discarded: usize,
    pub clipped: usize,
    /// 相対タイミングタグを持つイベントの開始側クリップ、複数区間に跨るイベント
    /// のクリップなど、黙って進めるべきではない事象の説明。
    pub warnings: Vec<String>,
}

/// `remap_ass` / `remap_srt` の戻り値。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemapOutput {
    /// 張り替え後の字幕ファイルの内容（BOM・改行・時刻以外のフィールドは元のまま）。
    pub content: String,
    pub stats: RemapStats,
}

/// 1 イベント（開始・終了時刻）を区間マップに当てはめた結果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EventFate {
    /// どの保持区間とも重ならない（除去区間に完全に含まれる）。
    Discarded,
    /// 保持区間 1 個に完全に含まれる。出力タイムライン上の開始・終了（ticks）。
    Shifted { start: u64, end: u64 },
    /// 保持区間の境界にクランプした。出力タイムライン上の開始・終了（ticks）。
    Clipped {
        start: u64,
        end: u64,
        start_clamped: bool,
        end_clamped: bool,
        /// 除去区間を挟んで後続の保持区間にも重なっていた（クリップ先は先頭の
        /// 区間のみ、後続分は失われる。必ず警告する）。
        spans_multiple_segments: bool,
    },
}

/// イベント `[start, end)`（区間マップと同じ timescale 単位の ticks）を、区間マップに
/// 当てはめて分類する。
///
/// `segments` は `SegmentMap::segments` の並び（ソース時刻の昇順、`SegmentMap::build`
/// が保証する）をそのまま渡す前提。重なる最初の保持区間だけを見る
/// （複数区間に跨る場合の方針は本モジュールの doc comment 参照）。
fn map_event(segments: &[Segment], start: u64, end: u64) -> EventFate {
    let hit = segments.iter().enumerate().find(|(_, seg)| {
        seg.duration > 0
            && start < seg.source_start_dts + seg.duration
            && end > seg.source_start_dts
    });

    let Some((idx, seg)) = hit else {
        return EventFate::Discarded;
    };

    let seg_start = seg.source_start_dts;
    let seg_end = seg.source_start_dts + seg.duration;
    let clamped_start = start.max(seg_start);
    let clamped_end = end.min(seg_end);
    let start_clamped = clamped_start != start;
    let end_clamped = clamped_end != end;

    let out_start = seg.output_start + (clamped_start - seg_start);
    let out_end = seg.output_start + (clamped_end - seg_start);

    if !start_clamped && !end_clamped {
        EventFate::Shifted {
            start: out_start,
            end: out_end,
        }
    } else {
        let spans_multiple_segments = end_clamped
            && segments
                .get(idx + 1)
                .is_some_and(|next| next.duration > 0 && next.source_start_dts < end);
        EventFate::Clipped {
            start: out_start,
            end: out_end,
            start_clamped,
            end_clamped,
            spans_multiple_segments,
        }
    }
}

// ---------------------------------------------------------------------
// ticks ⇔ 秒表記の変換。丸め方向の理由は本モジュールの doc comment「罠: ASS の
// 時刻の丸め方向」参照。`unit_per_sec` は ASS なら 100（センチ秒）、SRT なら
// 1000（ミリ秒）。
// ---------------------------------------------------------------------

fn ticks_to_units_floor(ticks: u64, timescale: u32, unit_per_sec: u64) -> u64 {
    (u128::from(ticks) * u128::from(unit_per_sec) / u128::from(timescale)) as u64
}

fn ticks_to_units_ceil(ticks: u64, timescale: u32, unit_per_sec: u64) -> u64 {
    let numerator = u128::from(ticks) * u128::from(unit_per_sec);
    let denominator = u128::from(timescale);
    numerator.div_ceil(denominator) as u64
}

fn units_to_ticks_floor(units: u64, timescale: u32, unit_per_sec: u64) -> u64 {
    (u128::from(units) * u128::from(timescale) / u128::from(unit_per_sec)) as u64
}

/// ASS の時刻テキスト `h:mm:ss.cc` をセンチ秒に変換する。時・分・秒・センチ秒の
/// 桁数は問わない（`cc` は2桁が普通だが決め打ちしない。`.cc` の値が 99 を超える
/// 場合は不正としてパース失敗にする）。
fn parse_ass_time(text: &str) -> Option<u64> {
    let text = text.trim();
    let (h, rest) = text.split_once(':')?;
    let (m, rest) = rest.split_once(':')?;
    let (s, cc) = rest.split_once('.')?;
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    let s: u64 = s.parse().ok()?;
    let cc: u64 = cc.parse().ok()?;
    if cc > 99 || m > 59 || s > 59 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * 100 + cc)
}

/// センチ秒を ASS の時刻テキスト `h:mm:ss.cc` に変換する（時は0埋めしない、
/// ASS の慣例どおり）。
fn format_ass_time(centiseconds: u64) -> String {
    let cc = centiseconds % 100;
    let total_seconds = centiseconds / 100;
    let s = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let m = total_minutes % 60;
    let h = total_minutes / 60;
    format!("{h}:{m:02}:{s:02}.{cc:02}")
}

/// SRT の時刻テキスト `hh:mm:ss,mmm` をミリ秒に変換する。
fn parse_srt_time(text: &str) -> Option<u64> {
    let text = text.trim();
    let (h, rest) = text.split_once(':')?;
    let (m, rest) = rest.split_once(':')?;
    let (s, ms) = rest.split_once(',')?;
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    let s: u64 = s.parse().ok()?;
    let ms: u64 = ms.parse().ok()?;
    if ms > 999 || m > 59 || s > 59 {
        return None;
    }
    Some(((h * 60 + m) * 60 + s) * 1000 + ms)
}

/// ミリ秒を SRT の時刻テキスト `hh:mm:ss,mmm` に変換する。
fn format_srt_time(millis: u64) -> String {
    let ms = millis % 1000;
    let total_seconds = millis / 1000;
    let s = total_seconds % 60;
    let total_minutes = total_seconds / 60;
    let m = total_minutes % 60;
    let h = total_minutes / 60;
    format!("{h:02}:{m:02}:{s:02},{ms:03}")
}

/// ASS のオーバーライドタグのうち、イベントの開始時刻を基準にする相対タイミング
/// タグを含むかどうかを判定する（本モジュールの doc comment「罠: 相対タイミング
/// タグと開始側クリップ」参照）。誤検出（無関係なテキストが偶然 `\k` を含む等）は
/// 「警告が多く出るだけ」で安全側なので、簡易な部分文字列判定で十分と判断した。
fn contains_relative_timing_tag(text: &str) -> bool {
    const TAGS: [&str; 5] = ["\\move(", "\\t(", "\\fad(", "\\fade(", "\\k"];
    TAGS.iter().any(|tag| text.contains(tag))
}

/// テキストを行ごとに分割し、各行の内容（終端記号を含まない）と、その行に
/// 続いていた終端記号（`"\r\n"` / `"\n"` / 終端が無い最終行なら `""`）の組を返す。
/// 改行コードを行単位で保つため（CRLF/LF 混在ファイルでも壊さない）、
/// `str::lines` ではなく手書きで実装する。
fn split_lines_preserving_terminators(body: &str) -> Vec<(&str, &str)> {
    let mut out = Vec::new();
    let mut rest = body;
    while !rest.is_empty() {
        if let Some(idx) = rest.find('\n') {
            let (line_with_cr, remainder) = rest.split_at(idx);
            let remainder = &remainder[1..];
            if let Some(line) = line_with_cr.strip_suffix('\r') {
                out.push((line, "\r\n"));
            } else {
                out.push((line_with_cr, "\n"));
            }
            rest = remainder;
        } else {
            out.push((rest, ""));
            rest = "";
        }
    }
    out
}

/// UTF-8 BOM の有無を調べ、あれば取り除いた本文を返す。
fn strip_bom(content: &str) -> (bool, &str) {
    match content.strip_prefix('\u{FEFF}') {
        Some(rest) => (true, rest),
        None => (false, content),
    }
}

/// ASS/SSA の `[Events]` セクションの `Dialogue:`/`Comment:` 行の Start/End を
/// 区間マップで張り替える。
///
/// 対応する罠・方針は本モジュールの doc comment に集約してある。ここでは概要のみ:
/// - `Format:` 行から Start/End の列位置を読む（列順を決め打ちしない）
/// - `Dialogue:`/`Comment:` 以外の行、`[Events]` 以外のセクションはそのまま通す
/// - Text（最後の列）はコンマを含みうるので `splitn` で切る（Text が最終列である
///   ことは ASS の構造上の前提。`Format:` 行の列順が変わってもこの前提自体は
///   崩れない）
pub fn remap_ass(
    content: &str,
    segments: &[Segment],
    video_timescale: u32,
) -> Result<RemapOutput, SubtitleError> {
    if video_timescale == 0 {
        return Err(SubtitleError::InvalidSegmentMap(
            "video_timescale が 0 です".to_string(),
        ));
    }

    let (has_bom, body) = strip_bom(content);
    let lines = split_lines_preserving_terminators(body);

    let mut current_section: Option<String> = None;
    let mut format_columns: Option<Vec<String>> = None;
    let mut col_start: Option<usize> = None;
    let mut col_end: Option<usize> = None;

    let mut stats = RemapStats::default();
    let mut out = String::new();
    if has_bom {
        out.push('\u{FEFF}');
    }

    for (line_number, (text, terminator)) in lines.into_iter().enumerate() {
        let trimmed = text.trim();

        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
            current_section = Some(trimmed[1..trimmed.len() - 1].to_string());
            out.push_str(text);
            out.push_str(terminator);
            continue;
        }

        let in_events = current_section.as_deref() == Some("Events");

        if in_events {
            if let Some(rest) = text.strip_prefix("Format:") {
                let columns: Vec<String> = rest.split(',').map(|c| c.trim().to_string()).collect();
                col_start = columns.iter().position(|c| c == "Start");
                col_end = columns.iter().position(|c| c == "End");
                format_columns = Some(columns);
                out.push_str(text);
                out.push_str(terminator);
                continue;
            }

            let event_prefix = if text.starts_with("Dialogue:") {
                Some("Dialogue:")
            } else if text.starts_with("Comment:") {
                Some("Comment:")
            } else {
                None
            };

            if let Some(prefix) = event_prefix {
                let rest = &text[prefix.len()..];

                let (columns, start_idx, end_idx) = match (&format_columns, col_start, col_end) {
                    (Some(cols), Some(s), Some(e)) => (cols, s, e),
                    _ => {
                        stats.warnings.push(format!(
                            "{}行目: Format行が無い、またはStart/End列が見つからないため\
                             この行をそのまま通しました",
                            line_number + 1
                        ));
                        out.push_str(text);
                        out.push_str(terminator);
                        continue;
                    }
                };

                let fields: Vec<&str> = rest.splitn(columns.len(), ',').collect();
                if fields.len() < columns.len() {
                    stats.warnings.push(format!(
                        "{}行目: フィールド数がFormat行より少ないためこの行をそのまま通しました",
                        line_number + 1
                    ));
                    out.push_str(text);
                    out.push_str(terminator);
                    continue;
                }

                let start_ticks = parse_ass_time(fields[start_idx])
                    .map(|cs| units_to_ticks_floor(cs, video_timescale, 100));
                let end_ticks = parse_ass_time(fields[end_idx])
                    .map(|cs| units_to_ticks_floor(cs, video_timescale, 100));
                let (start_ticks, end_ticks) = match (start_ticks, end_ticks) {
                    (Some(s), Some(e)) => (s, e),
                    _ => {
                        return Err(SubtitleError::InvalidTimestamp {
                            context: format!("{}行目", line_number + 1),
                            value: format!("{}/{}", fields[start_idx], fields[end_idx]),
                        });
                    }
                };

                let (new_start, new_end) = match map_event(segments, start_ticks, end_ticks) {
                    EventFate::Discarded => {
                        stats.discarded += 1;
                        continue;
                    }
                    EventFate::Shifted { start, end } => {
                        stats.shifted += 1;
                        (start, end)
                    }
                    EventFate::Clipped {
                        start,
                        end,
                        start_clamped,
                        spans_multiple_segments,
                        ..
                    } => {
                        stats.clipped += 1;
                        if start_clamped {
                            let text_field = fields[columns.len() - 1];
                            if contains_relative_timing_tag(text_field) {
                                stats.warnings.push(format!(
                                    "{}行目: 相対タイミングタグ(\\move/\\t/\\fad/\\k)を含む\
                                     イベントの開始側をクリップしました。アニメーションの\
                                     基準時刻がずれる可能性があります",
                                    line_number + 1
                                ));
                            }
                        }
                        if spans_multiple_segments {
                            stats.warnings.push(format!(
                                "{}行目: イベントが除去区間をまたいで複数の保持区間にわたって\
                                 いるため、先頭の区間にクリップしました（後続区間にかかる分は\
                                 失われます）",
                                line_number + 1
                            ));
                        }
                        (start, end)
                    }
                };

                let start_text =
                    format_ass_time(ticks_to_units_floor(new_start, video_timescale, 100));
                let end_text = format_ass_time(ticks_to_units_ceil(new_end, video_timescale, 100));

                let mut new_fields: Vec<String> = fields.iter().map(|f| f.to_string()).collect();
                new_fields[start_idx] = start_text;
                new_fields[end_idx] = end_text;

                out.push_str(prefix);
                out.push_str(&new_fields.join(","));
                out.push_str(terminator);
                continue;
            }
        }

        out.push_str(text);
        out.push_str(terminator);
    }

    Ok(RemapOutput {
        content: out,
        stats,
    })
}

/// SRT のタイミング行 `HH:MM:SS,mmm --> HH:MM:SS,mmm[任意の追加情報]` を区間マップで
/// 張り替える。
///
/// SRT はブロック（連番 + タイミング行 + テキスト行 + 空行区切り）単位の構造
/// なので、空行でブロックに分割してから処理する（`tests/prepare_e2e.rs` 等の実
/// フィクスチャで確認済みの ffmpeg 出力形式 = 連番行の直後にタイミング行、が
/// 前提。連番が無い/位置がずれた変則的な SRT でも「タイミング行を含むブロック」
/// という判定自体は崩れない）。
pub fn remap_srt(
    content: &str,
    segments: &[Segment],
    video_timescale: u32,
) -> Result<RemapOutput, SubtitleError> {
    if video_timescale == 0 {
        return Err(SubtitleError::InvalidSegmentMap(
            "video_timescale が 0 です".to_string(),
        ));
    }

    let (has_bom, body) = strip_bom(content);
    let lines = split_lines_preserving_terminators(body);
    let blocks = split_into_srt_blocks(&lines);

    let mut stats = RemapStats::default();
    let mut out = String::new();
    if has_bom {
        out.push('\u{FEFF}');
    }

    for (block_lines, blank_lines) in blocks {
        let timing_pos = block_lines.iter().position(|(t, _)| t.contains("-->"));

        let Some(timing_pos) = timing_pos else {
            for (text, term) in &block_lines {
                out.push_str(text);
                out.push_str(term);
            }
            for (text, term) in &blank_lines {
                out.push_str(text);
                out.push_str(term);
            }
            continue;
        };

        let (timing_text, timing_term) = block_lines[timing_pos];
        let (left, right) = timing_text
            .split_once("-->")
            .expect("position() で `-->` の存在を確認済み");
        let right_trimmed = right.trim_start();
        let (end_time_text, trailing) = split_time_and_trailing(right_trimmed);

        let start_ticks =
            parse_srt_time(left).map(|ms| units_to_ticks_floor(ms, video_timescale, 1000));
        let end_ticks =
            parse_srt_time(end_time_text).map(|ms| units_to_ticks_floor(ms, video_timescale, 1000));
        let (start_ticks, end_ticks) = match (start_ticks, end_ticks) {
            (Some(s), Some(e)) => (s, e),
            _ => {
                return Err(SubtitleError::InvalidTimestamp {
                    context: "タイミング行".to_string(),
                    value: timing_text.to_string(),
                });
            }
        };

        let write_block = |out: &mut String, start: u64, end: u64| {
            let start_text = format_srt_time(ticks_to_units_floor(start, video_timescale, 1000));
            let end_text = format_srt_time(ticks_to_units_ceil(end, video_timescale, 1000));
            for (i, (text, term)) in block_lines.iter().enumerate() {
                if i == timing_pos {
                    out.push_str(&start_text);
                    out.push_str(" --> ");
                    out.push_str(&end_text);
                    out.push_str(trailing);
                    out.push_str(timing_term);
                } else {
                    out.push_str(text);
                    out.push_str(term);
                }
            }
            for (text, term) in &blank_lines {
                out.push_str(text);
                out.push_str(term);
            }
        };

        match map_event(segments, start_ticks, end_ticks) {
            EventFate::Discarded => {
                stats.discarded += 1;
                for (text, term) in &blank_lines {
                    out.push_str(text);
                    out.push_str(term);
                }
            }
            EventFate::Shifted { start, end } => {
                stats.shifted += 1;
                write_block(&mut out, start, end);
            }
            EventFate::Clipped {
                start,
                end,
                spans_multiple_segments,
                ..
            } => {
                stats.clipped += 1;
                if spans_multiple_segments {
                    stats.warnings.push(format!(
                        "タイミング行 {timing_text:?}: イベントが除去区間をまたいで複数の保持区間\
                         にわたっているため、先頭の区間にクリップしました（後続区間にかかる分は\
                         失われます）"
                    ));
                }
                write_block(&mut out, start, end);
            }
        }
    }

    Ok(RemapOutput {
        content: out,
        stats,
    })
}

/// 1行の内容と、それに続く改行の終端記号（[`split_lines_preserving_terminators`]
/// の戻り値の要素）。
type Line<'a> = (&'a str, &'a str);

/// SRT の1ブロック: `(ブロック本体の行, そのブロックに続く空行群)`。
type SrtBlock<'a> = (Vec<Line<'a>>, Vec<Line<'a>>);

/// SRT の本文を、空行（1行以上連続）を区切りとしたブロックに分割する。戻り値は
/// `(ブロック本体の行, そのブロックに続く空行群)` の並び。空行の並びも保持する
/// ことで、破棄したブロックの分もおおむね元のレイアウトに近い間隔を保つ
/// （ブロックを1つ丸ごと消しても、その直後にあった空行区切りは残すことで、前後の
/// ブロックが余分にくっついたり離れたりしない）。
fn split_into_srt_blocks<'a>(lines: &[Line<'a>]) -> Vec<SrtBlock<'a>> {
    let mut blocks = Vec::new();
    let mut current: Vec<(&str, &str)> = Vec::new();
    let mut i = 0;
    while i < lines.len() {
        let (text, term) = lines[i];
        if text.trim().is_empty() {
            let mut blanks = vec![(text, term)];
            i += 1;
            while i < lines.len() && lines[i].0.trim().is_empty() {
                blanks.push(lines[i]);
                i += 1;
            }
            blocks.push((std::mem::take(&mut current), blanks));
        } else {
            current.push((text, term));
            i += 1;
        }
    }
    if !current.is_empty() {
        blocks.push((current, Vec::new()));
    }
    blocks
}

/// SRT のタイミング行の `-->` より後ろの部分から、終了時刻テキスト（先頭12文字、
/// `HH:MM:SS,mmm` 固定長）と、それに続く残り（稀に付く座標指定等。無ければ空）を
/// 切り分ける。
fn split_time_and_trailing(s: &str) -> (&str, &str) {
    const TIME_LEN: usize = "00:00:00,000".len();
    if s.len() >= TIME_LEN && s.is_char_boundary(TIME_LEN) {
        s.split_at(TIME_LEN)
    } else {
        (s, "")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `source_start_dts` から `duration` ぶんの長さを持つ保持区間を作る簡易ヘルパ。
    /// `output_start` は呼び出し側が累積計算して渡す（`SegmentMap::build` と同じ
    /// 責務分担）。
    fn segment(source_start_dts: u64, duration: u64, output_start: u64) -> Segment {
        Segment {
            source_start_frame: 0,
            source_end_frame: 0,
            source_start_dts,
            frame_count: 0,
            output_start,
            duration,
        }
    }

    // timescale=1000 とすると ticks が実質ミリ秒になり、期待値の暗算がしやすい。

    #[test]
    fn map_event_shifts_event_fully_inside_one_segment() {
        // 保持区間: source[1000,5000) -> output[0,4000)
        let segments = vec![segment(1000, 4000, 0)];
        let fate = map_event(&segments, 2000, 3000);
        assert_eq!(
            fate,
            EventFate::Shifted {
                start: 1000,
                end: 2000
            }
        );
    }

    #[test]
    fn map_event_discards_event_fully_inside_removed_region() {
        // 保持区間: source[1000,5000) -> output[0,4000)。イベントは手前のCM区間。
        let segments = vec![segment(1000, 4000, 0)];
        let fate = map_event(&segments, 0, 500);
        assert_eq!(fate, EventFate::Discarded);
    }

    #[test]
    fn map_event_discards_event_in_gap_between_two_segments() {
        let segments = vec![segment(0, 1000, 0), segment(2000, 1000, 1000)];
        // [1000,2000) はCMで除去された区間。
        let fate = map_event(&segments, 1200, 1800);
        assert_eq!(fate, EventFate::Discarded);
    }

    #[test]
    fn map_event_clips_event_straddling_start_boundary() {
        // 保持区間: source[1000,5000) -> output[0,4000)。イベントが手前から始まる。
        let segments = vec![segment(1000, 4000, 0)];
        let fate = map_event(&segments, 500, 1500);
        assert_eq!(
            fate,
            EventFate::Clipped {
                start: 0,
                end: 500,
                start_clamped: true,
                end_clamped: false,
                spans_multiple_segments: false,
            }
        );
    }

    #[test]
    fn map_event_clips_event_straddling_end_boundary() {
        let segments = vec![segment(1000, 4000, 0)];
        // 区間の終端(5000)を超えて伸びるが、後続の保持区間は無い。
        let fate = map_event(&segments, 4500, 5500);
        assert_eq!(
            fate,
            EventFate::Clipped {
                start: 3500,
                end: 4000,
                start_clamped: false,
                end_clamped: true,
                spans_multiple_segments: false,
            }
        );
    }

    #[test]
    fn map_event_clips_to_first_segment_when_spanning_removed_gap() {
        // 区間1: source[0,1000) -> output[0,1000)
        // 区間2: source[2000,3000) -> output[1000,2000)  (間の[1000,2000)がCM除去)
        let segments = vec![segment(0, 1000, 0), segment(2000, 1000, 1000)];
        // イベントが区間1の途中から区間2の途中まで、CMをまたいで続く。
        let fate = map_event(&segments, 500, 2500);
        assert_eq!(
            fate,
            EventFate::Clipped {
                start: 500,
                end: 1000,
                start_clamped: false,
                end_clamped: true,
                spans_multiple_segments: true,
            }
        );
    }

    #[test]
    fn ass_time_parse_and_format_round_trip() {
        let (h, m, s, cc): (u64, u64, u64, u64) = (1, 2, 3, 45);
        let expected_cs = ((h * 60 + m) * 60 + s) * 100 + cc;
        assert_eq!(parse_ass_time("1:02:03.45"), Some(expected_cs));
        assert_eq!(format_ass_time(expected_cs), "1:02:03.45");
    }

    #[test]
    fn srt_time_parse_and_format_round_trip() {
        let (h, m, s, ms): (u64, u64, u64, u64) = (1, 2, 3, 456);
        let expected_ms = ((h * 60 + m) * 60 + s) * 1000 + ms;
        assert_eq!(parse_srt_time("01:02:03,456"), Some(expected_ms));
        assert_eq!(format_srt_time(expected_ms), "01:02:03,456");
    }

    #[test]
    fn ticks_to_units_floor_and_ceil_bracket_non_exact_division() {
        // timescale=3, unit_per_sec=1 (1単位=3ticks とする)。ticks=4は 1.33...単位。
        assert_eq!(ticks_to_units_floor(4, 3, 1), 1);
        assert_eq!(ticks_to_units_ceil(4, 3, 1), 2);
        // ちょうど割り切れる場合は floor == ceil。
        assert_eq!(ticks_to_units_floor(6, 3, 1), 2);
        assert_eq!(ticks_to_units_ceil(6, 3, 1), 2);
    }

    // ---------------------------------------------------------------------
    // ASS: シフト/破棄/クリップの3分類を固定するテスト（完了条件）。
    // ---------------------------------------------------------------------

    /// timescale=1000（1 tick = 1ms）とし、区間マップは [0,4000)ms(=CM無し) と
    /// [5000,9000)ms(=出力[4000,8000)) の2区間。[4000,5000)msがCMで除去される。
    fn sample_segments_ms() -> Vec<Segment> {
        vec![segment(0, 4000, 0), segment(5000, 4000, 4000)]
    }

    const ASS_HEADER: &str = "[Script Info]\r\nTitle: test\r\n\r\n[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n";

    #[test]
    fn remap_ass_classifies_shift_discard_and_clip() {
        let content = format!(
            "{ASS_HEADER}\
Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,shifted\r\n\
Dialogue: 0,0:00:04.50,0:00:04.90,Default,,0,0,0,,discarded (CM)\r\n\
Dialogue: 0,0:00:03.50,0:00:05.50,Default,,0,0,0,,clipped across boundary\r\n"
        );

        let segments = sample_segments_ms();
        let result = remap_ass(&content, &segments, 1000).expect("パースできるはず");

        assert_eq!(result.stats.shifted, 1);
        assert_eq!(result.stats.discarded, 1);
        assert_eq!(result.stats.clipped, 1);

        // シフト: [1000,2000)ms -> そのまま(区間1のoutput_start=0なので変化なし)。
        assert!(result
            .content
            .contains("Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,shifted"));
        // 破棄: CM区間のイベントは出力に残らない。
        assert!(!result.content.contains("discarded (CM)"));
        // クリップ: [3500,5500)msは区間1の終端(4000ms)でクリップ -> [3500,4000)ms。
        assert!(result
            .content
            .contains("Dialogue: 0,0:00:03.50,0:00:04.00,Default,,0,0,0,,clipped across boundary"));
    }

    #[test]
    fn remap_ass_discarded_event_does_not_leak_into_output() {
        let content =
            format!("{ASS_HEADER}Dialogue: 0,0:00:04.10,0:00:04.90,Default,,0,0,0,,cm only\r\n");
        let segments = sample_segments_ms();
        let result = remap_ass(&content, &segments, 1000).expect("パースできるはず");
        assert_eq!(result.stats.discarded, 1);
        assert_eq!(result.stats.shifted, 0);
        assert_eq!(result.stats.clipped, 0);
        assert!(!result.content.contains("cm only"));
    }

    #[test]
    fn remap_ass_preserves_format_line_column_order_variation() {
        // Start/End の列位置がデフォルトと異なる(Endが先、Startが後)場合でも正しく動く。
        let content = "[Events]\r\n\
Format: Layer, End, Start, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n\
Dialogue: 0,0:00:02.00,0:00:01.00,Default,,0,0,0,,hello\r\n";
        let segments = sample_segments_ms();
        let result = remap_ass(content, &segments, 1000).expect("パースできるはず");
        assert_eq!(result.stats.shifted, 1);
        // Start(2番目のフィールド)=0:00:01.00, End(1番目のフィールド)=0:00:02.00 の並びのまま
        // 変わらない(区間1のoutput_startが0なので値そのものも変わらない)。
        assert!(result
            .content
            .contains("Dialogue: 0,0:00:02.00,0:00:01.00,Default,,0,0,0,,hello"));
    }

    #[test]
    fn remap_ass_only_time_lines_differ_from_input() {
        // 完了条件: 時刻以外のバイト列が変わっていない(差分が時刻の行だけ)。
        // シフト量が0になる(区間1がsource_start_dts=0, output_start=0)イベントを使い、
        // 「時刻の行」自体も実質バイト同一になることまで確認する。
        let content =
            format!("{ASS_HEADER}Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,unchanged\r\n");
        let segments = sample_segments_ms();
        let result = remap_ass(&content, &segments, 1000).expect("パースできるはず");
        assert_eq!(
            result.content, content,
            "シフト量0なら出力は入力と完全に一致するはず"
        );
    }

    #[test]
    fn remap_ass_preserves_bom_and_crlf() {
        let content =
            format!("\u{FEFF}{ASS_HEADER}Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,x\r\n");
        let segments = sample_segments_ms();
        let result = remap_ass(&content, &segments, 1000).expect("パースできるはず");
        assert!(result.content.starts_with('\u{FEFF}'));
        assert!(result.content.contains("\r\n"));
        assert!(!result.content.contains("\n\n")); // CRLFのままでLFに化けていないこと
    }

    #[test]
    fn remap_ass_warns_when_clipping_event_with_relative_tag_at_start() {
        // [4500,5500)ms は区間1の終端(4000ms)より後ろの除去区間から始まり、区間2
        // ([5000,9000)ms)に食い込む → 区間2にヒットし、開始側(4500ms→5000ms)がクリップ
        // される（`map_event_clips_event_straddling_start_boundary` と同じ形）。
        let content = format!(
            "{ASS_HEADER}Dialogue: 0,0:00:04.50,0:00:05.50,Default,,0,0,0,,{{\\move(0,0,100,100)}}moving\r\n"
        );
        let segments = sample_segments_ms();
        let result = remap_ass(&content, &segments, 1000).expect("パースできるはず");
        assert_eq!(result.stats.clipped, 1);
        assert!(
            result
                .stats
                .warnings
                .iter()
                .any(|w| w.contains("相対タイミングタグ")),
            "警告が出るはず: {:?}",
            result.stats.warnings
        );
    }

    #[test]
    fn remap_ass_warns_when_event_spans_multiple_kept_segments() {
        let content = format!(
            "{ASS_HEADER}Dialogue: 0,0:00:03.50,0:00:05.50,Default,,0,0,0,,spans cm break\r\n"
        );
        let segments = sample_segments_ms();
        let result = remap_ass(&content, &segments, 1000).expect("パースできるはず");
        assert_eq!(result.stats.clipped, 1);
        assert!(result
            .stats
            .warnings
            .iter()
            .any(|w| w.contains("複数の保持区間")));
    }

    #[test]
    fn remap_ass_passes_through_non_events_sections_untouched() {
        let content = "[Script Info]\r\nTitle: 何か\r\n\r\n[V4+ Styles]\r\nFormat: Name, Fontname\r\nStyle: Default,Arial\r\n\r\n[Events]\r\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\r\n";
        let segments = sample_segments_ms();
        let result = remap_ass(content, &segments, 1000).expect("パースできるはず");
        assert_eq!(
            result.content, content,
            "イベントが無ければ入力と完全一致するはず"
        );
    }

    // ---------------------------------------------------------------------
    // SRT: ASSと同じ3分類が同じ結果になることを確認する（完了条件）。
    // ---------------------------------------------------------------------

    #[test]
    fn remap_srt_classifies_shift_discard_and_clip_same_as_ass() {
        let content = "1\r\n\
00:00:01,000 --> 00:00:02,000\r\n\
shifted\r\n\
\r\n\
2\r\n\
00:00:04,500 --> 00:00:04,900\r\n\
discarded (CM)\r\n\
\r\n\
3\r\n\
00:00:03,500 --> 00:00:05,500\r\n\
clipped across boundary\r\n\
\r\n";
        let segments = sample_segments_ms();
        let result = remap_srt(content, &segments, 1000).expect("パースできるはず");

        assert_eq!(result.stats.shifted, 1);
        assert_eq!(result.stats.discarded, 1);
        assert_eq!(result.stats.clipped, 1);

        assert!(result.content.contains("00:00:01,000 --> 00:00:02,000"));
        assert!(result.content.contains("shifted"));
        assert!(!result.content.contains("discarded (CM)"));
        assert!(result.content.contains("00:00:03,500 --> 00:00:04,000"));
        assert!(result.content.contains("clipped across boundary"));
        // 破棄されたブロックの連番("2")は残らない。
        assert!(!result.content.contains("\r\n2\r\n") && !result.content.starts_with("2\r\n"));
    }

    #[test]
    fn remap_srt_only_time_lines_differ_from_input() {
        let content = "1\r\n00:00:01,000 --> 00:00:02,000\r\nunchanged\r\n\r\n";
        let segments = sample_segments_ms();
        let result = remap_srt(content, &segments, 1000).expect("パースできるはず");
        assert_eq!(result.content, content);
    }

    #[test]
    fn remap_srt_preserves_bom() {
        let content = "\u{FEFF}1\r\n00:00:01,000 --> 00:00:02,000\r\nx\r\n\r\n";
        let segments = sample_segments_ms();
        let result = remap_srt(content, &segments, 1000).expect("パースできるはず");
        assert!(result.content.starts_with('\u{FEFF}'));
    }

    #[test]
    fn remap_srt_warns_when_event_spans_multiple_kept_segments() {
        let content = "1\r\n00:00:03,500 --> 00:00:05,500\r\nspans cm break\r\n\r\n";
        let segments = sample_segments_ms();
        let result = remap_srt(content, &segments, 1000).expect("パースできるはず");
        assert_eq!(result.stats.clipped, 1);
        assert!(result
            .stats
            .warnings
            .iter()
            .any(|w| w.contains("複数の保持区間")));
    }

    #[test]
    fn remap_ass_and_remap_srt_agree_on_output_ticks_for_equivalent_events() {
        // 同じイベント(1000ms〜2000ms)をASSとSRTそれぞれで表現し、同じ出力時刻に
        // マップされることを確認する（完了条件: 「SRTでも同じ結果になるテスト」）。
        let segments = sample_segments_ms();

        let ass_content =
            format!("{ASS_HEADER}Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,x\r\n");
        let ass_result = remap_ass(&ass_content, &segments, 1000).expect("ASSがパースできるはず");

        let srt_content = "1\r\n00:00:01,000 --> 00:00:02,000\r\nx\r\n\r\n";
        let srt_result = remap_srt(srt_content, &segments, 1000).expect("SRTがパースできるはず");

        assert_eq!(ass_result.stats.shifted, 1);
        assert_eq!(srt_result.stats.shifted, 1);
        assert!(ass_result.content.contains("0:00:01.00,0:00:02.00"));
        assert!(srt_result.content.contains("00:00:01,000 --> 00:00:02,000"));
    }

    #[test]
    fn remap_ass_rejects_zero_video_timescale() {
        let content =
            format!("{ASS_HEADER}Dialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,x\r\n");
        let segments = sample_segments_ms();
        let err = remap_ass(&content, &segments, 0).expect_err("timescale=0はエラーのはず");
        assert!(matches!(err, SubtitleError::InvalidSegmentMap(_)));
    }

    #[test]
    fn subs_format_from_extension_is_case_insensitive() {
        assert_eq!(SubsFormat::from_extension("ASS"), Some(SubsFormat::Ass));
        assert_eq!(SubsFormat::from_extension("Srt"), Some(SubsFormat::Srt));
        assert_eq!(SubsFormat::from_extension("ssa"), Some(SubsFormat::Ass));
        assert_eq!(SubsFormat::from_extension("txt"), None);
    }

    #[test]
    fn subs_format_from_path_reads_extension() {
        assert_eq!(
            SubsFormat::from_path(Path::new("/tmp/subs.ass")),
            Some(SubsFormat::Ass)
        );
        assert_eq!(
            SubsFormat::from_path(Path::new("/tmp/subs.srt")),
            Some(SubsFormat::Srt)
        );
        assert_eq!(SubsFormat::from_path(Path::new("/tmp/subs")), None);
    }
}
