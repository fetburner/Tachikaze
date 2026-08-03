//! `analyze` / `cut` サブコマンドの組み立て。
//!
//! `src/main.rs` から切り出してライブラリ側に置いてある（理由は crate ルートの
//! ドキュメント参照）。各モジュールの処理をつなぐだけで、アルゴリズムは持たない。

use std::collections::HashSet;
use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};
use mp4_atom::{Codec, Moov};

use crate::cli::{AnalyzeArgs, AutoArgs, Cli, Commands, CutArgs, PrepareArgs, RemapSubsArgs};
use crate::dtvi::Dtvi;
use crate::mp4io::read::SampleInfo;
use crate::order::{DecodeIdx, DisplayIdx, OrderMap};
use crate::{
    analyze, audio, auto, cli, dtvi, gate, mp4io, plan, prepare, report, segmap, subtitle, tools,
    trim, verify, workdir,
};

/// `main.rs` が終了コードを決めるための、サブコマンド実行結果。
///
/// - [`ExitOutcome::Success`]: 0 で終了する。
/// - [`ExitOutcome::GateStopped`]: 3 で終了する（2 は clap が引数の誤り
///   （usage error）に使うため、`main.rs` の doc comment参照）。
///
/// `auto`（#62）の gate が疑わしいと判定して cut を実行せず止まった場合にだけ
/// [`ExitOutcome::GateStopped`] を返す。`analyze` / `cut` / `prepare` / `remap-subs`
/// はこの値を返す経路を持たない（常に `Ok(ExitOutcome::Success)` か `Err`
/// （呼び出し元の `main.rs` で exit code 1 になる、変更前と同じ挙動）のどちらかで、
/// **既存の CLI 挙動を一切変えない**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitOutcome {
    Success,
    GateStopped,
}

/// パース済みの CLI 引数を受け取り、対応するサブコマンドを実行する。
pub fn run(cli: Cli) -> anyhow::Result<ExitOutcome> {
    let cache_dir = cli.cache_dir.clone();

    match cli.command {
        Commands::Analyze(args) => run_analyze(cache_dir, args).map(|()| ExitOutcome::Success),
        Commands::Cut(args) => run_cut(cache_dir, args).map(|()| ExitOutcome::Success),
        Commands::Prepare(args) => run_prepare(cache_dir, args).map(|()| ExitOutcome::Success),
        Commands::RemapSubs(args) => run_remap_subs(cache_dir, args).map(|()| ExitOutcome::Success),
        Commands::Auto(args) => run_auto(cache_dir, args),
    }
}

/// `auto` サブコマンドの実行。処理本体は [`auto::run`] に集約してあり（`commands.rs`
/// はアルゴリズムを持たない方針、本ファイル冒頭の doc comment参照）、ここでは
/// [`auto::AutoConfig`] の組み立てと、[`auto::InputStatus`] から exit code
/// （[`ExitOutcome`]）への変換だけを行う。
fn run_auto(cache_dir: Option<PathBuf>, args: AutoArgs) -> anyhow::Result<ExitOutcome> {
    let AutoArgs {
        input,
        output,
        cm_output,
        ignore_gate,
        force,
        analyze_only,
        no_subtitles,
        snap,
        verify,
        jl_file,
        jls_set,
    } = args;

    let config = auto::AutoConfig {
        cache_dir,
        output,
        cm_output,
        ignore_gate,
        force,
        analyze_only,
        no_subtitles,
        snap,
        verify,
        jl_file,
        jls_set,
    };

    match auto::run(&config, &input)? {
        auto::InputStatus::Completed | auto::InputStatus::Skipped => Ok(ExitOutcome::Success),
        auto::InputStatus::GateStopped => Ok(ExitOutcome::GateStopped),
    }
}

/// `prepare` サブコマンドの実行。処理本体は [`prepare::run`] に集約してあり、
/// ここでは結果を人間向けに表示するだけ。
fn run_prepare(cache_dir: Option<PathBuf>, args: PrepareArgs) -> anyhow::Result<()> {
    let PrepareArgs { input, subs } = args;
    let outcome = prepare::run(&input, cache_dir.as_deref(), subs.as_deref())?;

    if outcome.ran_ffmpeg {
        eprintln!("prepare 完了: {}", outcome.media_path.display());
        if outcome.had_edit_list {
            eprintln!("  edit list (elst) を除去しました。");
        }
    } else {
        eprintln!(
            "prepare 不要: 入力をそのまま使えます: {}",
            outcome.media_path.display()
        );
    }
    if let Some(subtitle_path) = &outcome.subtitle_path {
        eprintln!("字幕: {}", subtitle_path.display());
    }

    Ok(())
}

/// `remap-subs` サブコマンドの実行。区間マップ・字幕サイドカーを解決し、
/// [`subtitle::remap_ass`] / [`subtitle::remap_srt`] に処理を委ね、結果を書き出して
/// 件数を報告する。処理そのもの（分類・時刻変換）は `subtitle` モジュールに集約して
/// あり、ここでは配線とログ出力だけを行う（`commands.rs` はアルゴリズムを持たない
/// 方針、本ファイル冒頭の doc comment参照）。
fn run_remap_subs(cache_dir: Option<PathBuf>, args: RemapSubsArgs) -> anyhow::Result<()> {
    let RemapSubsArgs {
        input,
        segment_map: segment_map_path,
        subs: subs_path_arg,
        output,
    } = args;
    if let Some(path) = &output {
        reject_dash_output(path, "-o/--output")?;
    }
    let segment_map_path =
        resolve_segment_map_path(segment_map_path, cache_dir.as_deref(), &input)?;
    let segment_map_json = fs::read_to_string(&segment_map_path).with_context(|| {
        format!(
            "区間マップの読み込みに失敗しました: {}",
            segment_map_path.display()
        )
    })?;
    let segment_map = segmap::SegmentMap::from_json(&segment_map_json)
        .map_err(|err| anyhow!("区間マップのパースに失敗しました: {err}"))?;

    let (subs_input_path, format) = resolve_subs_path(subs_path_arg, cache_dir.as_deref(), &input)?;
    let subs_content = fs::read_to_string(&subs_input_path).with_context(|| {
        format!(
            "字幕サイドカーの読み込みに失敗しました: {}",
            subs_input_path.display()
        )
    })?;

    let remap_output = match format {
        subtitle::SubsFormat::Ass => subtitle::remap_ass(
            &subs_content,
            &segment_map.segments,
            segment_map.video_timescale,
        ),
        subtitle::SubsFormat::Srt => subtitle::remap_srt(
            &subs_content,
            &segment_map.segments,
            segment_map.video_timescale,
        ),
    }
    .map_err(|err| anyhow!("字幕の張り替えに失敗しました: {err}"))?;

    let output =
        output.unwrap_or_else(|| default_remap_subs_output_path(&input, format.extension()));
    fs::write(&output, &remap_output.content)
        .with_context(|| format!("字幕の書き出しに失敗しました: {}", output.display()))?;

    let stats = &remap_output.stats;
    eprintln!(
        "remap-subs 完了: {}（シフト {} 件 / 破棄 {} 件 / クリップ {} 件）",
        output.display(),
        stats.shifted,
        stats.discarded,
        stats.clipped
    );
    if stats.shifted == 0 && stats.clipped == 0 && stats.discarded > 0 {
        eprintln!(
            "[remap-subs] 字幕イベントがすべて除去区間(CM)に含まれていたため、出力は\
             空です: {}",
            output.display()
        );
    }
    for warning in &stats.warnings {
        eprintln!("[remap-subs] {warning}");
    }

    Ok(())
}

