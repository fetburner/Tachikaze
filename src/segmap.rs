//! `cut` が計算した区間マップ（snap 後の境界と出力タイムライン上の開始時刻）の
//! 構造体と JSON への書き出し。
//!
//! ## 何のためにあるか
//!
//! `cut` は区間ごとの「ソース上の開始 DTS」と「出力側の長さ」を内部で計算しているが
//! （[`crate::commands`] の `segment_video_source_starts` / `segment_video_durations`）、
//! 従来はレポート表示にしか使わず外に出していなかった。外部で作った字幕やチャプターを
//! cut 後のタイムラインに合わせるにはこのデータが要る。**`trim.avs` から再計算しては
//! いけない**: キーフレーム丸め（snap）のぶん、境界あたり平均 2.1〜2.5 秒ずれる。
//! snap 後の値を知っているのは `cut` だけなので、ここでは受け取った値をそのまま
//! 記録するだけにする（再計算・再導出はしない）。
//!
//! ## なぜ JSON を手書きするか（依存を増やさない）
//!
//! スキーマは固定で小さく（数値主体 + パス文字列 1 個 + その配列）、消費側もこの
//! クレート外（将来の字幕張り替え）になる想定のため、書き出し専用でよい。`serde` /
//! `serde_json` を追加する選択肢もあったが、このプロジェクトは `.dtvi`（`dtvi.rs`）や
//! trim リスト（`trim.rs`）など自前フォーマットのパーサを既に手書きしており、
//! 「小さく固定されたスキーマは自前で書く」という既存の方針に合わせた。読み込みが
//! 必要になった時点（次に読む側が Rust であれば）で改めて依存を検討する。
//!
//! ## 罠4がここにも効く
//!
//! [`Segment::source_start_dts`] は **PTS/合成時刻ではなく DTS**。
//! `crate::commands::segment_video_source_starts` の doc comment に理由がある
//! （CLAUDE.md の罠4）。ここで pts に戻すと、消費側で字幕が `cts_offset` ぶん先行する。

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use crate::plan::SnappedRange;

/// 1 保持区間分のマップ情報。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    /// 表示順、snap 後、半開区間の開始（含む）。
    pub source_start_frame: u32,
    /// 表示順、snap 後、半開区間の終了（含まない）。
    pub source_end_frame: u32,
    /// この区間の先頭サンプルの、ソース上の絶対 DTS（映像 timescale 単位）。
    /// PTS ではない理由は本モジュールの doc comment 参照。
    pub source_start_dts: u64,
    /// `source_end_frame - source_start_frame`。
    pub frame_count: u32,
    /// 出力タイムライン上の開始（映像 timescale 単位、それ以前の区間の長さの累積）。
    pub output_start: u64,
    /// この区間の出力側の長さ（映像 timescale 単位）。
    pub duration: u64,
}

/// `cut` が書き出す区間マップ全体。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SegmentMap {
    /// `source_start_dts` / `output_start` / `duration` が使っている単位（映像トラックの
    /// timescale）。秒への変換は消費側の責務（このモジュールでは行わない）。
    pub video_timescale: u32,
    /// `.dtvi` の `frame_rate_num`（無ければ既定値。[`crate::commands::fps_from_dtvi`] と
    /// 同じ既定値を使う）。
    pub frame_rate_num: u32,
    /// `.dtvi` の `frame_rate_den`。
    pub frame_rate_den: u32,
    /// 入力 mp4 のパス（可能なら絶対パス。呼び出し側で解決する）。
    pub input: PathBuf,
    /// 入力の総フレーム数（映像トラックの総サンプル数）。
    pub total_frames: u32,
    /// 保持区間ごとのマップ情報。`snapped` と同じ順序。
    pub segments: Vec<Segment>,
}

