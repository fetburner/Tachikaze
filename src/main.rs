mod analyze;
mod audio;
mod cli;
mod dtvi;
mod external;
mod jls;
mod mp4io;
mod order;
mod plan;
mod report;
mod tools;
mod trim;
mod verify;
mod workdir;

use std::fs;
use std::path::PathBuf;

use anyhow::{anyhow, Context};
use clap::Parser;
use mp4_atom::{Codec, Moov};

use cli::{Cli, Commands};
use mp4io::read::SampleInfo;
use order::DecodeIdx;

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let tool_dir = cli.tool_dir.clone();

    match cli.command {
        Commands::Analyze {
            input,
            output,
            report,
            work_dir,
            jls_set,
            jl_file,
        } => run_analyze(tool_dir, input, output, report, work_dir, jls_set, jl_file),
        Commands::Cut {
            input,
            trim,
            output,
            snap,
            video_only,
            verify,
            dtvi,
        } => run_cut(
            tool_dir, input, trim, output, snap, video_only, verify, dtvi,
        ),
    }
}

#[allow(clippy::too_many_arguments)]
fn run_analyze(
    tool_dir: Option<PathBuf>,
    input: PathBuf,
    output: PathBuf,
    show_report: bool,
    work_dir: Option<PathBuf>,
    jls_set: Vec<String>,
    jl_file: Option<PathBuf>,
) -> anyhow::Result<()> {
    let jls_set = jls_set
        .iter()
        .map(|raw| analyze::parse_jls_set_arg(raw))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let config = analyze::AnalyzeConfig {
        input,
        output: output.clone(),
        tool_dir,
        work_dir,
        jls_set,
        jl_file,
    };

    let result = analyze::run(&config)?;
    println!("trim.avs を書き出しました: {}", output.display());

    if show_report {
        let fps = fps_from_dtvi(&result.dtvi);

        println!(
            "\n{}",
            report::format_report(&result.trim, &result.dtvi, &result.jls_entries, true)
        );

        let missed = report::missed::find_missed_candidates(&result.trim, &result.jls_entries);
        if !missed.is_empty() {
            println!("見逃し候補の警告:");
            for candidate in &missed {
                println!("{}", report::missed::format_warning(candidate, fps));
            }
        }
    }

    Ok(())
}

/// `.dtvi` ヘッダの `frame_rate_num` / `frame_rate_den` から fps を求める。
/// キーが無い、または数値としてパースできない場合は対象素材の実測値
/// （30000/1001 ≈ 29.97fps）を既定にする。
fn fps_from_dtvi(dtvi: &dtvi::Dtvi) -> f64 {
    let parse = |key: &str, default: f64| {
        dtvi.header_value(key)
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(default)
    };
    let num = parse("frame_rate_num", 30000.0);
    let den = parse("frame_rate_den", 1001.0);
    if den == 0.0 {
        num
    } else {
        num / den
    }
}