/// `--segment-map` を解決する。[`resolve_dtvi_path`] と同じ方針（明示優先、
/// 未指定ならキャッシュ、どちらにも無ければ生成コマンド例を添えて停止）。
fn resolve_segment_map_path(
    explicit: Option<PathBuf>,
    cache_dir: Option<&Path>,
    input: &Path,
) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }

    let cached = workdir::cached_segment_map_path(cache_dir, input)
        .with_context(|| format!("区間マップの自動解決に失敗しました: {}", input.display()))?;
    if cached.is_file() {
        return Ok(cached);
    }

    bail!(
        "--segment-map が指定されておらず、キャッシュにも区間マップが見つかりませんでした: {}\n\
         先に次を実行してください:\n  \
         tachikaze cut {} --trim trim.avs -o OUT.mp4",
        cached.display(),
        input.display()
    );
}

/// `--subs` を解決する。明示されていればその拡張子から形式を判定する。未指定なら
/// `prepare` が書くキャッシュ（`workdir::subs_path`）を `ass` → `srt` の順に探す
/// （issue #59 「やること」1）。
fn resolve_subs_path(
    explicit: Option<PathBuf>,
    cache_dir: Option<&Path>,
    input: &Path,
) -> anyhow::Result<(PathBuf, subtitle::SubsFormat)> {
    if let Some(path) = explicit {
        let format = subtitle::SubsFormat::from_path(&path).ok_or_else(|| {
            anyhow!(
                "字幕ファイルの拡張子から形式を判定できません(ass/ssa/srtのみ対応): {}",
                path.display()
            )
        })?;
        return Ok((path, format));
    }

    for format in [subtitle::SubsFormat::Ass, subtitle::SubsFormat::Srt] {
        let candidate =
            workdir::subs_path(cache_dir, input, format.extension()).with_context(|| {
                format!(
                    "字幕サイドカーのキャッシュパスの解決に失敗しました: {}",
                    input.display()
                )
            })?;
        if candidate.is_file() {
            return Ok((candidate, format));
        }
    }

    bail!(
        "--subs が指定されておらず、キャッシュにも字幕サイドカー(ass/srt)が見つかりませんでした。\n\
         先に次を実行するか、--subs PATH で明示してください:\n  \
         tachikaze prepare {}",
        input.display()
    );
}

/// `-` を出力パスとして受け取ったときに拒否する。mp4・区間マップ・字幕サイドカーは
/// いずれも seek や事後の rename を伴う書き込みで、標準出力には出せない。
/// 拒否せずに `fs::write` / `File::create` へそのまま渡すと、`-` という名前の
/// ファイルをカレントディレクトリに黙って作ってしまう（CLAUDE.md の罠。`analyze -o -`
/// だけが標準出力を意味する特別扱いで、`commands::run_analyze` が別に処理する）。
///
/// `pub(crate)`: `auto::run`（#62）が `cut` と同じ出力先（`output` / `cm_output`）を
/// 検証するため。
pub(crate) fn reject_dash_output(path: &Path, flag: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        path != Path::new("-"),
        "{flag} に `-`（標準出力）は指定できません: シーク可能なファイルとして書き込む\
         必要があるため、実際のパスを指定してください"
    );
    Ok(())
}

/// `remap-subs` の `-o` 省略時の出力先。`*_CMcut.<ext>` という stem を使う
/// （かつて存在したシェルラッパー `scripts/tachikaze-cmcut` の `build_output_path`
/// が使っていた、`cut` の当時の既定出力名 `*_CMcut.mp4` と同じ規則。`cut` の `-o`
/// は現在は必須で既定名を持たないが、`remap-subs` を単体で使うときの利便性の
/// ためにこの規則を残している。`auto` の追加に伴いシェルラッパー自体は削除済み、
/// `[E11-7]`）ことで、多くのプレイヤーが同名の字幕サイドカーを自動的に読み込める
/// 形にする（issue #59「やること」5）。出力は入力の隣に置く
/// （`docs/architecture.md`「パス解決」節の「出力」分類と同じ扱い。キャッシュ
/// ではなく成果物）。
///
/// `auto` は入力の stem ではなく `-o` の stem から字幕サイドカーを導出するため
/// （`src/auto.rs::subs_sidecar_path`、issue #73「やること」5）、この関数は
/// 使わない。
fn default_remap_subs_output_path(input: &Path, extension: &str) -> PathBuf {
    let dir = input.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    dir.join(format!("{stem}_CMcut.{extension}"))
}