impl SegmentMap {
    /// `cut` が既に計算済みの区間情報から `SegmentMap` を組み立てる。
    ///
    /// `snapped` / `source_starts` / `durations` は同じ長さ・同じ順序であることを
    /// 呼び出し側が保証する（`crate::commands::CutPipeline::run` が実際に計算する値を
    /// そのまま渡す想定で、ここでは再計算しない）。
    #[allow(clippy::too_many_arguments)]
    pub fn build(
        snapped: &[SnappedRange],
        source_starts: &[u64],
        durations: &[u64],
        video_timescale: u32,
        frame_rate_num: u32,
        frame_rate_den: u32,
        input: PathBuf,
        total_frames: u32,
    ) -> SegmentMap {
        assert_eq!(
            snapped.len(),
            source_starts.len(),
            "snapped と source_starts の長さが一致しません"
        );
        assert_eq!(
            snapped.len(),
            durations.len(),
            "snapped と durations の長さが一致しません"
        );

        let mut segments = Vec::with_capacity(snapped.len());
        let mut output_cursor: u64 = 0;
        for ((range, &source_start_dts), &duration) in
            snapped.iter().zip(source_starts).zip(durations)
        {
            let frame_count = range.end.snapped.0 - range.start.snapped.0;
            segments.push(Segment {
                source_start_frame: range.start.snapped.0,
                source_end_frame: range.end.snapped.0,
                source_start_dts,
                frame_count,
                output_start: output_cursor,
                duration,
            });
            output_cursor += duration;
        }

        SegmentMap {
            video_timescale,
            frame_rate_num,
            frame_rate_den,
            input,
            total_frames,
            segments,
        }
    }

    /// このマップを JSON 文字列にする。フォーマットの選定理由は本モジュールの
    /// doc comment 参照。
    pub fn to_json(&self) -> String {
        let mut out = String::new();
        out.push_str("{\n");
        out.push_str(&format!(
            "  \"video_timescale\": {},\n",
            self.video_timescale
        ));
        out.push_str(&format!("  \"frame_rate_num\": {},\n", self.frame_rate_num));
        out.push_str(&format!("  \"frame_rate_den\": {},\n", self.frame_rate_den));
        out.push_str(&format!(
            "  \"input\": \"{}\",\n",
            json_escape(&self.input.to_string_lossy())
        ));
        out.push_str(&format!("  \"total_frames\": {},\n", self.total_frames));
        out.push_str("  \"segments\": [\n");
        for (i, seg) in self.segments.iter().enumerate() {
            out.push_str("    {\n");
            out.push_str(&format!(
                "      \"source_start_frame\": {},\n",
                seg.source_start_frame
            ));
            out.push_str(&format!(
                "      \"source_end_frame\": {},\n",
                seg.source_end_frame
            ));
            out.push_str(&format!(
                "      \"source_start_dts\": {},\n",
                seg.source_start_dts
            ));
            out.push_str(&format!("      \"frame_count\": {},\n", seg.frame_count));
            out.push_str(&format!("      \"output_start\": {},\n", seg.output_start));
            out.push_str(&format!("      \"duration\": {}\n", seg.duration));
            let is_last = i + 1 == self.segments.len();
            out.push_str(if is_last { "    }\n" } else { "    },\n" });
        }
        out.push_str("  ]\n");
        out.push_str("}\n");
        out
    }

    /// `path` へ JSON を書き出す。親ディレクトリが無ければ作る。
    ///
    /// 呼び出し側（`crate::commands::run_cut`）の方針: このマップは再生成できる
    /// キャッシュなので、書き込み失敗は検証済みの mp4 を破棄する理由にはしない。
    /// ただし黙って省略もしない（呼び出し側で警告する）。この関数自体は素直に
    /// `io::Result` を返すだけで、失敗時の扱いは呼び出し側に委ねる。
    pub fn write_to_file(&self, path: &Path) -> io::Result<()> {
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)?;
            }
        }
        fs::write(path, self.to_json())
    }
}

