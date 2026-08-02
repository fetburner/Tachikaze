//! `auto` サブコマンド: `prepare`(#58) → `analyze` → gate 判定(#61) →
//! `cut`(区間マップ込み、#57) → `remap-subs`(#59) を対話なしで合成する（#62）。
//!
//! `docs/architecture.md`「モジュール構成」の「`commands.rs` は各モジュールを繋ぐ
//! 組み立て（アルゴリズムは持たない）」と同じ方針で書く。このモジュール自身も
//! アルゴリズムを持たない: 実際の処理はすべて `prepare::run` / `analyze::run` /
//! `gate::evaluate` / `commands::execute_cut` / `subtitle::remap_ass` /
//! `subtitle::remap_srt` に委ねる。
//!
//! ## `analyze` / `cut` のロジックを複製しない（CLAUDE.md の罠）
//!
//! - **analyze**: `analyze::run` と `gate::evaluate` はどちらも既に `pub` な純粋関数
//!   （mp4 を読まない、`docs/architecture.md`「解析側は mp4 の読み込みに依存しない」
//!   参照）なので、ここから直接呼ぶだけで済む。`commands::run_analyze`（CLI ハンドラ）
//!   を経由しないのは、あちらは `--report` の表示という `analyze` サブコマンド固有の
//!   UI 都合を持つのに対し、`auto` は「gate の判定結果を見て cut するかどうかを
//!   自分で決める」という異なる制御フローを持つため（呼び出し方自体が違う）。
//! - **cut**: `cut` の実体（`CutPipeline` の組み立て、`--cm-output` 時の自己検証8・
//!   atomic rename、区間マップの書き出し）は `src/commands.rs` の `execute_cut` に
//!   ある。ここを複製すると `cut` 単体のバグ修正が `auto` に伝播しなくなる
//!   （これが、ロジックを複製していたシェルラッパー `scripts/tachikaze-cmcut` を
//!   このエピックで削除した理由そのもの、CLAUDE.md「罠」参照）。そのため
//!   `commands::execute_cut` を `pub(crate)` にしてそのまま呼ぶ（`commands.rs`
//!   の doc comment参照）。
//!
//! ## キャッシュを短絡しない
//!
//! `trim.avs` / `.dtvi` が既にキャッシュにあっても `analyze::run` を必ず呼ぶ
//! （`analyze` のスキップは行わない）。キャッシュキーは入力の絶対パスの
//! ハッシュだけ（`workdir::cache_dir_for_input`）であり、同じパスに別内容の
//! ファイルが置かれた場合（録画ファイルの再利用・上書きなど）を区別できない。
//! 短絡を入れるなら「入力の size + mtime がキャッシュ作成時と一致するか」を
//! 照合する仕組みが前提になるが、そのような照合はまだ無い。実装するとしても
//! 「古い `.dtvi` を新しい入力に対して誤って使う」という、CLAUDE.md の最優先事項
//! （静かに壊れる）に直結するリスクを持つ機能なので、要求されていない現時点では
//! 追加しない。`auto` がキャッシュを使うのは「最終成果物（本編/CM側/字幕）が
//! 既に存在するか」の判定（後述の `--overwrite`）だけであり、これは常に
//! 実ファイルの存在を直接見るため上記のリスクを持たない。
//!
//! ## `--force` の範囲
//!
//! `--force` は gate の「疑わしいので止める」判定だけを無視する。自己検証
//! （`docs/architecture.md`「自己検証」節の1〜8）や `.dtvi` 必須（CLAUDE.md 罠3）は
//! `execute_cut` の内部でそのまま実行される（`auto` はそれらを迂回する経路を
//! 持たない）。
//!
//! ## 既存出力のスキップと `--overwrite`
//!
//! 入力ごとに、最終成果物になり得るパス（本編 `*_CMcut.mp4` / CM側 `*_CM.mp4`
//! （`--no-cm` 時は対象外）/ 字幕サイドカー `*_CMcut.ass` と `*_CMcut.srt`
//! （`--no-subtitles` 時は対象外、拡張子は実際に抽出されるまで確定しないため
//! 両方を候補にする））のいずれかが既に存在すれば、`--overwrite` が無い限り
//! その入力の処理全体をスキップする（バッチの再実行で成果物を黙って
//! 潰さないため）。判定は実処理（`prepare`/`analyze`/`cut`)を始める前に行うため、
//! 800MB 級の重い処理を無駄に行わない。
//!
//! **例外: 字幕が必要なのに欠けている場合はスキップしない（レビュー指摘#2）。**
//! `remap_subtitles` は `cut` が本編・CM側を最終パスへ rename した**後**に走る
//! ため、字幕の張り替えに失敗すると本編/CM側だけが最終パスに残った状態で
//! この入力全体が失敗扱いになる（字幕を黙って落とさないための意図的な仕様、
//! 下記「字幕の張り替えと失敗時の扱い」参照）。この状態で次回バッチを
//! 再実行すると、上記の素朴な判定では「本編がある」だけでスキップと判定されて
//! しまい、**字幕が永久に欠落したまま「完了」扱いになる**（実処理を始める前の
//! ファイル存在チェックだけを見ているため、前回が完走したのか途中で
//! 失敗したのかを区別できない）。これを防ぐため、[`input_has_subtitle_track`]
//! で入力の `moov` を軽量に読み（`prepare::run` は呼ばず、ffmpeg も起動しない）
//! 字幕トラックの有無を判定し、「字幕が必要なのに字幕サイドカー出力が無い」場合は
//! 他の出力が揃っていてもスキップせず、パイプライン全体
//! （prepare/analyze/cut/remap-subs）を再実行する。`prepare`/`cut` は同じ入力に
//! 対して常に同じ出力を作る設計（`prepare.rs`/`workdir.rs` の doc comment）なので、
//! 本編・CM側の再生成は無駄ではあるが安全（内容は変わらない）。

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context};