/// `analyze` サブコマンドの実行。`-o` の3通り（省略/明示パス/`-`）を
/// [`analyze::AnalyzeConfig::output`] と [`analyze::AnalyzeOutput`] の
/// 組み合わせで処理する: 省略はキャッシュのみ（`cache_trim_path` を stderr へ
/// 案内）、明示パスは従来どおり、`-` は標準出力（`raw_trim` を直接書く。
/// パース結果の `TrimList` を再シリアライズすると元のバイト列と一致する
/// 保証が無いため使わない）。
fn run_analyze(cache_dir: Option<PathBuf>, args: AnalyzeArgs) -> anyhow::Result<()> {
    let AnalyzeArgs {
        input,
        output,
        report: show_report,
        jls_set,
        jl_file,
    } = args;

    let jls_set = jls_set
        .iter()
        .map(|raw| analyze::parse_jls_set_arg(raw))
        .collect::<anyhow::Result<Vec<_>>>()?;

    // `-o -` は標準出力を意味するため、`-` という名前のファイルを作ってしまわない
    // よう、`analyze::AnalyzeConfig::output` には渡さない（キャッシュにだけ書かせ、
    // 生の内容は `AnalyzeOutput::raw_trim` から直接標準出力へ書く。CLAUDE.md の罠）。
    let to_stdout = output.as_deref() == Some(Path::new("-"));
    let explicit_output = if to_stdout { None } else { output.clone() };

    let config = analyze::AnalyzeConfig {
        input,
        output: explicit_output,
        cache_dir,
        jls_set,
        jl_file,
    };

    let result = analyze::run(&config)?;

    if to_stdout {
        let mut stdout = io::stdout();
        stdout
            .write_all(result.raw_trim.as_bytes())
            .context("trim.avs の標準出力への書き出しに失敗しました")?;
        // `io::stdout()` は行バッファリングのため、`raw_trim` が改行で終わらない
        // 場合はプロセス終了時の暗黙 flush 頼みになり、そこでの書き込みエラーが
        // 黙って捨てられる（exit 0 になりうる）。明示的に flush してエラーを拾う。
        stdout
            .flush()
            .context("trim.avs の標準出力への flush に失敗しました")?;
        eprintln!("trim.avs を標準出力へ書き出しました。");
    } else if let Some(path) = &output {
        eprintln!("trim.avs を書き出しました: {}", path.display());
    } else {
        eprintln!(
            "trim.avs をキャッシュへ書き出しました: {}",
            result.cache_trim_path.display()
        );
    }

    if show_report {
        let fps = fps_from_dtvi(&result.dtvi);

        eprintln!(
            "\n{}",
            report::format_report(&result.trim, &result.dtvi, &result.jls_entries, true)
        );

        let missed = report::missed::find_missed_candidates(&result.trim, &result.jls_entries);
        if !missed.is_empty() {
            eprintln!("見逃し候補の警告:");
            for candidate in &missed {
                eprintln!("{}", report::missed::format_warning(candidate, fps));
            }
        }

        let verdict = gate::evaluate(&result.trim, &result.jls_entries, &result.dtvi);
        eprintln!("\n{}", gate::format_gate_report(&verdict));
    }

    Ok(())
}