#[allow(clippy::too_many_arguments)]
fn run_cut(
    tool_dir: Option<PathBuf>,
    input: PathBuf,
    trim_path: PathBuf,
    output: PathBuf,
    snap: cli::Snap,
    video_only: bool,
    verify_with_ffprobe: bool,
    dtvi_path: Option<PathBuf>,
) -> anyhow::Result<()> {
    let moov = mp4io::read::read_moov(&input)
        .with_context(|| format!("入力 mp4 の読み込みに失敗しました: {}", input.display()))?;

    let dtvi_data = match &dtvi_path {
        Some(path) => {
            let content = fs::read_to_string(path)
                .with_context(|| format!(".dtvi の読み込みに失敗しました: {}", path.display()))?;
            Some(
                dtvi::parse(&content)
                    .map_err(|err| anyhow!(".dtvi のパースに失敗しました: {err}"))?,
            )
        }
        None => None,
    };

    mp4io::support::check_supported(&moov, dtvi_data.as_ref()).map_err(|err| anyhow!("{err}"))?;

    let (video_trak, video_info) = mp4io::read::find_video_track(&moov)
        .ok_or_else(|| anyhow!("映像トラックが見つかりません"))?;
    let video_track_index = track_index(&moov, TrackKind::Video)
        .ok_or_else(|| anyhow!("映像トラックが見つかりません"))?;
    let video_samples = mp4io::read::samples(&video_trak.mdia.minf.stbl);

    let map = mp4io::order_map::DisplayDecodeMap::build(&video_samples)?;
    let sync_display = map.sync_display_indices();
    let total_frames = video_samples.len() as u32;

    let trim_content = fs::read_to_string(&trim_path).with_context(|| {
        format!(
            "trim ファイルの読み込みに失敗しました: {}",
            trim_path.display()
        )
    })?;
    let trim = trim::TrimList::parse(&trim_content)
        .map_err(|err| anyhow!("trim ファイルのパースに失敗しました: {err}"))?;

    let snapped = plan::snap(&trim, &sync_display, total_frames, snap)?;
    let video_keep = plan::keep_list(&snapped, &map.order)?;
    let video_segment_durations = segment_video_durations(&snapped, &video_keep, &video_samples);

    let audio_track = if video_only {
        None
    } else {
        mp4io::read::find_audio_track(&moov)
    };
    let audio_track_index = if audio_track.is_some() {
        track_index(&moov, TrackKind::Audio)
    } else {
        None
    };
    let audio_samples: Option<Vec<SampleInfo>> = audio_track
        .as_ref()
        .map(|(trak, _)| mp4io::read::samples(&trak.mdia.minf.stbl));

    let audio_segments = match (&audio_track, &audio_samples) {
        (Some((_, info)), Some(samples)) => {
            let (segments, _drift) = audio::select_audio_segments(
                &video_segment_durations,
                video_info.timescale,
                samples,
                info.timescale,
            )?;
            Some(segments)
        }
        _ => None,
    };

    let audio_diff_inputs = match (&audio_track, &audio_samples) {
        (Some((_, info)), Some(samples)) => Some(verify::AudioDiffInputs {
            video_segment_durations: &video_segment_durations,
            video_timescale: video_info.timescale,
            audio_samples: samples,
            audio_timescale: info.timescale,
        }),
        _ => None,
    };

    let report = if verify_with_ffprobe {
        let ffprobe_path = tools::resolve_tool(tool_dir.as_deref(), tools::FFPROBE)?;
        verify::cut_verify_and_ffprobe_check(
            &input,
            &output,
            &moov,
            video_track_index,
            audio_track_index,
            &snapped,
            &video_keep,
            audio_segments.as_deref(),
            &map.order,
            dtvi_data.as_ref(),
            audio_diff_inputs,
            &ffprobe_path,
        )?
    } else {
        verify::cut_and_verify(
            &input,
            &output,
            &moov,
            video_track_index,
            audio_track_index,
            &snapped,
            &video_keep,
            audio_segments.as_deref(),
            &map.order,
            dtvi_data.as_ref(),
            audio_diff_inputs,
        )?
    };

    println!("cut 完了: {}", output.display());
    println!(
        "映像パケット数: {} / 保持区間数: {}",
        report.video_packet_count, report.video_range_count
    );
    if let Some(av_sync) = &report.av_sync {
        println!("{}", audio::format_av_sync_report(av_sync));
    }

    Ok(())
}

#[derive(Clone, Copy)]
enum TrackKind {
    Video,
    Audio,
}

/// `moov.trak` の中から、`stsd` の先頭コーデックが目的の種別に一致する
/// 最初のトラックのインデックスを返す。
fn track_index(moov: &Moov, kind: TrackKind) -> Option<usize> {
    moov.trak.iter().position(|trak| {
        matches!(
            (kind, trak.mdia.minf.stbl.stsd.codecs.first()),
            (TrackKind::Video, Some(Codec::Avc1(_))) | (TrackKind::Audio, Some(Codec::Opus(_)))
        )
    })
}

/// スナップ済み区間ごとに、対応する映像の再生時間（映像トラックの timescale 単位）を求める。
///
/// `video_keep` は `snapped` の各区間の `E - S` 個ずつが順番に連結されたものなので、
/// 先頭から順に切り出して合計するだけでよい。
fn segment_video_durations(
    snapped: &[plan::SnappedRange],
    video_keep: &[DecodeIdx],
    video_samples: &[SampleInfo],
) -> Vec<u64> {
    let mut durations = Vec::with_capacity(snapped.len());
    let mut offset = 0usize;
    for range in snapped {
        let count = (range.end.snapped - range.start.snapped) as usize;
        let slice = &video_keep[offset..offset + count];
        let duration: u64 = slice
            .iter()
            .map(|idx| video_samples[idx.0 as usize].duration as u64)
            .sum();
        durations.push(duration);
        offset += count;
    }
    durations
}

#[cfg(test)]
mod tests {
    use mp4_atom::{Encode, Ftyp};

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