use crate::{analyze, cli, commands, gate, mp4io, prepare, report, segmap, subtitle, workdir};

/// `auto` サブコマンドの設定（`src/cli.rs::Commands::Auto` の CLI 引数と1対1）。
#[derive(Debug, Clone)]
pub struct AutoConfig {
    /// `--cache-dir`（キャッシュの根）。複数入力でも共通のまま使える
    /// （根だけを差し替えるオプションで、入力ごとのサブディレクトリは
    /// 常にハッシュから決まるため、複数入力時に個別指定できない
    /// `output` / `cm_output` とは事情が異なる）。
    pub cache_dir: Option<PathBuf>,
    /// 単一入力時のみ有効。複数入力時は `run` が事前に拒否する。
    pub output: Option<PathBuf>,
    /// 単一入力時のみ有効。`no_cm` とは併用できない。
    pub cm_output: Option<PathBuf>,
    pub no_cm: bool,
    pub force: bool,
    pub overwrite: bool,
    pub analyze_only: bool,
    pub no_subtitles: bool,
    pub snap: cli::Snap,
    pub verify: bool,
    pub jl_file: Option<PathBuf>,
    pub jls_set: Vec<String>,
}

/// 入力1本の処理結果（失敗は `anyhow::Result::Err` で表す。`run` 側で集計する）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InputStatus {
    Completed,
    GateStopped,
    Skipped,
}

/// バッチ全体の集計。「完了 N / 判定で停止 M / 失敗 K / スキップ L」の内訳
/// （issue #62 完了条件「複数入力で1本失敗しても残りが処理され、最後に内訳が出る」）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BatchTally {
    pub completed: usize,
    pub gate_stopped: usize,
    pub failed: usize,
    pub skipped: usize,
}

impl BatchTally {
    fn total(&self) -> usize {
        self.completed + self.gate_stopped + self.failed + self.skipped
    }
}