/// `.dtvi` ヘッダの `frame_rate_num` / `frame_rate_den` から fps を求める。
/// キーが無い、または数値としてパースできない場合は対象素材の実測値
/// （30000/1001 ≈ 29.97fps）を既定にする。
///
/// `pub(crate)`: `auto::run`（#62）が見逃し候補の警告表示に同じ fps を使うため
/// （`analyze --report` と同じ計算式で重複させないための共有）。
pub(crate) fn fps_from_dtvi(dtvi: &dtvi::Dtvi) -> f64 {
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

/// `cut` サブコマンドの実行。処理本体は [`execute_cut`] に集約してあり、ここでは
/// CLI 引数から [`CutParams`] を組み立て、結果を人間向けに表示するだけ。
///
/// この分離は `auto`（#62、`src/auto.rs`）が `cut` と同じ処理（`--cm-output` の
/// 自己検証8・保持側/CM側の atomic rename・区間マップの書き出しを含む）を複製せずに
/// 再利用するために導入した（CLAUDE.md「analyze/cut のロジックを複製しない」）。
/// `run_cut` 自体は `pub` にせず（CLI のオプション構成は変えたくない）、
/// [`execute_cut`] / [`CutParams`] / [`CutOutcome`] を `pub(crate)` にして
/// 同一クレート内の `auto` から呼べるようにした。この変更で `cut` サブコマンド自体の
/// 標準出力・exit code は変わらない（`tests/segmap_e2e.rs` 等の既存 E2E がそのまま通る
/// ことで確認済み）。
fn run_cut(cache_dir: Option<PathBuf>, args: CutArgs) -> anyhow::Result<()> {
    let CutArgs {
        input,
        trim: trim_path,
        output,
        snap,
        video_only,
        verify: verify_with_ffprobe,
        dtvi: dtvi_path,
        cm_output,
        segment_map: segment_map_path,
    } = args;

    let outcome = execute_cut(CutParams {
        cache_dir,
        input,
        trim_path,
        output,
        snap,
        video_only,
        verify_with_ffprobe,
        dtvi_path,
        cm_output,
        segment_map_path,
    })?;

    print_cut_report("cut 完了", "保持区間数", &outcome.output, &outcome.report);
    if let (Some(cm_output), Some(cm_report)) =
        (outcome.cm_output.as_ref(), outcome.cm_report.as_ref())
    {
        print_cut_report("CM 出力完了", "CM区間数", cm_output, cm_report);
    }

    Ok(())
}

/// [`execute_cut`] にまとめて渡す引数。フィールドの意味は `cut` の CLI オプション
/// （`src/cli.rs::Commands::Cut`）と1対1に対応する。
pub(crate) struct CutParams {
    /// `--cache-dir`（キャッシュの根）。未指定なら既定値。
    pub cache_dir: Option<PathBuf>,
    pub input: PathBuf,
    pub trim_path: PathBuf,
    pub output: PathBuf,
    pub snap: cli::Snap,
    pub video_only: bool,
    pub verify_with_ffprobe: bool,
    pub dtvi_path: Option<PathBuf>,
    pub cm_output: Option<PathBuf>,
    pub segment_map_path: Option<PathBuf>,
}

/// [`execute_cut`] の戻り値。呼び出し側（`run_cut` / `auto::run`）が結果を
/// 表示するのに必要な情報だけを持つ（`print_cut_report` にそのまま渡せる形）。
pub(crate) struct CutOutcome {
    pub output: PathBuf,
    pub report: verify::VerifyReport,
    /// `--cm-output` 指定時のみ `Some`。
    pub cm_output: Option<PathBuf>,
    pub cm_report: Option<verify::VerifyReport>,
}

/// `cut` パイプライン本体。`run_cut`（CLI ハンドラ）と `auto::run` の両方から呼ばれる
/// （`auto` がこのロジックを複製しないための唯一の入口。上記 `run_cut` の doc comment
/// 参照）。コンソールへの出力は一切行わない（結果表示は呼び出し側の責務）。
/// `params.trim_path` が `-` の場合、標準入力を最後まで読むまでブロックする
/// （`auto::run` は常に実ファイルパスを渡すため、この経路は CLI の `cut` からのみ
/// 到達する）。
pub(crate) fn execute_cut(params: CutParams) -> anyhow::Result<CutOutcome> {
    let CutParams {
        cache_dir,
        input,
        trim_path,
        output,
        snap,
        video_only,
        verify_with_ffprobe,
        dtvi_path,
        cm_output,
        segment_map_path,
    } = params;

    // mp4 / 区間マップは seek が要るため stdout には出せない。`-` を渡すと
    // `-` という名前のファイルを黙って作ってしまう（CLAUDE.md の罠）ため、
    // 出力になり得る全パスで明示的に拒否する。
    reject_dash_output(&output, "-o/--output")?;
    if let Some(path) = &cm_output {
        reject_dash_output(path, "--cm-output")?;
    }
    if let Some(path) = &segment_map_path {
        reject_dash_output(path, "--segment-map")?;
    }

    // 自己検証を通って新しいマップを書けた場合だけ区間マップが残る、という状態に
    // するため、処理を始める前に既定キャッシュパスの古い区間マップを削除する
    // （レビュー指摘#4）。cut が一度成功して区間マップを書いた後、同じ入力に対して
    // 別の trim.avs で再実行して自己検証に失敗すると、古い区間マップがキャッシュに
    // 残ったままになり、`remap-subs` が鮮度チェックなしにそれを使ってしまう
    // （古い trim.avs に基づく境界で字幕を張り替えてしまうが、エラーも警告も出ない）。
    // `--segment-map` で明示されたパスは呼び出し側が管理するファイルなので触らない。
    clear_stale_cached_segment_map(cache_dir.as_deref(), &input);

    // `--snap inward` は保持区間を退化させうる（終端が開始より前になる）。その場合
    // 「保持区間の補集合をそのまま区間リストとして使える」という complement_ranges の
    // 前提（区間が昇順・非重複）が崩れ、CM側の区間の順序も壊れる
    // （docs/lossless-cut.md「CM 側（除去した区間）を別ファイルに出す」節）。
    // 実害が出る前に、ここで明示エラーにして止める。`auto::run` は「既定で
    // --cm-output 相当を付ける」という auto 固有の文脈に沿った、より分かりやすい
    // メッセージで同じ組み合わせを事前に弾く（`auto.rs` の doc comment参照）ため、
    // ここに来た時点では通常 `cut` を直接叩いた場合のメッセージでよい。
    if cm_output.is_some() && snap == cli::Snap::Inward {
        bail!(
            "--snap inward と --cm-output は併用できません。inward スナップでは保持区間が \
             退化する（終端が開始より前になる）ことがあり、その場合 CM 側（補集合）の \
             区間の順序も壊れるため、意味のある CM 出力を作れません。--cm-output を \
             使う場合は既定の --snap outward を使ってください。"
        );
    }

    let moov = mp4io::read::read_moov(&input)
        .with_context(|| format!("入力 mp4 の読み込みに失敗しました: {}", input.display()))?;

    let dtvi_path = resolve_dtvi_path(dtvi_path, cache_dir.as_deref(), &input)?;
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

    // `--trim -` は標準入力を意味する（`analyze -o -` の出力をそのまま渡せる）。
    let trim_content = if trim_path == Path::new("-") {
        let mut buf = String::new();
        io::stdin()
            .read_to_string(&mut buf)
            .context("trim の標準入力からの読み込みに失敗しました")?;
        buf
    } else {
        fs::read_to_string(&trim_path).with_context(|| {
            format!(
                "trim ファイルの読み込みに失敗しました: {}",
                trim_path.display()
            )
        })?
    };
    let trim = trim::TrimList::parse(&trim_content)
        .map_err(|err| anyhow!("trim ファイルのパースに失敗しました: {err}"))?;

    let snapped = plan::snap(&trim, &sync_display, total_frames, snap)?;
    let video_keep = plan::keep_list(&snapped, &map.order)?;

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
    let audio_timescale = audio_track.as_ref().map(|(_, info)| info.timescale);

    let pipeline = CutPipeline {
        input: &input,
        moov: &moov,
        video_track_index,
        audio_track_index,
        video_samples: &video_samples,
        video_timescale: video_info.timescale,
        order: &map.order,
        audio_samples: audio_samples.as_deref(),
        audio_timescale,
        dtvi_data: dtvi_data.as_ref(),
        verify_with_ffprobe,
    };

    match cm_output {
        None => {
            let run_output = pipeline.run(&snapped, &video_keep, &output)?;
            if let Some(dtvi) = dtvi_data.as_ref() {
                write_segment_map(
                    cache_dir.as_deref(),
                    &input,
                    &snapped,
                    &run_output.video_segment_durations,
                    &run_output.video_segment_source_starts,
                    video_info.timescale,
                    dtvi,
                    total_frames,
                    segment_map_path.as_deref(),
                );
            }
            Ok(CutOutcome {
                output,
                report: run_output.report,
                cm_output: None,
                cm_report: None,
            })
        }
        Some(cm_output) => {
            let complement = plan::complement_ranges(&snapped, total_frames);
            let cm_keep = plan::keep_list(&complement, &map.order)?;

            // 自己検証8（docs/architecture.md「自己検証」節）: 保持側とCM側の映像フレーム
            // 数の合計 == 総フレーム数、かつ DecodeIdx の集合が互いに素であることを、
            // どちらの出力もまだ書き出していない時点（純粋な計算結果の比較のみ）で確認する。
            // ここで先に確認しておくことで、失敗時にどちらの出力ファイルも一切作られない
            // ことを保証する（「片方だけ書けて片方で落ちる」順序を避ける）。
            verify_mutual_exclusivity(&video_keep, &cm_keep, total_frames)?;

            // 保持側・CM側とも、まず最終出力先とは別の一時パスに書き出す。両方成功した
            // 場合にのみ最終パスへ rename することで、どちらか一方の書き出し・検証が
            // 失敗したときに成功した側だけが最終出力として残ってしまう事態を防ぐ
            // （`verify::cut_and_verify` 自体は1ファイル単位でしか原子性を持たないため、
            // 2ファイルにまたがる原子性はここで担保する）。
            let tmp_output = sibling_pending_path(&output);
            let tmp_cm_output = sibling_pending_path(&cm_output);

            let write_result = (|| -> anyhow::Result<(CutRunOutput, CutRunOutput)> {
                let run_output = pipeline.run(&snapped, &video_keep, &tmp_output)?;
                let cm_run_output = pipeline.run(&complement, &cm_keep, &tmp_cm_output)?;
                Ok((run_output, cm_run_output))
            })();

            let (run_output, cm_run_output) = match write_result {
                Ok(v) => v,
                Err(err) => {
                    let _ = fs::remove_file(&tmp_output);
                    let _ = fs::remove_file(&tmp_cm_output);
                    return Err(err);
                }
            };

            fs::rename(&tmp_output, &output).with_context(|| {
                format!(
                    "一時ファイル({})から出力先({})への rename に失敗しました",
                    tmp_output.display(),
                    output.display()
                )
            })?;
            fs::rename(&tmp_cm_output, &cm_output).with_context(|| {
                format!(
                    "一時ファイル({})から出力先({})への rename に失敗しました",
                    tmp_cm_output.display(),
                    cm_output.display()
                )
            })?;

            // 区間マップは保持側だけ出す（CM側は検出確認用で、字幕を付ける対象ではない。
            // issue #57「やること」6）。両方の rename が終わった後（＝自己検証を通って
            // 最終出力へ rename できた後）にだけ書く。
            if let Some(dtvi) = dtvi_data.as_ref() {
                write_segment_map(
                    cache_dir.as_deref(),
                    &input,
                    &snapped,
                    &run_output.video_segment_durations,
                    &run_output.video_segment_source_starts,
                    video_info.timescale,
                    dtvi,
                    total_frames,
                    segment_map_path.as_deref(),
                );
            }

            Ok(CutOutcome {
                output,
                report: run_output.report,
                cm_output: Some(cm_output),
                cm_report: Some(cm_run_output.report),
            })
        }
    }
}

/// `--dtvi` を解決する。
///
/// - 明示されていればそれを最優先でそのまま使う。
/// - 未指定なら、`analyze` が使うのと同じキャッシュパス規則
///   （[`workdir::cached_dtvi_path`]）から `work.mp4.dtvi` を探す。直前に
///   同じ入力・同じ `--cache-dir` で `analyze` を実行していればそのまま見つかる。
/// - どちらの経路でも見つからない場合は、検証を省略せず停止する（罠3:
///   オープン GOP 判定と自己検証4に `.dtvi` が必須なため。`.dtvi` が無い
///   まま処理を続けると間違った位置で切っても例外が飛ばない）。`analyze`
///   を実行するコマンド例を添えたエラーにする。
fn resolve_dtvi_path(
    dtvi_path: Option<PathBuf>,
    cache_dir: Option<&Path>,
    input: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    if dtvi_path.is_some() {
        return Ok(dtvi_path);
    }

    let cached = workdir::cached_dtvi_path(cache_dir, input)
        .with_context(|| format!("`.dtvi` の自動解決に失敗しました: {}", input.display()))?;
    if cached.is_file() {
        return Ok(Some(cached));
    }

    bail!(
        "`--dtvi` が指定されておらず、キャッシュにも `.dtvi` が見つかりませんでした: {}\n\
         先に次を実行してください:\n  \
         tachikaze analyze {} -o trim.avs",
        cached.display(),
        input.display()
    );
}

/// cut パイプラインのうち「どの区間を切り出すか」以外は変わらない入力をまとめたもの。
///
/// `--cm-output` 指定時、保持側と CM 側（補集合）に対して同じ処理（音声区間選択→
/// `verify::cut_and_verify`(または ffprobe 検証付き版)の呼び出し）を2回行う。呼び出しごと
/// に変わるのは区間リスト（`snapped` / `video_keep`）と出力先だけなので、それ以外を
/// この構造体にまとめることで `run_cut` 本体の `#[allow(clippy::too_many_arguments)]` を
/// 増やさずに済ませる。
struct CutPipeline<'a> {
    input: &'a Path,
    moov: &'a Moov,
    video_track_index: usize,
    audio_track_index: Option<usize>,
    video_samples: &'a [SampleInfo],
    video_timescale: u32,
    order: &'a OrderMap,
    audio_samples: Option<&'a [SampleInfo]>,
    audio_timescale: Option<u32>,
    dtvi_data: Option<&'a Dtvi>,
    verify_with_ffprobe: bool,
}