/// JSON 文字列リテラルとして安全な形にエスケープする。
///
/// 汎用 JSON エスケープではなく、このモジュールが書き出す値（ファイルパス）に必要な
/// 分だけ（引用符・バックスラッシュ・制御文字）を扱う。
fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::order::DisplayIdx;
    use crate::plan::SnappedBoundary;

    fn range(start: u32, end: u32) -> SnappedRange {
        SnappedRange {
            start: SnappedBoundary {
                original: DisplayIdx(start),
                snapped: DisplayIdx(start),
                delta_frames: 0,
            },
            end: SnappedBoundary {
                original: DisplayIdx(end),
                snapped: DisplayIdx(end),
                delta_frames: 0,
            },
        }
    }

    #[test]
    fn build_computes_output_start_as_cumulative_duration() {
        let snapped = vec![range(0, 120), range(360, 480)];
        let source_starts = vec![0u64, 360_360u64];
        let durations = vec![120_120u64, 120_120u64];

        let map = SegmentMap::build(
            &snapped,
            &source_starts,
            &durations,
            90_000,
            30000,
            1001,
            PathBuf::from("/tmp/IN.mp4"),
            599,
        );

        assert_eq!(map.segments.len(), 2);

        assert_eq!(map.segments[0].source_start_frame, 0);
        assert_eq!(map.segments[0].source_end_frame, 120);
        assert_eq!(map.segments[0].frame_count, 120);
        assert_eq!(map.segments[0].source_start_dts, 0);
        assert_eq!(map.segments[0].output_start, 0);
        assert_eq!(map.segments[0].duration, 120_120);

        assert_eq!(map.segments[1].source_start_frame, 360);
        assert_eq!(map.segments[1].source_end_frame, 480);
        assert_eq!(map.segments[1].frame_count, 120);
        assert_eq!(map.segments[1].source_start_dts, 360_360);
        // 2区間目の output_start は1区間目の duration の累積(120_120)から始まる。
        assert_eq!(map.segments[1].output_start, 120_120);
        assert_eq!(map.segments[1].duration, 120_120);
    }

    #[test]
    fn build_frame_count_sum_matches_total_kept_frames() {
        // 「区間数と frame_count の合計が保持側の総フレーム数(≒ VerifyReport の
        // video_packet_count)と一致する」ことを、合成データで固定する。実フィクスチャを
        // 使った同種の確認は commands.rs 側の統合テストで行う(cut_and_verify が返す
        // VerifyReport との突き合わせにはフィクスチャの外部依存があるため)。
        let snapped = vec![range(0, 120), range(360, 480), range(480, 599)];
        let source_starts = vec![0u64, 360_360u64, 480_480u64];
        let durations = vec![120_120u64, 120_120u64, 119_119u64];

        let map = SegmentMap::build(
            &snapped,
            &source_starts,
            &durations,
            90_000,
            30000,
            1001,
            PathBuf::from("/tmp/IN.mp4"),
            599,
        );

        let frame_count_sum: u32 = map.segments.iter().map(|s| s.frame_count).sum();
        assert_eq!(frame_count_sum, 120 + 120 + 119);
        assert_eq!(map.segments.len(), snapped.len());
    }

    #[test]
    fn to_json_contains_header_and_segment_fields() {
        let snapped = vec![range(0, 120)];
        let source_starts = vec![42u64];
        let durations = vec![120_120u64];

        let map = SegmentMap::build(
            &snapped,
            &source_starts,
            &durations,
            90_000,
            30000,
            1001,
            PathBuf::from("/tmp/IN.mp4"),
            599,
        );

        let json = map.to_json();
        assert!(json.contains("\"video_timescale\": 90000"));
        assert!(json.contains("\"frame_rate_num\": 30000"));
        assert!(json.contains("\"frame_rate_den\": 1001"));
        assert!(json.contains("\"total_frames\": 599"));
        assert!(json.contains("\"input\": \"/tmp/IN.mp4\""));
        assert!(json.contains("\"source_start_frame\": 0"));
        assert!(json.contains("\"source_end_frame\": 120"));
        assert!(json.contains("\"source_start_dts\": 42"));
        assert!(json.contains("\"frame_count\": 120"));
        assert!(json.contains("\"output_start\": 0"));
        assert!(json.contains("\"duration\": 120120"));
    }

    #[test]
    fn to_json_escapes_quotes_and_backslashes_in_input_path() {
        let map = SegmentMap::build(
            &[],
            &[],
            &[],
            90_000,
            30000,
            1001,
            PathBuf::from("/tmp/weird\"name\\dir/IN.mp4"),
            0,
        );

        let json = map.to_json();
        assert!(json.contains("\\\"name\\\\dir"));
        assert!(json.contains("\"segments\": [\n  ]\n".trim_start()) || json.contains("[\n  ]"));
    }

    #[test]
    fn write_to_file_creates_parent_directory() {
        let dir = std::env::temp_dir().join(format!(
            "tachikaze-segmap-test-{}-{}",
            std::process::id(),
            "write-creates-parent"
        ));
        let _ = fs::remove_dir_all(&dir);
        let path = dir.join("nested").join("work.mp4.segmap.json");

        let map = SegmentMap::build(
            &[],
            &[],
            &[],
            90_000,
            30000,
            1001,
            PathBuf::from("IN.mp4"),
            0,
        );
        map.write_to_file(&path)
            .expect("親ディレクトリを作って書けるはず");

        assert!(path.is_file());
        let content = fs::read_to_string(&path).expect("読み戻せるはず");
        assert!(content.contains("\"segments\": [\n  ]\n") || content.contains("[\n  ]"));

        let _ = fs::remove_dir_all(&dir);
    }

    /// 実フィクスチャ(GOP=120・599フレーム)を使い、`SegmentMap::build` に渡す
    /// `snapped` / `source_starts` / `durations` を `crate::commands` と同じ手順
    /// (`plan::snap` → `plan::keep_list`)で組み立てたときに、frame_count の合計が
    /// 保持パケット数と一致することを確認する。
    /// (完了条件: 「区間数と frame_count の合計が VerifyReport の映像パケット数と
    /// 一致する単体テスト」。VerifyReport 自体は `verify::cut_and_verify` を通した
    /// テストで `report.video_packet_count == video_keep.len()` であることが
    /// 既に確認されている。)
    #[test]
    fn build_frame_count_sum_matches_real_fixture_keep_list_len() {
        use crate::cli::Snap;
        use crate::mp4io::order_map::DisplayDecodeMap;
        use crate::mp4io::read::{find_video_track, read_moov, samples};
        use crate::plan;
        use crate::trim::TrimList;

        const FIXTURE: &str = "tests/fixtures/sample.mp4";
        if !Path::new(FIXTURE).exists() {
            eprintln!(
                "{FIXTURE} が無いためスキップします。`tests/fixtures/gen.sh` を実行してください。"
            );
            return;
        }

        let input_path = PathBuf::from(FIXTURE);
        let moov = read_moov(&input_path).expect("moov を読めること");
        let (video_trak, video_info) = find_video_track(&moov).expect("映像トラックが見つかること");
        let video_samples = samples(&video_trak.mdia.minf.stbl);
        let total_frames = video_samples.len() as u32;

        let map = DisplayDecodeMap::build(&video_samples).expect("同値の合成時刻は無いはず");
        let sync_display = map.sync_display_indices();

        let trim =
            TrimList::parse("Trim(10,109) ++ Trim(370,469)").expect("Trim をパースできること");
        let snapped = plan::snap(&trim, &sync_display, total_frames, Snap::Outward)
            .expect("スナップ後の区間が重ならないこと");
        let video_keep = plan::keep_list(&snapped, &map.order).expect("keep_list が成功すること");

        let mut durations = Vec::new();
        let mut source_starts = Vec::new();
        let mut cursor = 0usize;
        for r in &snapped {
            let count = (r.end.snapped - r.start.snapped) as usize;
            let duration: u64 = video_keep[cursor..cursor + count]
                .iter()
                .map(|d| u64::from(video_samples[d.0 as usize].duration))
                .sum();
            durations.push(duration);
            cursor += count;

            let decode = map
                .order
                .to_decode(r.start.snapped)
                .expect("区間開始の表示順に対応するデコード順があるはず");
            let dts = crate::mp4io::order_map::decode_timestamp(&video_samples, decode)
                .expect("デコード順が映像サンプルの範囲内のはず");
            source_starts.push(dts);
        }

        let segment_map = SegmentMap::build(
            &snapped,
            &source_starts,
            &durations,
            video_info.timescale,
            30000,
            1001,
            input_path,
            total_frames,
        );

        let frame_count_sum: u32 = segment_map.segments.iter().map(|s| s.frame_count).sum();
        assert_eq!(
            frame_count_sum as usize,
            video_keep.len(),
            "frame_count の合計は cut_and_verify が返す video_packet_count(== video_keep.len()) と \
             一致するはず"
        );
        assert_eq!(segment_map.segments.len(), snapped.len());
    }
}