/// `auto` バッチ全体を実行する。1本の失敗で残りを止めない
/// （入力ごとに panic ではなく通常の `Result` で失敗を隔離する）。
///
/// 複数入力時に `-o` / `--cm-output` / `--work-dir` を使えない、`--no-cm` と
/// `--cm-output` を併用できない、`--snap inward` を既定の CM 側出力
/// （`--no-cm` 未指定）と併用できない、といった**静的な設定の誤り**は、
/// どの入力の処理も始める前に検出してすぐ `Err` を返す（1本目の重い処理が
/// 走ってから初めて設定ミスが分かる事態を避ける）。
///
/// 個々の入力の失敗はここでは `Err` にしない（呼び出し元の [`crate::commands`]
/// が `BatchTally::failed` を見て exit code を決める。`ExitOutcome` の doc
/// comment参照）。
pub fn run(config: &AutoConfig, inputs: &[PathBuf]) -> anyhow::Result<BatchTally> {
    anyhow::ensure!(!inputs.is_empty(), "入力 mp4 を指定してください");

    if inputs.len() > 1 {
        anyhow::ensure!(
            config.output.is_none(),
            "複数入力時は -o を指定できません（各入力の隣に既定名 *_CMcut.mp4 で出力します）"
        );
        anyhow::ensure!(
            config.cm_output.is_none(),
            "複数入力時は --cm-output を指定できません（各入力の隣に既定名 *_CM.mp4 で出力します）"
        );
        // `--cache-dir` はキャッシュの根だけを指すオプションで、入力ごとの
        // サブディレクトリは常に絶対パスのハッシュから決まる（`workdir.rs` の
        // doc comment参照）。そのため `-o` / `--cm-output` と違い、複数入力でも
        // 共通のまま使えるので拒否しない。
    }

    anyhow::ensure!(
        !(config.no_cm && config.cm_output.is_some()),
        "--no-cm と --cm-output は併用できません"
    );

    // `auto` は既定で CM 側出力を付ける（`--no-cm` を指定しない限り）。`--snap inward`
    // は保持区間を退化させうるため `--cm-output` と併用できない
    // （`commands::execute_cut` にも同じ検査がある。ここでは「auto は既定で
    // --cm-output 相当を付ける」という auto 固有の文脈に沿ったメッセージにする
    // ため、`execute_cut` の一般的なメッセージより前に、auto の語彙で説明する
    // （issue #62「罠」8）。
    if config.snap == cli::Snap::Inward && !config.no_cm {
        bail!(
            "--snap inward は、auto が既定で付ける CM 側出力（*_CM.mp4 / --cm-output）と \
             併用できません。inward スナップでは保持区間が退化しうるため、CM 側（補集合）の \
             区間の順序も壊れます。--no-cm を付けるか、--snap を既定の outward のままにして \
             ください。"
        );
    }

    let jls_set = config
        .jls_set
        .iter()
        .map(|raw| analyze::parse_jls_set_arg(raw))
        .collect::<anyhow::Result<Vec<_>>>()?;

    let mut tally = BatchTally::default();
    for input in inputs {
        println!("\n======== {} ========", input.display());
        match process_one(config, &jls_set, input) {
            Ok(InputStatus::Completed) => tally.completed += 1,
            Ok(InputStatus::GateStopped) => tally.gate_stopped += 1,
            Ok(InputStatus::Skipped) => tally.skipped += 1,
            Err(err) => {
                eprintln!("[auto] エラー: {}: {err:?}", input.display());
                tally.failed += 1;
            }
        }
    }

    println!(
        "\n完了 {} / 判定で停止 {} / 失敗 {} / 既存出力のためスキップ {}（計 {} 件）",
        tally.completed,
        tally.gate_stopped,
        tally.failed,
        tally.skipped,
        tally.total()
    );

    Ok(tally)
}