impl CutPipeline<'_> {
    /// 区間リスト `snapped`（に対応する `video_keep`）を1回分書き出し、検証する。
    /// 保持側・CM側のどちらの呼び出しにも使う共通処理（音声区間選択→書き出し→
    /// 自己検証）。`--video-only` / `--verify` / `.dtvi` の扱いは呼び出し元に関わらず
    /// 常に同じになる（`self` のフィールドで一元管理しているため分岐が生まれない）。
    fn run(
        &self,
        snapped: &[plan::SnappedRange],
        video_keep: &[DecodeIdx],
        output: &Path,
    ) -> anyhow::Result<CutRunOutput> {
        let video_segment_durations =
            segment_video_durations(snapped, video_keep, self.video_samples);
        let video_segment_source_starts =
            segment_video_source_starts(snapped, self.order, self.video_samples)?;

        let audio_segments = match (self.audio_samples, self.audio_timescale) {
            (Some(samples), Some(audio_timescale)) => {
                let (segments, _drift) = audio::select_audio_segments(
                    &video_segment_durations,
                    &video_segment_source_starts,
                    self.video_timescale,
                    samples,
                    audio_timescale,
                )?;
                Some(segments)
            }
            _ => None,
        };

        let audio_diff_inputs = match (self.audio_samples, self.audio_timescale) {
            (Some(samples), Some(audio_timescale)) => Some(verify::AudioDiffInputs {
                video_segment_durations: &video_segment_durations,
                video_segment_source_starts: &video_segment_source_starts,
                video_timescale: self.video_timescale,
                audio_samples: samples,
                audio_timescale,
            }),
            _ => None,
        };

        let report = if self.verify_with_ffprobe {
            let ffprobe_path = tools::resolve_tool(tools::FFPROBE)?;
            verify::cut_verify_and_ffprobe_check(
                self.input,
                output,
                self.moov,
                self.video_track_index,
                self.audio_track_index,
                snapped,
                video_keep,
                audio_segments.as_deref(),
                self.order,
                self.dtvi_data,
                audio_diff_inputs,
                &ffprobe_path,
            )
        } else {
            verify::cut_and_verify(
                self.input,
                output,
                self.moov,
                self.video_track_index,
                self.audio_track_index,
                snapped,
                video_keep,
                audio_segments.as_deref(),
                self.order,
                self.dtvi_data,
                audio_diff_inputs,
            )
        }?;

        Ok(CutRunOutput {
            report,
            video_segment_durations,
            video_segment_source_starts,
        })
    }
}

/// [`CutPipeline::run`] の戻り値。自己検証を通り、`output` へ書き出し済みの
/// [`verify::VerifyReport`] に加えて、その書き出しで使った区間ごとの情報
/// （出力の長さ・ソース上の開始 DTS）も一緒に返す。
///
/// この2つは元々 `run` の内部でだけ計算していた値だが、区間マップ（`segmap.rs`、
/// issue #57）を書くには snap 後の値そのもの（`trim.avs` から再計算した値ではない）
/// が要る。ここで一緒に返すことで、`run_cut` 側で同じ計算をやり直さずに済む
/// （計算が2箇所に分かれて食い違うリスクを避ける）。
struct CutRunOutput {
    report: verify::VerifyReport,
    /// 区間ごとの出力側の長さ（映像 timescale 単位）。`segment_video_durations` の
    /// 戻り値そのもの。
    video_segment_durations: Vec<u64>,
    /// 区間ごとの、ソース上の絶対開始 DTS（映像 timescale 単位）。
    /// `segment_video_source_starts` の戻り値そのもの（PTS ではなく DTS。理由は
    /// 同関数の doc comment 参照）。
    video_segment_source_starts: Vec<u64>,
}

/// 自己検証8（docs/architecture.md「自己検証」節）: `--cm-output` 指定時、保持側とCM側で
/// 次の2点を assert する。
///
/// - 映像フレーム数の合計が入力の総フレーム数と一致する
/// - `DecodeIdx` の集合が互いに素である
///
/// `video_keep` / `cm_keep` はどちらも `plan::keep_list` の戻り値そのもの（I/O を伴わない
/// 純粋な計算結果）なので、この検査は書き出し前に行える。境界が1フレームでもずれれば
/// 必ず失敗する。
fn verify_mutual_exclusivity(
    video_keep: &[DecodeIdx],
    cm_keep: &[DecodeIdx],
    total_frames: u32,
) -> anyhow::Result<()> {
    let total = video_keep.len() + cm_keep.len();
    anyhow::ensure!(
        total as u64 == u64::from(total_frames),
        "自己検証8(相互検証)に失敗: 保持側の映像フレーム数({}) と CM側の映像フレーム数({}) \
         の合計({total}) が入力の総フレーム数({total_frames})と一致しません",
        video_keep.len(),
        cm_keep.len()
    );

    let video_set: HashSet<DecodeIdx> = video_keep.iter().copied().collect();
    let mut overlap: Vec<u32> = cm_keep
        .iter()
        .filter(|d| video_set.contains(d))
        .map(|d| d.0)
        .collect();
    overlap.sort_unstable();
    anyhow::ensure!(
        overlap.is_empty(),
        "自己検証8(相互検証)に失敗: 保持側とCM側の映像デコード順インデックスが{}個重複して \
         います(先頭5件: {:?})。境界のスナップまたは補集合の計算に誤りがある可能性があります",
        overlap.len(),
        overlap.iter().take(5).collect::<Vec<_>>()
    );

    Ok(())
}

/// `path` と同じディレクトリに、一意なサフィックスを付けた一時パスを作る。
///
/// `--cm-output` 指定時、保持側とCM側の両方の書き出し・検証が成功するまで最終的な
/// 出力先へ rename しない（`run_cut` のコメント参照）。`verify::cut_and_verify` 内部の
/// 一時ファイル（書き出し直後の検証用、`temp_output_path`）とは別の、もう1段階の
/// 一時パス。
fn sibling_pending_path(path: &Path) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let mut os = path.as_os_str().to_os_string();
    os.push(format!(".cm-pending-{}-{nonce}", std::process::id()));
    PathBuf::from(os)
}

/// cut 完了時の結果表示。保持側/CM側で見出しと区間数のラベルだけを変える
/// （`docs/architecture.md`「コマンド構成」節の表示例参照）。
///
/// `pub(crate)`: `auto::run`（#62）が `cut` と同じ完了表示を再利用するため。
pub(crate) fn print_cut_report(
    label: &str,
    range_label: &str,
    output: &Path,
    report: &verify::VerifyReport,
) {
    eprintln!("{label}: {}", output.display());
    eprintln!(
        "映像パケット数: {} / {range_label}: {}",
        report.video_packet_count, report.video_range_count
    );
    if let Some(av_sync) = &report.av_sync {
        eprintln!("{}", audio::format_av_sync_report(av_sync));
    }
}

/// `cut` の処理を始める前に、既定キャッシュパス（`workdir::cached_segment_map_path`）
/// にある古い区間マップを削除する（[`execute_cut`] 冒頭のコメント、レビュー指摘#4）。
///
/// 削除に失敗しても致命的エラーにはしない（警告に留める。区間マップ自体が
/// 「消えても再生成できるキャッシュ」という位置づけで、その削除の失敗を理由に
/// `cut` 本体を止める必要が無いため、`workdir.rs` の分類と同じ扱い）。ファイルが
/// 元々存在しない場合（`NotFound`）は警告すら出さない（毎回の `cut` 実行で
/// ノイズになるため）。`--segment-map` の明示パスはここでは一切触らない
/// （呼び出し側が管理するファイルであり、キャッシュではない）。
fn clear_stale_cached_segment_map(cache_dir: Option<&Path>, input: &Path) {
    let path = match workdir::cached_segment_map_path(cache_dir, input) {
        Ok(path) => path,
        Err(err) => {
            eprintln!(
                "[segmap] 区間マップのキャッシュパス解決に失敗しました（古いマップの削除を\
                 スキップします）: {err}"
            );
            return;
        }
    };

    match fs::remove_file(&path) {
        Ok(()) => {
            eprintln!(
                "[segmap] 古い区間マップを削除しました（今回の cut が自己検証を通った場合のみ\
                 再作成されます）: {}",
                path.display()
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "[segmap] 古い区間マップの削除に失敗しました（警告のみ、処理は続行します）: {} \
                 ({err})",
                path.display()
            );
        }
    }
}