/// 入力1本を処理する。戻り値は成功時の状態（[`InputStatus`]）、失敗時は
/// `anyhow::Error`（呼び出し元の `run` が握りつぶしてタリーに反映する）。
fn process_one(
    config: &AutoConfig,
    jls_set: &[(String, String)],
    input: &Path,
) -> anyhow::Result<InputStatus> {
    anyhow::ensure!(input.is_file(), "入力がありません: {}", input.display());

    let out_path = config
        .output
        .clone()
        .unwrap_or_else(|| sibling_output_path(input, "_CMcut", "mp4"));
    let cm_out_path = if config.no_cm {
        None
    } else {
        Some(
            config
                .cm_output
                .clone()
                .unwrap_or_else(|| sibling_output_path(input, "_CM", "mp4")),
        )
    };

    // 既存出力のスキップ判定（本モジュール冒頭の doc comment参照）。実処理を始める
    // 前に行うことで、800MB級の重い処理を無駄にしない。
    let mut existing = Vec::new();
    if out_path.is_file() {
        existing.push(out_path.clone());
    }
    if let Some(cm) = &cm_out_path {
        if cm.is_file() {
            existing.push(cm.clone());
        }
    }
    let subs_existing: Vec<PathBuf> = if config.no_subtitles {
        Vec::new()
    } else {
        ["ass", "srt"]
            .into_iter()
            .map(|ext| commands::default_remap_subs_output_path(input, ext))
            .filter(|p| p.is_file())
            .collect()
    };
    existing.extend(subs_existing.iter().cloned());

    // 「字幕が必要なのに字幕サイドカー出力が無い」場合は、他の出力(本編/CM側)が
    // 揃っていてもスキップしない（本モジュール冒頭 doc comment「既存出力のスキップと
    // --overwrite」の例外、レビュー指摘#2）。前回 remap-subs が失敗して本編/CM側だけ
    // 残った状態を、次回バッチ再実行で自動的に再試行できるようにするため。
    let subs_missing_but_expected =
        !config.no_subtitles && subs_existing.is_empty() && input_has_subtitle_track(input);

    if subs_missing_but_expected {
        println!(
            "[auto] 本編/CM側の出力はありますが、字幕サイドカーが見つかりません\
             （前回 remap-subs が失敗した可能性があります）。スキップせず再試行します: {}",
            input.display()
        );
    } else if !existing.is_empty() && !config.overwrite {
        println!(
            "[auto] 既存の出力があるためスキップします（--overwrite で上書き）: {}",
            existing
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
        return Ok(InputStatus::Skipped);
    }

    // 1. prepare（elst 除去・字幕抽出。#58）。`auto` は外部字幕（--subs 相当）を
    // 受け付けない（issue #62 の CLI 一覧に無い）ため常に `None`。
    println!("[auto] prepare: {}", input.display());
    let prepare_outcome = prepare::run(input, config.cache_dir.as_deref(), None)
        .with_context(|| format!("prepare に失敗しました: {}", input.display()))?;
    let media_path = prepare_outcome.media_path.clone();
    if prepare_outcome.ran_ffmpeg {
        println!(
            "[auto] prepare 完了（elst除去={}）: {}",
            prepare_outcome.had_edit_list,
            media_path.display()
        );
    } else {
        println!("[auto] prepare 不要: 入力をそのまま使います");
    }

    // 中間ファイル（.dtvi / trim.avs）の置き場所を、analyze を呼ぶ前に確定させる。
    // `analyze::run` 自身も同じ入力（media_path）・同じ `--cache-dir` から同一の
    // ディレクトリを導出するため（`workdir::WorkDir::new` は同じ引数に対して
    // 同じキャッシュディレクトリを返す。冪等）、ここで一度作っても競合しない。
    // `commands::resolve_dtvi_path` はキャッシュの既定パスしか見ないため、
    // ここで確定させたパスを cut にも明示的に渡す。
    let work_probe =
        workdir::WorkDir::new(config.cache_dir.as_deref(), &media_path).with_context(|| {
            format!(
                "作業ディレクトリの解決に失敗しました: {}",
                media_path.display()
            )
        })?;
    let trim_path = work_probe.trim_path();
    let dtvi_path = work_probe.dtvi_path();

    // 2. analyze（キャッシュを短絡せず必ず呼ぶ。本モジュール冒頭の doc comment参照）。
    let analyze_config = analyze::AnalyzeConfig {
        input: media_path.clone(),
        output: trim_path.clone(),
        cache_dir: config.cache_dir.clone(),
        jls_set: jls_set.to_vec(),
        jl_file: config.jl_file.clone(),
    };
    println!("[auto] analyze: {}", media_path.display());
    let analyze_output = analyze::run(&analyze_config)
        .with_context(|| format!("analyze に失敗しました: {}", media_path.display()))?;
    println!("trim.avs を書き出しました: {}", trim_path.display());

    // 3. gate 判定（#61）。`analyze --report` と同じ情報を表示する
    // （gate.rs の doc comment「auto を使わない人間も同じ情報を見られるよう」）。
    let fps = commands::fps_from_dtvi(&analyze_output.dtvi);
    let missed =
        report::missed::find_missed_candidates(&analyze_output.trim, &analyze_output.jls_entries);
    if !missed.is_empty() {
        println!("見逃し候補の警告:");
        for candidate in &missed {
            println!("{}", report::missed::format_warning(candidate, fps));
        }
    }
    let verdict = gate::evaluate(
        &analyze_output.trim,
        &analyze_output.jls_entries,
        &analyze_output.dtvi,
    );
    println!("{}", gate::format_gate_report(&verdict));

    let cut_hint = || {
        let cm_flag = cm_out_path
            .as_ref()
            .map(|p| format!(" --cm-output {}", p.display()))
            .unwrap_or_default();
        format!(
            "tachikaze cut {} --trim {} --dtvi {} -o {}{cm_flag}",
            media_path.display(),
            trim_path.display(),
            dtvi_path.display(),
            out_path.display(),
        )
    };

    if config.analyze_only {
        println!(
            "[auto] --analyze-only のため、ここで停止します（cut / remap-subs は実行しません）。"
        );
        println!("  trim: {}", trim_path.display());
        println!("  dtvi: {}", dtvi_path.display());
        println!("  続きは次のコマンドで cut できます:");
        println!("    {}", cut_hint());
        return Ok(if verdict.stop {
            InputStatus::GateStopped
        } else {
            InputStatus::Completed
        });
    }

    if verdict.stop {
        if config.force {
            println!("[auto] --force が指定されているため、gate の停止判定を無視して続行します。");
        } else {
            println!("[auto] gate が疑わしいと判定したため、cut を実行せず停止します。");
            println!("  trim: {}", trim_path.display());
            println!(
                "  内容を確認し、必要なら trim.avs を直してから次のコマンドで cut してください:"
            );
            println!("    {}", cut_hint());
            return Ok(InputStatus::GateStopped);
        }
    }

    // 4. cut（区間マップ込み、#57）。`commands::execute_cut` をそのまま呼ぶ
    // （本モジュール冒頭の doc comment「cut のロジックを複製しない」）。
    println!(
        "[auto] cut: {} -> {}",
        media_path.display(),
        out_path.display()
    );
    let cut_outcome = commands::execute_cut(commands::CutParams {
        cache_dir: config.cache_dir.clone(),
        input: media_path.clone(),
        trim_path: trim_path.clone(),
        output: out_path.clone(),
        snap: config.snap,
        video_only: false,
        verify_with_ffprobe: config.verify,
        dtvi_path: Some(dtvi_path.clone()),
        cm_output: cm_out_path.clone(),
        segment_map_path: None,
    })
    .with_context(|| format!("cut に失敗しました: {}", media_path.display()))?;

    commands::print_cut_report(
        "cut 完了",
        "保持区間数",
        &cut_outcome.output,
        &cut_outcome.report,
    );
    if let (Some(cm), Some(cm_report)) = (&cut_outcome.cm_output, &cut_outcome.cm_report) {
        commands::print_cut_report("CM 出力完了", "CM区間数", cm, cm_report);
    }

    // 5. remap-subs（#59）。字幕の張り替えは既定でハードエラーにする
    // （issue #62「やること」7: 本編だけ出して字幕を黙って落とさない）。
    // `--no-subtitles` のときは抽出済みの字幕サイドカーがあっても張り替えない。
    if config.no_subtitles {
        println!("[auto] --no-subtitles のため字幕の張り替えは行いません。");
    } else if let Some(subs_input) = &prepare_outcome.subtitle_path {
        if let Err(err) =
            remap_subtitles(config.cache_dir.as_deref(), &media_path, subs_input, input)
        {
            // 本編/CM側は既に最終パスへ rename 済み（失敗しても削除しない、
            // モジュール冒頭 doc comment参照）。この入力は従来どおり失敗扱いに
            // するが（issue #62「やること」7: 本編だけ出して字幕を黙って落とさない）、
            // 次回バッチ実行時に何が起きるかを明示する（レビュー指摘#2）。
            eprintln!(
                "[auto] 警告: 本編{}の出力は完了していますが、字幕の張り替えに失敗しました。\
                 字幕サイドカーが未作成のため、次回このバッチを（--overwrite なしでも）\
                 再実行すると自動的に再試行します: {}",
                if cm_out_path.is_some() { "/CM側" } else { "" },
                input.display()
            );
            return Err(err)
                .with_context(|| format!("字幕の張り替えに失敗しました: {}", input.display()));
        }
    } else {
        println!("[auto] 字幕トラックが無いため remap-subs は行いません。");
    }

    log_disk_usage(&prepare_outcome, &out_path, cm_out_path.as_deref());

    println!("[auto] 完了: {}", out_path.display());
    if let Some(cm) = &cm_out_path {
        println!(
            "[auto] CM側: {}（本編が混ざっていないか目視推奨）",
            cm.display()
        );
    }
    println!(
        "[auto] 注意: gate が止めなかったことは検出が完全に当たっている保証ではありません \
         （見逃し候補ヒューリスティックの限界、gate.rs の doc comment参照）。"
    );

    Ok(InputStatus::Completed)
}

/// cut が書いた区間マップ（`workdir::cached_segment_map_path`）を読み、
/// `prepare` が抽出した字幕サイドカーを cut 後のタイムラインへ張り替えて
/// `commands::default_remap_subs_output_path` の既定パスへ書き出す。
///
/// `remap-subs` サブコマンド（`commands::run_remap_subs`）と同じ処理を、
/// パス解決（キャッシュ探索）を経由せず直接行う: `auto` は区間マップと字幕の
/// 場所を既に知っている（このバッチ内で `cut` と `prepare` を呼んだ直後）ため、
/// `resolve_segment_map_path` / `resolve_subs_path` のキャッシュ自動探索
/// （他人が置いた古いキャッシュを拾う可能性がある）を経由する必要が無い。
fn remap_subtitles(
    cache_dir: Option<&Path>,
    media_path: &Path,
    subs_input: &Path,
    original_input: &Path,
) -> anyhow::Result<()> {
    let segment_map_path = workdir::cached_segment_map_path(cache_dir, media_path)
        .context("区間マップのキャッシュパス解決に失敗しました")?;
    let segment_map_json = fs::read_to_string(&segment_map_path).with_context(|| {
        format!(
            "区間マップの読み込みに失敗しました（cut が書き出しているはず）: {}",
            segment_map_path.display()
        )
    })?;
    let segment_map = segmap::SegmentMap::from_json(&segment_map_json)
        .map_err(|err| anyhow!("区間マップのパースに失敗しました: {err}"))?;

    let format = subtitle::SubsFormat::from_path(subs_input).ok_or_else(|| {
        anyhow!(
            "字幕ファイルの拡張子から形式を判定できません: {}",
            subs_input.display()
        )
    })?;
    let subs_content = fs::read_to_string(subs_input).with_context(|| {
        format!(
            "字幕サイドカーの読み込みに失敗しました: {}",
            subs_input.display()
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
    .map_err(|err| anyhow!("{err}"))?;

    let subs_out = commands::default_remap_subs_output_path(original_input, format.extension());
    fs::write(&subs_out, &remap_output.content)
        .with_context(|| format!("字幕の書き出しに失敗しました: {}", subs_out.display()))?;

    let stats = &remap_output.stats;
    println!(
        "[auto] remap-subs 完了: {}（シフト {} 件 / 破棄 {} 件 / クリップ {} 件）",
        subs_out.display(),
        stats.shifted,
        stats.discarded,
        stats.clipped
    );
    for warning in &stats.warnings {
        eprintln!("[auto][remap-subs] {warning}");
    }

    Ok(())
}

/// 入力 mp4 に字幕トラックがあるかどうかを、`prepare::run` を呼ばず（＝ffmpeg を
/// 起動せず）軽量に判定する。`moov` を読むだけなので800MB級の入力でも安い
/// （既存出力のスキップ判定「字幕が必要なのに無い」の検出に使う。本モジュール
/// 冒頭の doc comment「既存出力のスキップと --overwrite」参照）。入力が読めない
/// 場合は `false` を返す（どのみち後続の `prepare::run` が同じ入力に対して同じ
/// エラーで失敗するため、ここでエラーの経路を増やす必要が無い）。
fn input_has_subtitle_track(input: &Path) -> bool {
    match mp4io::read::read_moov(input) {
        Ok(moov) => prepare::inspect_moov(&moov).subtitle.is_some(),
        Err(_) => false,
    }
}

/// `input` の隣に `<stem><suffix>.<ext>` という名前のパスを作る
/// （`*_CMcut.mp4` / `*_CM.mp4` の既定命名規則。かつて存在したシェルラッパー
/// `scripts/tachikaze-cmcut` の `default_out_path` / `default_cm_path` も
/// 同じ規則だった。`auto` の追加に伴い削除済み、`[E11-7]`）。
fn sibling_output_path(input: &Path, suffix: &str, ext: &str) -> PathBuf {
    let dir = input.parent().map(Path::to_path_buf).unwrap_or_default();
    let stem = input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("output");
    dir.join(format!("{stem}{suffix}.{ext}"))
}

/// 完了時に、何がどこに残るか（ディスク使用量）をログへ出す
/// （issue #62「罠」: 800MB級 × 保持側 + CM側 + prepare済み中間物）。
///
/// サイズ取得に失敗しても処理は続ける（ログ用の補助情報のため、失敗を
/// エラーにする必要が無い）。
fn log_disk_usage(
    prepare_outcome: &prepare::PrepareOutcome,
    out_path: &Path,
    cm_out_path: Option<&Path>,
) {
    println!("[auto] ディスク使用量:");
    if prepare_outcome.ran_ffmpeg {
        log_file_size(
            "  prepare 済み中間物（キャッシュ、自動削除しません）",
            &prepare_outcome.media_path,
        );
    }
    log_file_size("  本編", out_path);
    if let Some(cm) = cm_out_path {
        log_file_size("  CM側", cm);
    }
}

fn log_file_size(label: &str, path: &Path) {
    match fs::metadata(path) {
        Ok(meta) => {
            let mb = meta.len() as f64 / 1_048_576.0;
            println!("{label}: {} ({mb:.1} MB)", path.display());
        }
        Err(err) => println!("{label}: {} (サイズ取得失敗: {err})", path.display()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_has_subtitle_track_returns_false_for_unreadable_input() {
        // mp4 として読めない(そもそも存在しない)入力は false を返す
        // (後続の prepare::run が同じ理由で失敗するため、ここでは判定不能を
        // false として扱う。本関数の doc comment参照)。
        assert!(!input_has_subtitle_track(Path::new(
            "/nonexistent-for-auto-test.mp4"
        )));
    }

    #[test]
    fn sibling_output_path_uses_stem_and_suffix() {
        let input = Path::new("/rec/番組.mp4");
        assert_eq!(
            sibling_output_path(input, "_CMcut", "mp4"),
            PathBuf::from("/rec/番組_CMcut.mp4")
        );
        assert_eq!(
            sibling_output_path(input, "_CM", "mp4"),
            PathBuf::from("/rec/番組_CM.mp4")
        );
    }

    #[test]
    fn batch_tally_total_sums_all_categories() {
        let tally = BatchTally {
            completed: 2,
            gate_stopped: 1,
            failed: 1,
            skipped: 3,
        };
        assert_eq!(tally.total(), 7);
    }

    #[test]
    fn run_rejects_empty_inputs() {
        let config = AutoConfig {
            output: None,
            cm_output: None,
            no_cm: false,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Outward,
            verify: false,
            jl_file: None,
            jls_set: vec![],
            cache_dir: None,
        };
        let err = run(&config, &[]).expect_err("空の入力リストは拒否するはず");
        assert!(err.to_string().contains("入力"));
    }

    #[test]
    fn run_rejects_multiple_inputs_with_explicit_output() {
        let config = AutoConfig {
            output: Some(PathBuf::from("/tmp/out.mp4")),
            cm_output: None,
            no_cm: false,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Outward,
            verify: false,
            jl_file: None,
            jls_set: vec![],
            cache_dir: None,
        };
        let inputs = vec![PathBuf::from("/a.mp4"), PathBuf::from("/b.mp4")];
        let err = run(&config, &inputs).expect_err("複数入力 + -o は拒否するはず");
        assert!(err.to_string().contains("-o"));
    }

    /// 完了条件: `--cache-dir` はキャッシュの根だけを指すオプションなので、
    /// `-o` / `--cm-output`（削除済みの `--work-dir` はこちらの仲間だった）とは
    /// 違い、複数入力でも拒否されない。
    #[test]
    fn run_allows_multiple_inputs_with_explicit_cache_dir() {
        let config = AutoConfig {
            cache_dir: Some(PathBuf::from("/tmp/cache")),
            output: None,
            cm_output: None,
            no_cm: false,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Outward,
            verify: false,
            jl_file: None,
            jls_set: vec![],
        };
        let inputs = vec![
            PathBuf::from("/nonexistent-for-auto-test-a.mp4"),
            PathBuf::from("/nonexistent-for-auto-test-b.mp4"),
        ];
        // `--cache-dir` を理由にした事前拒否はされない（入力自体が無いので
        // 個々の処理は failed になるが、静的検証は通る）。
        let tally = run(&config, &inputs).expect("--cache-dir は複数入力でも拒否されないはず");
        assert_eq!(tally.failed, 2);
    }

    #[test]
    fn run_rejects_no_cm_and_cm_output_together() {
        let config = AutoConfig {
            output: None,
            cm_output: Some(PathBuf::from("/tmp/cm.mp4")),
            no_cm: true,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Outward,
            verify: false,
            jl_file: None,
            jls_set: vec![],
            cache_dir: None,
        };
        let inputs = vec![PathBuf::from("/a.mp4")];
        let err = run(&config, &inputs).expect_err("--no-cm と --cm-output の併用は拒否するはず");
        assert!(err.to_string().contains("--no-cm"));
    }

    #[test]
    fn run_rejects_snap_inward_with_default_cm_output() {
        let config = AutoConfig {
            output: None,
            cm_output: None,
            no_cm: false,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Inward,
            verify: false,
            jl_file: None,
            jls_set: vec![],
            cache_dir: None,
        };
        let inputs = vec![PathBuf::from("/a.mp4")];
        let err =
            run(&config, &inputs).expect_err("--snap inward は既定の CM 側出力と併用できないはず");
        assert!(err.to_string().contains("--snap inward"));
    }

    #[test]
    fn run_allows_snap_inward_with_no_cm() {
        // `--no-cm` があれば `--snap inward` の併用禁止には抵触しない。この場合、
        // 入力ファイルが実在しないため別のエラー（"入力がありません"）にはなるが、
        // 「--snap inward」を理由にした事前拒否はされないことを確認する。
        let config = AutoConfig {
            output: None,
            cm_output: None,
            no_cm: true,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Inward,
            verify: false,
            jl_file: None,
            jls_set: vec![],
            cache_dir: None,
        };
        let inputs = vec![PathBuf::from("/nonexistent-for-auto-test.mp4")];
        let tally = run(&config, &inputs).expect("事前の静的検証は通るはず（入力自体は無い）");
        assert_eq!(tally.failed, 1, "存在しない入力なので failed が1件のはず");
    }

    #[test]
    fn run_rejects_invalid_jls_set_before_processing_any_input() {
        let config = AutoConfig {
            output: None,
            cm_output: None,
            no_cm: false,
            force: false,
            overwrite: false,
            analyze_only: false,
            no_subtitles: false,
            snap: cli::Snap::Outward,
            verify: false,
            jl_file: None,
            jls_set: vec!["not-key-value".to_string()],
            cache_dir: None,
        };
        let inputs = vec![PathBuf::from("/nonexistent-for-auto-test.mp4")];
        let err = run(&config, &inputs).expect_err("--jls-set の形式不正は事前に拒否するはず");
        assert!(err.to_string().contains("KEY=VALUE"));
    }
}