/// cut が自己検証を通り、最終出力へ rename し終えた**後**に、保持側の区間マップ
/// （`segmap.rs`、issue #57）を書き出す。
///
/// マップは analyze の中間物（`.dtvi` / `trim.avs` / `detail.jls`）と同じ分類の
/// 再生成できるキャッシュ（docs/architecture.md「パス解決」節）。書き込みに失敗しても
/// 既に検証済みの mp4（呼び出し元で既に rename 済み）を破棄する理由にはしないため、
/// ここでは `anyhow::Result` を返さず、失敗時は警告を出すだけにする（次にここを触る
/// 人へ: `.ok()` で握りつぶさず、警告だけは必ず出すこと）。
#[allow(clippy::too_many_arguments)]
fn write_segment_map(
    cache_dir: Option<&Path>,
    input: &Path,
    snapped: &[plan::SnappedRange],
    video_segment_durations: &[u64],
    video_segment_source_starts: &[u64],
    video_timescale: u32,
    dtvi: &Dtvi,
    total_frames: u32,
    explicit_path: Option<&Path>,
) {
    let (frame_rate_num, frame_rate_den) = frame_rate_num_den_from_dtvi(dtvi);
    let canonical_input = fs::canonicalize(input).unwrap_or_else(|_| input.to_path_buf());

    let map = segmap::SegmentMap::build(
        snapped,
        video_segment_source_starts,
        video_segment_durations,
        video_timescale,
        frame_rate_num,
        frame_rate_den,
        canonical_input,
        total_frames,
    );

    match workdir::cached_segment_map_path(cache_dir, input) {
        Ok(path) => {
            if let Err(err) = map.write_to_file(&path) {
                eprintln!(
                    "[segmap] キャッシュへの区間マップの書き出しに失敗しました: {} ({err})",
                    path.display()
                );
            }
        }
        Err(err) => {
            eprintln!("[segmap] 区間マップのキャッシュパス解決に失敗しました: {err}");
        }
    }

    if let Some(explicit) = explicit_path {
        if let Err(err) = map.write_to_file(explicit) {
            eprintln!(
                "[segmap] {} への区間マップの書き出しに失敗しました: {err}",
                explicit.display()
            );
        }
    }
}

/// `.dtvi` ヘッダから `frame_rate_num` / `frame_rate_den` を読む。[`fps_from_dtvi`] と
/// 同じ既定値（キーが無い、または数値としてパースできない場合は対象素材の実測値
/// 30000/1001）を使うが、区間マップのヘッダにはヘッダの浮動小数点値ではなく生の
/// 分数（num/den）をそのまま残す。
fn frame_rate_num_den_from_dtvi(dtvi: &Dtvi) -> (u32, u32) {
    let parse = |key: &str, default: u32| {
        dtvi.header_value(key)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(default)
    };
    (
        parse("frame_rate_num", 30000),
        parse("frame_rate_den", 1001),
    )
}

#[derive(Clone, Copy)]
enum TrackKind {
    Video,
    Audio,
}

/// `moov.trak` の中から、`stsd` の先頭コーデックが目的の種別に一致する
/// 最初のトラックのインデックスを返す。
///
/// 音声判定は `mp4io::read::is_audio_codec` に集約している（Opus / AAC など
/// mp4-atom が認識する音声 Codec 全般。詳細は同関数の doc comment）。
fn track_index(moov: &Moov, kind: TrackKind) -> Option<usize> {
    moov.trak.iter().position(
        |trak| match (kind, trak.mdia.minf.stbl.stsd.codecs.first()) {
            (TrackKind::Video, Some(Codec::Avc1(_))) => true,
            (TrackKind::Audio, Some(codec)) => mp4io::read::is_audio_codec(codec),
            _ => false,
        },
    )
}

/// スナップ済み区間ごとに、その区間の**ソース上の絶対開始時刻（DTS 基準）**（映像
/// トラックの timescale 単位）を求める。
///
/// 【最重要・静かに壊れる罠】ここは**合成時刻（PTS 相当、`dts + cts_offset`）ではなく
/// DTS を返す**必要がある。理由は次の式（出力側で区間内のサンプルが連続し duration が
/// 保たれることから導ける）による:
///
/// ```text
/// output_pts(i) = T_{k-1} + source_pts(i) - source_dts(区間kの先頭サンプル)
/// ```
///
/// （`T_{k-1}` はそれ以前の区間の映像長の累積 = 出力タイムライン上の区間 k の開始。
/// `source_pts(i)` / `source_dts(i)` はソース上のサンプル `i` の合成時刻 / DTS。）
///
/// つまり出力タイムライン上の時刻 `T_{k-1} + u` に表示されるフレームのソース上の時刻は
/// `source_dts(区間kの先頭サンプル) + u` であり、**区間の音声は「ソース時刻
/// `dts(区間先頭サンプル)`」から選び始めるのが正しい**。
///
/// 以前はここに合成時刻（`dts + cts_offset` を返す `composition_time` という関数が
/// あった。以後は使われなくなったため削除し、DTS のみを返す
/// `mp4io::order_map::decode_timestamp` に置き換えた）を渡していた。`mp4io/write.rs` は
/// `ctts` を引き継ぐため、出力の先頭フレームの pts は 0 ではなく `cts_offset`
/// （B フレームの並べ替え深度ぶん。実測で約 66.7ms = 2 フレーム分）のままになる。
/// 一方で音声は出力タイムライン 0 から並び始めるため、**音声が映像より
/// `cts_offset` ぶん系統的に先行する**ずれが生まれていた（エラーは一切出ない）。
/// 次にここを触る人へ: 「合成時刻の方が実際の再生時刻に近いから正しいはず」という
/// 直感は罠なので、pts/合成時刻に戻さないこと。
///
/// 区間の開始点（`SnappedRange::start.snapped`、`DisplayIdx`）を `order.to_decode` で
/// デコード順に変換し、`mp4io::order_map::decode_timestamp`（`dts(i) = samples[0..i]`
/// の duration 累積、`dts(0) = 0`、`DisplayDecodeMap::build` と同じ定義）で DTS を
/// 求める。CFR 前提の `S * frame_duration` のような決め打ちはしない（open GOP 由来の
/// 端数がある場合や将来の可変長エンコードでも正しく動くようにするため）。
fn segment_video_source_starts(
    snapped: &[plan::SnappedRange],
    order: &OrderMap,
    video_samples: &[SampleInfo],
) -> anyhow::Result<Vec<u64>> {
    snapped
        .iter()
        .map(|range| video_source_decode_timestamp(video_samples, order, range.start.snapped))
        .collect()
}

/// 表示順インデックス `display` に対応する、ソース上の絶対 DTS（映像トラックの
/// timescale 単位）を求める。合成時刻（PTS）ではなく DTS を返す理由は
/// [`segment_video_source_starts`] の doc comment を参照。
fn video_source_decode_timestamp(
    video_samples: &[SampleInfo],
    order: &OrderMap,
    display: DisplayIdx,
) -> anyhow::Result<u64> {
    let decode = order.to_decode(display).ok_or_else(|| {
        anyhow!(
            "表示順インデックス {} に対応するデコード順インデックスが見つかりません",
            display.0
        )
    })?;

    mp4io::order_map::decode_timestamp(video_samples, decode).ok_or_else(|| {
        anyhow!(
            "デコード順インデックス {} が映像サンプルの範囲外です",
            decode.0
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
    use super::*;
    use std::env;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn reject_dash_output_rejects_bare_dash() {
        let err = reject_dash_output(Path::new("-"), "--cm-output").expect_err("拒否するはず");
        assert!(err.to_string().contains("--cm-output"));
    }

    #[test]
    fn reject_dash_output_allows_other_paths() {
        reject_dash_output(Path::new("out.mp4"), "-o/--output").expect("通常のパスは通るはず");
        reject_dash_output(Path::new("./-weird-but-not-bare-dash.mp4"), "-o/--output")
            .expect("`-` で始まるだけの通常のファイル名は拒否しないはず");
    }

    fn make_scratch_dir(label: &str) -> PathBuf {
        let base = env::temp_dir();
        let pid = process::id();
        for attempt in 0..100 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let candidate = base.join(format!(
                "tachikaze-commands-test-{label}-{pid}-{nanos}-{attempt}"
            ));
            if fs::create_dir(&candidate).is_ok() {
                return candidate;
            }
        }
        panic!("scratch dir の作成に失敗しました");
    }

    #[test]
    fn resolve_dtvi_path_prefers_explicit_path_over_cache() {
        // キャッシュに何も無くても、明示された `--dtvi` をそのまま最優先で使う
        // （キャッシュを探索すらしない）。
        let explicit = PathBuf::from("/explicit/path/to/some.dtvi");
        let resolved =
            resolve_dtvi_path(Some(explicit.clone()), None, Path::new("/nonexistent.mp4"))
                .expect("明示指定は解決に失敗しないはず");
        assert_eq!(resolved, Some(explicit));
    }

    #[test]
    fn resolve_dtvi_path_finds_cached_dtvi_left_by_analyze() {
        // キャッシュの根を引数で直接渡すため、環境変数もロックも不要（E12-2）。
        let cache_root = make_scratch_dir("resolve-dtvi-found-cache");

        let input_dir = make_scratch_dir("resolve-dtvi-found-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        // analyze が残すのと同じ規則でキャッシュに .dtvi を用意する。
        let expected = workdir::cached_dtvi_path(Some(&cache_root), &input_path)
            .expect("compute cached dtvi path");
        fs::create_dir_all(expected.parent().unwrap()).expect("create cache dir");
        fs::write(&expected, "dummy dtvi content").expect("write cached dtvi");

        let resolved = resolve_dtvi_path(None, Some(&cache_root), &input_path)
            .expect("キャッシュから解決できるはず");
        assert_eq!(resolved, Some(expected));

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    #[test]
    fn resolve_dtvi_path_missing_suggests_analyze_command() {
        let cache_root = make_scratch_dir("resolve-dtvi-missing-cache");

        let input_dir = make_scratch_dir("resolve-dtvi-missing-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let err = resolve_dtvi_path(None, Some(&cache_root), &input_path)
            .expect_err("キャッシュに無ければ解決に失敗するはず");
        let message = err.to_string();
        assert!(
            message.contains("tachikaze analyze"),
            "analyze の実行例が含まれていない: {message}"
        );
        assert!(
            message.contains(&input_path.display().to_string()),
            "入力パスが含まれていない: {message}"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// CLAUDE.md の罠4: `segment_video_source_starts`（区間マップの `source_start_dts`
    /// の元データ）は合成時刻（PTS 相当、`dts + cts_offset`）ではなく DTS を返す
    /// ことを、`cts_offset` を持つ合成データで固定する。
    ///
    /// `mp4io::order_map.rs` の `decode_timestamp_matches_build_derivation` と同じ
    /// 合成データ（duration 一律1000、`cts_offset` で表示順を入れ替える）を使う。
    #[test]
    fn segment_video_source_starts_returns_dts_not_composition_time() {
        let video_samples = vec![
            SampleInfo {
                file_offset: 0,
                size: 10,
                duration: 1000,
                cts_offset: 0,
                is_sync: true,
            },
            SampleInfo {
                file_offset: 10,
                size: 10,
                duration: 1000,
                cts_offset: 3000,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 20,
                size: 10,
                duration: 1000,
                cts_offset: -1000,
                is_sync: false,
            },
            SampleInfo {
                file_offset: 30,
                size: 10,
                duration: 1000,
                cts_offset: -1000,
                is_sync: false,
            },
        ];

        // 合成時刻(cts = dts + cts_offset)昇順の表示順:
        // decode0(cts=0) -> decode2(cts=1000) -> decode3(cts=2000) -> decode1(cts=4000)
        let order = OrderMap::new(vec![
            (DisplayIdx(0), DecodeIdx(0)),
            (DisplayIdx(1), DecodeIdx(2)),
            (DisplayIdx(2), DecodeIdx(3)),
            (DisplayIdx(3), DecodeIdx(1)),
        ]);

        // 表示順1(=decode2、cts_offset=-1000)から始まる区間。
        let snapped = vec![plan::SnappedRange {
            start: plan::SnappedBoundary {
                original: DisplayIdx(1),
                snapped: DisplayIdx(1),
                delta_frames: 0,
            },
            end: plan::SnappedBoundary {
                original: DisplayIdx(3),
                snapped: DisplayIdx(3),
                delta_frames: 0,
            },
        }];

        let starts = segment_video_source_starts(&snapped, &order, &video_samples)
            .expect("表示順1に対応するデコード順が見つかるはず");

        // decode2 の DTS = duration[0] + duration[1] = 2000（dts(0)=0 を起点に累積）。
        assert_eq!(starts, vec![2000]);

        // 合成時刻(PTS相当) は dts + cts_offset = 2000 + (-1000) = 1000 であり、
        // DTS(2000) とは異なる。ここで合成時刻(1000)を返すと罠4のずれが起きる。
        assert_ne!(starts[0], 1000, "合成時刻(PTS相当)を返してはいけない(罠4)");
    }
}
