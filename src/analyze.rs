//! analyze コマンド: `dtvindex build` → `chapter_exe -v` → `join_logo_scp` の
//! 3 ツールパイプラインを実行し、`trim.avs` を生成する。
//!
//! 処理の流れ（`docs/pipeline.md` の「全体像」節）:
//!
//! ```text
//! work.mp4 (入力への symlink)
//!   ├─ dtvindex build work.mp4 -o work.mp4.dtvi
//!   ├─ chapter_exe -v work.mp4 -o scp.txt
//!   ├─ (--logo 指定時のみ) .lgd を読み、ffmpeg でロゴ矩形のフレームを流して
//!   │      ロゴ表示区間を判定し、閾値以上なら logoframe.txt を書く（[`detect_logo`]）
//!   └─ join_logo_scp -inscp scp.txt -incmd <JL command file> \
//!          [-inlogo logoframe.txt] \
//!          -o trim.avs -oscp detail.jls -set autocm_sub 11 -set param_cuttr 1
//! ```
//!
//! `-inlogo` は既定では付けない。E14-2（`docs/measurements.md`「ロゴの残存」）で
//! 判明したとおり、対象素材は delogo 済みでもロゴが実際には残っている場合がある
//! ため、`--logo <path>`（`.lgd`、`make-logo` で作る）を指定したときだけ自前の
//! ロゴ検出（`crate::logo`）を通す。検出フレーム割合が閾値未満、または
//! logoframe の出力が空（[`inlogo_decision`] 参照）ならフォールバックして
//! `-inlogo` を渡さない（[`detect_logo`] の doc comment参照、issue #97）。
//! `--logo` を省略、またはフォールバックした場合は従来どおり付けず、join_logo_scp は
//! 全フレームをロゴ表示中とみなす。

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::dtvi::{self, Dtvi};
use crate::errctx::PathContext;
use crate::external;
use crate::jls::{self, JlsEntry};
use crate::logo::frames::{self, LogoRect, VideoSize};
use crate::logo::lgd::{self, LogoData};
use crate::logo::{interval as logo_interval, score};
use crate::tools::{self, CHAPTER_EXE, DTVINDEX, FFMPEG, JOIN_LOGO_SCP};
use crate::trim::TrimList;
use crate::workdir::WorkDir;

/// join_logo_scp に既定で渡す `-set KEY VALUE`（根拠は `docs/jls-settings.md`）。
///
/// - `autocm_sub=11`: 既定の `10` では「先頭 15 秒単位構成は少数でも CM 化」が
///   無効なままで、番組冒頭の CM 30 秒が残る。
/// - `param_cuttr=1`: 既定の `0` では番宣が `Trailer(cut-cancel)` として残る。
///   `1` にすると末尾 50 秒が除去される。
const DEFAULT_JLS_SET: &[(&str, &str)] = &[("autocm_sub", "11"), ("param_cuttr", "1")];

/// `--logo` 指定時のロゴ検出割合フォールバック閾値（Amatsukaze
/// `CMAnalyze.hpp:301` と同じ規則）。検出フレーム割合がこの値未満なら
/// `-inlogo` を渡さない（誤ったロゴ情報で判定を崩すより現状維持に倒す、
/// issue #97「解くべき問題」）。
const LOGO_DETECTION_THRESHOLD: f64 = 0.1;
/// 映像長が [`LOGO_DETECTION_SHORT_VIDEO_SECONDS`] 以下の場合に使う、
/// 緩めたフォールバック閾値（同じく `CMAnalyze.hpp:301` の規則）。
const LOGO_DETECTION_THRESHOLD_SHORT: f64 = 0.03;
/// 上記の緩い閾値を使う映像長の上限（7分、秒単位）。
const LOGO_DETECTION_SHORT_VIDEO_SECONDS: f64 = 7.0 * 60.0;

/// analyze コマンドの実行に必要な設定。
#[derive(Debug, Clone)]
pub struct AnalyzeConfig {
    /// 入力 mp4 ファイル。
    pub input: PathBuf,
    /// `trim.avs` をキャッシュ以外にも書き出すパス。`None` ならキャッシュ
    /// （[`AnalyzeOutput::cache_trim_path`]）にだけ書く。標準出力へ書く場合
    /// （CLI の `-o -`）はここに渡さず、呼び出し側が [`AnalyzeOutput::raw_trim`]
    /// を使う（`-` という名前のファイルを作ってしまわないため、CLAUDE.md の罠）。
    pub output: Option<PathBuf>,
    /// `--cache-dir`（キャッシュの根）。未指定なら既定値
    /// （`workdir::cache_root` の doc comment参照）。
    pub cache_dir: Option<PathBuf>,
    /// `--jls-set` で上書き・追加された `(KEY, VALUE)`。
    pub jls_set: Vec<(String, String)>,
    /// `--jl-file`。未指定なら `tools::default_jl_command_file` の既定値を使う。
    pub jl_file: Option<PathBuf>,
    /// `--logo`（ロゴ検出に使う `.lgd`）。`None` なら従来どおりロゴ無しの経路
    /// のまま（1バイトも変わらない、issue #97「解くべき問題」）。
    pub logo: Option<PathBuf>,
}

/// analyze パイプライン全体の成果物。
///
/// `--report`（境界とキーフレームの距離、見逃し候補の警告）の組み立てに
/// `.dtvi` と `detail.jls` の内容が必要なため、キャッシュディレクトリ
/// （[`WorkDir`]）が片付けられる前にここで読み込んでおく。
#[derive(Debug, Clone)]
pub struct AnalyzeOutput {
    /// 生成された `trim.avs` のパース結果。
    pub trim: TrimList,
    /// `dtvindex build` が生成した `.dtvi` の内容。
    pub dtvi: Dtvi,
    /// `join_logo_scp -oscp` が生成した `detail.jls` の内容。
    pub jls_entries: Vec<JlsEntry>,
    /// `trim.avs` の生の内容（`join_logo_scp -o` が書いたバイト列そのまま）。
    /// CLI の `-o -` で標準出力に書くときに使う（`trim.to_string()` で
    /// 再構成すると、パース→再組み立てで元のバイト列と一致する保証が無いため）。
    pub raw_trim: String,
    /// `trim.avs` が書かれたキャッシュ内の実際のパス。`-o` 省略時に案内するため。
    pub cache_trim_path: PathBuf,
}

/// analyze パイプラインを実行し、生成された `trim.avs` / `.dtvi` / `detail.jls` を
/// パースして返す。
///
/// どこかの段階が失敗した場合、以降の段階は実行せずエラーを伝播する
/// （`external::run` のエラーにはコマンドライン全体と stderr の末尾が
/// 既に含まれている）。成功・失敗いずれの経路でも `WorkDir::finish` を
/// 呼ぶため、実際の処理は [`run_pipeline`] に分離している。
///
/// ツールの解決は `WorkDir::new` より先に行う。見つからない場合は入力ファイル
/// の存在確認や作業ディレクトリの作成（既定のキャッシュディレクトリ使用時は
/// 入力の `canonicalize` を伴う）より前に、探索場所を列挙したエラーで早期に
/// 失敗させるため（`run_propagates_tool_resolution_failure_with_searched_locations`
/// が実在しない入力パスでこの順序を検証している）。`--logo` 指定時は ffmpeg の
/// 解決と `.lgd` の存在確認もここで行う（レビュー指摘: 以前は
/// [`detect_logo`] の中、つまり dtvindex/chapter_exe の実行後にあったため、
/// `.lgd` のパスを打ち間違えても外部プロセス2本ぶん待たされてから失敗して
/// いた）。
pub fn run(config: &AnalyzeConfig) -> Result<AnalyzeOutput> {
    let dtvindex_path = tools::resolve_tool(DTVINDEX)?;
    let chapter_exe_path = tools::resolve_tool(CHAPTER_EXE)?;
    let join_logo_scp_path = tools::resolve_tool(JOIN_LOGO_SCP)?;
    let ffmpeg_path = match &config.logo {
        Some(lgd_path) => {
            fs::metadata(lgd_path).path_ctx("--logo で指定された .lgd の確認", lgd_path)?;
            Some(tools::resolve_tool(FFMPEG)?)
        }
        None => None,
    };

    let work = WorkDir::new(config.cache_dir.as_deref(), &config.input)?;
    let result = run_pipeline(
        config,
        &work,
        &dtvindex_path,
        &chapter_exe_path,
        &join_logo_scp_path,
        ffmpeg_path.as_deref(),
    );
    work.finish(result.is_ok());
    result
}

fn run_pipeline(
    config: &AnalyzeConfig,
    work: &WorkDir,
    dtvindex_path: &Path,
    chapter_exe_path: &Path,
    join_logo_scp_path: &Path,
    ffmpeg_path: Option<&Path>,
) -> Result<AnalyzeOutput> {
    let work_mp4 = work.link_input(&config.input)?;
    let dtvi_path = work.dtvi_path();
    let scp_path = work.scp_path();
    let trim_avs_path = work.trim_path();
    let detail_jls_path = work.detail_jls_path();

    // `external::run` のエラーには既にコマンドライン全体と stderr の末尾が
    // 含まれているため、追加の `.context()` で包まずそのまま伝播する
    // （包むと `anyhow::Error` の `Display`（`to_string()`）が外側のメッセージ
    // だけを返し、肝心の stderr が隠れてしまう）。
    external::run(
        dtvindex_path,
        &[
            OsStr::new("build"),
            work_mp4.as_os_str(),
            OsStr::new("-o"),
            dtvi_path.as_os_str(),
        ],
        work.path(),
    )?;

    let chapter_exe_output = external::run(
        chapter_exe_path,
        &[
            OsStr::new("-v"),
            work_mp4.as_os_str(),
            OsStr::new("-o"),
            scp_path.as_os_str(),
        ],
        work.path(),
    )?;
    // macOS には AviSynth が無いため、dtvindex 入力経路が有効なビルドである必要がある
    // （docs/toolchain-macos.md）。無効なビルドを渡されると入力経路が無く静かに
    // 動かなくなるため、起動ログから読み取れる場合は警告しておく。
    if tools::dtvindex_enabled_from_output(&chapter_exe_output.stdout) == Some(false)
        || tools::dtvindex_enabled_from_output(&chapter_exe_output.stderr) == Some(false)
    {
        eprintln!(
            "警告: chapter_exe が dtvindex=disabled で起動しました。\
             macOS には AviSynth が無いため、dtvindex 入力経路が有効なビルドが必要です\
             （docs/toolchain-macos.md 参照）。"
        );
    }

    let jl_file = match &config.jl_file {
        Some(path) => fs::canonicalize(path).path_ctx("JL コマンドファイルの絶対パス解決", path)?,
        None => tools::default_jl_command_file(join_logo_scp_path)?,
    };

    // `--logo` があるときだけ動く経路（E14-8、issue #97）。フレーム数の不一致
    // （`frames::stream_luma_frames` が内部で検査する。CLAUDE.md 罠3）は
    // ここで `?` によりエラーとして中断し、この時点ではまだ join_logo_scp を
    // 起動していない（issue #97「罠」: この検査は省略可能なオプションにせず、
    // 必ず join_logo_scp の起動前に済ませる）。
    let inlogo_path = match &config.logo {
        Some(lgd_path) => {
            // `run()` が `--logo` 指定時に必ず ffmpeg を解決してから
            // `run_pipeline` を呼ぶため、ここで `None` になることは無い
            // （`run_pipeline` は `run()` からしか呼ばれない private 関数）。
            let ffmpeg_path = ffmpeg_path.ok_or_else(|| {
                anyhow!(
                    "内部エラー: --logo 指定時に ffmpeg_path が解決されていません\
                     （run() の実装を確認してください）"
                )
            })?;
            let dtvi_content_for_logo = fs::read_to_string(&dtvi_path).path_ctx(
                "dtvindex が生成した .dtvi の読み込み（ロゴ検出用）",
                &dtvi_path,
            )?;
            let dtvi_for_logo = dtvi::parse(&dtvi_content_for_logo)
                .map_err(|err| anyhow!("生成された .dtvi のパースに失敗しました: {err}"))?;
            detect_logo(
                ffmpeg_path,
                lgd_path,
                &work_mp4,
                work.path(),
                &dtvi_for_logo,
                &work.logoframe_path(),
            )?
        }
        None => None,
    };

    let mut join_logo_scp_args: Vec<&OsStr> = vec![
        OsStr::new("-inscp"),
        scp_path.as_os_str(),
        OsStr::new("-incmd"),
        jl_file.as_os_str(),
        OsStr::new("-o"),
        trim_avs_path.as_os_str(),
        OsStr::new("-oscp"),
        detail_jls_path.as_os_str(),
    ];
    // `-inlogo` は `-set` 群より前に置く（join_logo_scp はオプションを左から
    // 順に処理して同じ項目を上書きするため、issue #97「罠」）。
    if let Some(inlogo_path) = &inlogo_path {
        join_logo_scp_args.push(OsStr::new("-inlogo"));
        join_logo_scp_args.push(inlogo_path.as_os_str());
    }
    let set_args = build_jls_set_args(DEFAULT_JLS_SET, &config.jls_set);
    join_logo_scp_args.extend(set_args.iter().map(|s| OsStr::new(s.as_str())));

    external::run(join_logo_scp_path, &join_logo_scp_args, work.path())?;

    // work 内の trim.avs を先に読む。`-o` が work の trim.avs と同じ
    // パスだと `fs::copy(src, src)` が空ファイルを生む（macOS で実測。前回の
    // 手動実行で hit した）。同一パスならコピーを省略する。
    let output_content = fs::read_to_string(&trim_avs_path).path_ctx(
        "join_logo_scp が生成した trim.avs の読み込み",
        &trim_avs_path,
    )?;
    let trim = TrimList::parse(&output_content)
        .map_err(|err| anyhow!("生成された trim.avs のパースに失敗しました: {err}"))?;

    if let Some(dest) = &config.output {
        if !same_path(&trim_avs_path, dest)? {
            fs::write(dest, &output_content).path_ctx("trim.avs の書き出し", dest)?;
        }
    }

    let dtvi_content = fs::read_to_string(&dtvi_path)
        .path_ctx("dtvindex が生成した .dtvi の読み込み", &dtvi_path)?;
    let dtvi = dtvi::parse(&dtvi_content)
        .map_err(|err| anyhow!("生成された .dtvi のパースに失敗しました: {err}"))?;

    let jls_content = fs::read_to_string(&detail_jls_path).path_ctx(
        "join_logo_scp が生成した detail.jls の読み込み",
        &detail_jls_path,
    )?;
    let jls_entries = jls::parse(&jls_content)
        .map_err(|err| anyhow!("生成された detail.jls のパースに失敗しました: {err}"))?;

    Ok(AnalyzeOutput {
        trim,
        dtvi,
        jls_entries,
        raw_trim: output_content,
        cache_trim_path: trim_avs_path,
    })
}

/// `--logo` 指定時のロゴ検出経路（E14-8、issue #97「解くべき問題」）。
///
/// `.lgd` を読み、ロゴ矩形（`imgx`/`imgy`/`w`/`h`）で `work_mp4` のフレームを
/// 流し、フレームごとに `(corr0, corr1)` を評価して区間を作る。読み取った
/// フレーム数が `dtvi` の `frame_count`（ヘッダ、`dtvi.rs` の doc comment参照）と
/// 一致しなければ [`frames::stream_luma_frames`] がエラーを返し（CLAUDE.md
/// 罠3）、そのエラーはこの関数から `?` でそのまま伝播する。呼び出し元
/// （[`run_pipeline`]）はこの時点でまだ `join_logo_scp` を起動していないため、
/// この検査は必ず起動前に効く。
///
/// 検出フレーム割合が閾値（[`logo_detection_threshold`]）以上、**かつ**
/// `LogoIntervals::text` が空でない場合にだけ logoframe テキストを
/// `logoframe_path` に書いてそのパスを返す。
///
/// `logo_frames`（`Judgement::HasLogo` の数え上げ）と `text`（`build_text` の
/// 出力）は別経路で計算される。`build_text` は精緻化の結果 `s_end >= e_end`
/// になった区間を出力しないため、**検出割合が閾値以上でも `text` が空文字列に
/// なりうる**（`logo::interval` のモジュール doc comment「呼び出し側への委譲」
/// 節）。ここで `text` が空でないことも確認しないと、空の logoframe ファイルを
/// 書いて `-inlogo` として渡してしまう。join_logo_scp は `-inlogo` を渡されて
/// ロゴ情報が無ければ警告を出して全フレームをロゴ表示中として扱うため
/// （issue #97「罠」）、これは避ける。
///
/// いずれかの条件を満たさない場合は書かずに `None` を返し、理由を stderr に
/// 出す（誤ったロゴ情報で判定を崩すより現状維持に倒す。issue #97
/// 「フォールバック」）。このとき、以前の実行でキャッシュに残っている
/// `logoframe_path` があれば削除する（`-inlogo` には渡らないため実害は無いが、
/// キャッシュを覗いたときに紛らわしいため）。
fn detect_logo(
    ffmpeg_path: &Path,
    lgd_path: &Path,
    work_mp4: &Path,
    cwd: &Path,
    dtvi: &Dtvi,
    logoframe_path: &Path,
) -> Result<Option<PathBuf>> {
    let logo_data = lgd::read(lgd_path)?;
    let mask = score::LogoMask::new(&logo_data)
        .map_err(|err| anyhow!(".lgd からロゴマスクを構築できませんでした: {err}"))?;

    let rect = logo_rect_from_lgd(&logo_data)?;
    let video_size = dtvi_video_size(dtvi)?;
    let expected_frame_count = dtvi_frame_count(dtvi)?;

    let mut scores: Vec<(f32, f32)> = Vec::new();
    frames::stream_luma_frames(
        ffmpeg_path,
        work_mp4,
        cwd,
        rect,
        video_size,
        expected_frame_count,
        |frame| {
            scores.push(mask.evaluate(frame));
            Ok(())
        },
    )?;

    let fps = dtvi_fps(dtvi);
    let result = logo_interval::write_result(&scores, fps);

    let fraction = if result.total_frames == 0 {
        0.0
    } else {
        result.logo_frames as f64 / result.total_frames as f64
    };
    let duration_seconds = if fps > 0.0 {
        result.total_frames as f64 / fps
    } else {
        f64::INFINITY
    };
    let threshold = logo_detection_threshold(duration_seconds);

    match inlogo_decision(fraction, threshold, &result.text) {
        InlogoDecision::Use => {
            fs::write(logoframe_path, &result.text)
                .path_ctx("logoframe ファイルの書き出し", logoframe_path)?;
            eprintln!(
                "[analyze] ロゴ検出: {}/{}フレーム（割合 {:.3}、閾値 {:.3}）。\
                 logoframe を書き出しました: {}",
                result.logo_frames,
                result.total_frames,
                fraction,
                threshold,
                logoframe_path.display()
            );
            Ok(Some(logoframe_path.to_path_buf()))
        }
        InlogoDecision::FallbackEmptyText => {
            // logo_frames の割合は閾値以上だが、build_text の精緻化で全区間が
            // 捨てられ text が空になったケース（上の doc comment参照）。
            // fraction 未満の通常フォールバックとは原因が違うので、メッセージを
            // 分けて残す（実際に起きたときに原因の切り分けができるように）。
            eprintln!(
                "[analyze] ロゴ検出割合は閾値以上ですが、区間の精緻化後に text が空に\
                 なったため -inlogo を渡しません（割合 {fraction:.3} >= 閾値 {threshold:.3}、\
                 検出 {}/{}フレーム、logoframe の区間数 0）。空の logoframe を\
                 join_logo_scp に渡すと警告の上で全フレームをロゴ表示中として\
                 扱われてしまうため、渡しません。",
                result.logo_frames, result.total_frames
            );
            clear_stale_logoframe(logoframe_path);
            Ok(None)
        }
        InlogoDecision::FallbackBelowThreshold => {
            eprintln!(
                "[analyze] ロゴ検出割合が閾値未満のため -inlogo を渡しません（割合 {fraction:.3} \
                 < 閾値 {threshold:.3}、検出 {}/{}フレーム）。誤ったロゴ情報で判定を崩すより\
                 現状維持（ロゴ無し）に倒します。",
                result.logo_frames, result.total_frames
            );
            clear_stale_logoframe(logoframe_path);
            Ok(None)
        }
    }
}

/// [`inlogo_decision`] の結果。`-inlogo` を渡すかどうかと、渡さない場合の
/// 原因を区別する（原因ごとに stderr のメッセージを分けるため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum InlogoDecision {
    /// `-inlogo` を渡す（`fraction >= threshold` かつ `text` が空でない）。
    Use,
    /// `fraction >= threshold` だが `text` が空だったため渡さない
    /// （`build_text` が `s_end >= e_end` の区間を出力しないことによる。
    /// `logo_interval` のモジュール doc comment参照）。
    FallbackEmptyText,
    /// `fraction < threshold` のため渡さない（通常のフォールバック）。
    FallbackBelowThreshold,
}

/// `-inlogo` を渡すかどうかを決める（issue #97「フォールバック」、レビュー
/// 指摘: `fraction >= threshold` だけでは `text` が空のケースを見落とす）。
///
/// プロセス（ffmpeg 等）を起動せずに検証できるよう、`detect_logo` から分離した
/// 純粋関数にしている。
fn inlogo_decision(fraction: f64, threshold: f64, text: &str) -> InlogoDecision {
    if fraction < threshold {
        InlogoDecision::FallbackBelowThreshold
    } else if text.trim().is_empty() {
        InlogoDecision::FallbackEmptyText
    } else {
        InlogoDecision::Use
    }
}

/// フォールバック（`-inlogo` を渡さない）ときに、以前の実行でキャッシュに
/// 残っている `logoframe_path` を削除する。`-inlogo` には渡らないため実害は
/// 無いが、キャッシュを覗いたときに古い検出結果が残っていると紛らわしい
/// （レビュー指摘、issue #97）。削除に失敗しても `detect_logo` 自体は失敗
/// させない（`clear_stale_cached_segment_map`、`src/commands.rs` と同じ扱い）。
fn clear_stale_logoframe(logoframe_path: &Path) {
    match fs::remove_file(logoframe_path) {
        Ok(()) => {
            eprintln!(
                "[analyze] 古い logoframe を削除しました: {}",
                logoframe_path.display()
            );
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "[analyze] 古い logoframe の削除に失敗しました（警告のみ、処理は続行します）: \
                 {} ({err})",
                logoframe_path.display()
            );
        }
    }
}

/// [`LogoData`] の `imgx`/`imgy`/`w`/`h` から [`LogoRect`] を作る。いずれも
/// 負の値であればエラーにする（`.lgd` の座標は本来非負のはずで、負値を
/// そのまま `u32` へキャストすると巨大な値に化けて後段の範囲外検査を無意味に
/// する）。
fn logo_rect_from_lgd(logo: &LogoData) -> Result<LogoRect> {
    let to_u32 = |label: &str, v: i32| -> Result<u32> {
        u32::try_from(v).map_err(|_| anyhow!(".lgd の {label} が負の値です: {v}"))
    };
    Ok(LogoRect {
        x: to_u32("imgx", logo.imgx)?,
        y: to_u32("imgy", logo.imgy)?,
        w: to_u32("w", logo.w)?,
        h: to_u32("h", logo.h)?,
    })
}

/// `.dtvi` ヘッダの `width`/`height` から [`VideoSize`] を求める。
fn dtvi_video_size(dtvi: &Dtvi) -> Result<VideoSize> {
    let parse = |key: &str| -> Result<u32> {
        let value = dtvi
            .header_value(key)
            .ok_or_else(|| anyhow!(".dtvi のヘッダに {key} がありません"))?;
        value
            .trim()
            .parse::<u32>()
            .map_err(|_| anyhow!(".dtvi のヘッダの {key}（{value:?}）を数値として解釈できません"))
    };
    Ok(VideoSize {
        width: parse("width")?,
        height: parse("height")?,
    })
}

/// `.dtvi` ヘッダの `frame_count` を読む（`dtvi.rs` の doc comment参照）。
/// `join_logo_scp` を起動する前に、ロゴ検出で読み取ったフレーム数との一致を
/// 検査するために使う（`frames::stream_luma_frames` の `expected_frame_count`）。
fn dtvi_frame_count(dtvi: &Dtvi) -> Result<u64> {
    let value = dtvi
        .header_value("frame_count")
        .ok_or_else(|| anyhow!(".dtvi のヘッダに frame_count がありません"))?;
    value
        .trim()
        .parse::<u64>()
        .map_err(|_| anyhow!(".dtvi のヘッダの frame_count（{value:?}）を数値として解釈できません"))
}

/// `.dtvi` ヘッダの `frame_rate_num`/`frame_rate_den` から fps を求める。
///
/// `commands::fps_from_dtvi` と同じ既定値（キーが無い、または数値として
/// パースできない場合は対象素材の実測値 30000/1001）を使うが、`analyze.rs` は
/// `commands.rs`（`analyze` を呼ぶ側）に依存しない方針のため、ここで独立に
/// 計算する（重複はこの1関数分だけで、値の食い違いは両者とも同じ既定値・同じ
/// ヘッダキーを見ているため起きない）。
fn dtvi_fps(dtvi: &Dtvi) -> f64 {
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

/// ロゴ検出割合のフォールバック閾値を決める（issue #97「フォールバック」:
/// 通常 [`LOGO_DETECTION_THRESHOLD`]、映像長が
/// [`LOGO_DETECTION_SHORT_VIDEO_SECONDS`] 以下なら
/// [`LOGO_DETECTION_THRESHOLD_SHORT`]。Amatsukaze `CMAnalyze.hpp:301` と同じ規則）。
fn logo_detection_threshold(duration_seconds: f64) -> f64 {
    if duration_seconds <= LOGO_DETECTION_SHORT_VIDEO_SECONDS {
        LOGO_DETECTION_THRESHOLD_SHORT
    } else {
        LOGO_DETECTION_THRESHOLD
    }
}

/// 2 つのパスが同じファイルを指すか（親ディレクトリを canonicalize して比較）。
///
/// 出力先がまだ存在しない場合もあるので、ファイル自体ではなく親 + ファイル名で
/// 判定する。親が存在しないときは文字列比較に落とす。
fn same_path(a: &Path, b: &Path) -> Result<bool> {
    if a == b {
        return Ok(true);
    }
    let resolve = |p: &Path| -> PathBuf {
        match (p.parent(), p.file_name()) {
            (Some(parent), Some(name)) if parent.as_os_str().is_empty() => {
                PathBuf::from(".").join(name)
            }
            (Some(parent), Some(name)) => match fs::canonicalize(parent) {
                Ok(abs) => abs.join(name),
                Err(_) => p.to_path_buf(),
            },
            _ => p.to_path_buf(),
        }
    };
    Ok(resolve(a) == resolve(b))
}

/// join_logo_scp に渡す `-set KEY VALUE ...` の引数列を組み立てる。
///
/// `defaults` の順序を保ったまま、`overrides` に同じキーがあれば値を置き換え、
/// `defaults` に無いキーは `overrides` に現れた順で末尾に追加する。
/// プロセスを起動せずに検証できるよう、`external::run` の呼び出しから分離した
/// 純粋関数にしている。
fn build_jls_set_args(defaults: &[(&str, &str)], overrides: &[(String, String)]) -> Vec<String> {
    let mut entries: Vec<(String, String)> = defaults
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();

    for (key, value) in overrides {
        if let Some(existing) = entries.iter_mut().find(|(k, _)| k == key) {
            existing.1 = value.clone();
        } else {
            entries.push((key.clone(), value.clone()));
        }
    }

    let mut args = Vec::with_capacity(entries.len() * 3);
    for (key, value) in entries {
        args.push("-set".to_string());
        args.push(key);
        args.push(value);
    }
    args
}

/// `"KEY=VALUE"` 形式の文字列を `(String, String)` にパースする。
///
/// CLI 側（`cli.rs`）で `--jls-set` の値パースに使う想定。`VALUE` 自体に `=`
/// が含まれていてもよいよう、最初の `=` でのみ分割する。
pub fn parse_jls_set_arg(raw: &str) -> Result<(String, String)> {
    let (key, value) = raw
        .split_once('=')
        .ok_or_else(|| anyhow!("--jls-set は KEY=VALUE 形式で指定してください: {raw:?}"))?;
    if key.is_empty() {
        return Err(anyhow!("--jls-set のキーが空です: {raw:?}"));
    }
    Ok((key.to_string(), value.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    // `PATH` を書き換えるロックは `tools::tests` と共有する（`resolve_tool` が
    // `PATH` しか見ないため、ツール解決の成功/失敗を作り分けるには `PATH` の
    // 書き換えが唯一の手段になった。`crate::tools::test_support` の doc
    // comment参照）。キャッシュの根は `--cache-dir`（引数）に一本化したため、
    // このモジュールはキャッシュ関連の環境変数を一切読み書きしない
    // （`workdir::test_support` は削除済み）。
    use crate::tools::test_support::{
        EnvVarGuard as ToolPathEnvGuard, ENV_LOCK as TOOL_PATH_ENV_LOCK,
    };
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    // --- build_jls_set_args: プロセス起動なしで検証できる純粋関数のテスト ---

    fn to_strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn build_jls_set_args_uses_defaults_when_no_overrides() {
        let args = build_jls_set_args(DEFAULT_JLS_SET, &[]);
        assert_eq!(
            args,
            to_strings(&["-set", "autocm_sub", "11", "-set", "param_cuttr", "1"])
        );
    }

    #[test]
    fn same_path_detects_identical_and_relative_forms() {
        let dir = unique_scratch_dir("same-path");
        let a = dir.join("trim.avs");
        fs::write(&a, "Trim(0,1)\n").unwrap();
        assert!(same_path(&a, &a).unwrap());
        // 親を canonicalize して比較するので、存在する親配下なら一致する
        let b = PathBuf::from(&dir).join("trim.avs");
        assert!(same_path(&a, &b).unwrap());
        let other = dir.join("other.avs");
        assert!(!same_path(&a, &other).unwrap());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn build_jls_set_args_overrides_existing_key_in_place() {
        // 完了条件: `--jls-set autocm_sub=10` で既定値が置き換わることを確認する。
        let overrides = vec![("autocm_sub".to_string(), "10".to_string())];
        let args = build_jls_set_args(DEFAULT_JLS_SET, &overrides);
        assert_eq!(
            args,
            to_strings(&["-set", "autocm_sub", "10", "-set", "param_cuttr", "1"])
        );
    }

    #[test]
    fn build_jls_set_args_appends_unknown_keys() {
        let overrides = vec![("extra_key".to_string(), "5".to_string())];
        let args = build_jls_set_args(DEFAULT_JLS_SET, &overrides);
        assert_eq!(
            args,
            to_strings(&[
                "-set",
                "autocm_sub",
                "11",
                "-set",
                "param_cuttr",
                "1",
                "-set",
                "extra_key",
                "5",
            ])
        );
    }

    #[test]
    fn build_jls_set_args_overrides_both_defaults() {
        let overrides = vec![
            ("param_cuttr".to_string(), "0".to_string()),
            ("autocm_sub".to_string(), "10".to_string()),
        ];
        let args = build_jls_set_args(DEFAULT_JLS_SET, &overrides);
        assert_eq!(
            args,
            to_strings(&["-set", "autocm_sub", "10", "-set", "param_cuttr", "0"])
        );
    }

    // --- parse_jls_set_arg ---

    #[test]
    fn parse_jls_set_arg_splits_key_value() {
        assert_eq!(
            parse_jls_set_arg("autocm_sub=10").unwrap(),
            ("autocm_sub".to_string(), "10".to_string())
        );
    }

    #[test]
    fn parse_jls_set_arg_rejects_missing_equals() {
        let err = parse_jls_set_arg("autocm_sub").unwrap_err();
        assert!(err.to_string().contains("KEY=VALUE"));
    }

    #[test]
    fn parse_jls_set_arg_rejects_empty_key() {
        assert!(parse_jls_set_arg("=10").is_err());
    }

    #[test]
    fn parse_jls_set_arg_allows_value_containing_equals() {
        let (key, value) = parse_jls_set_arg("key=a=b").unwrap();
        assert_eq!(key, "key");
        assert_eq!(value, "a=b");
    }

    // --- --logo 用の純粋関数: プロセス起動なしで検証できる ---

    /// テスト用に、指定したヘッダだけを持つ最小の `Dtvi` を組み立てる。
    fn dtvi_with_header(pairs: &[(&str, &str)]) -> Dtvi {
        Dtvi {
            format_version: 1,
            header: pairs
                .iter()
                .map(|&(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            frames: Vec::new(),
        }
    }

    #[test]
    fn logo_rect_from_lgd_reads_imgx_imgy_w_h() {
        let logo = LogoData {
            w: 8,
            h: 8,
            log_uv_x: 1,
            log_uv_y: 1,
            imgw: 640,
            imgh: 360,
            imgx: 620,
            imgy: 4,
            name: String::new(),
            service_id: 0,
            a_y: vec![],
            b_y: vec![],
            a_u: vec![],
            b_u: vec![],
            a_v: vec![],
            b_v: vec![],
        };
        let rect = logo_rect_from_lgd(&logo).expect("非負なので成功するはず");
        assert_eq!(
            rect,
            LogoRect {
                x: 620,
                y: 4,
                w: 8,
                h: 8
            }
        );
    }

    #[test]
    fn logo_rect_from_lgd_rejects_negative_coordinate() {
        let mut logo = LogoData {
            w: 8,
            h: 8,
            log_uv_x: 1,
            log_uv_y: 1,
            imgw: 640,
            imgh: 360,
            imgx: -1,
            imgy: 4,
            name: String::new(),
            service_id: 0,
            a_y: vec![],
            b_y: vec![],
            a_u: vec![],
            b_u: vec![],
            a_v: vec![],
            b_v: vec![],
        };
        let err = logo_rect_from_lgd(&logo).expect_err("負値はエラーになるはず");
        assert!(err.to_string().contains("imgx"));
        // w が負でも同様にエラーになる（別フィールドでも検査経路が働くことの確認）。
        logo.imgx = 0;
        logo.w = -1;
        let err = logo_rect_from_lgd(&logo).expect_err("負値はエラーになるはず");
        assert!(err.to_string().contains('w'));
    }

    #[test]
    fn dtvi_video_size_reads_width_and_height() {
        let dtvi = dtvi_with_header(&[("width", "640"), ("height", "360")]);
        let size = dtvi_video_size(&dtvi).expect("width/height があるので成功するはず");
        assert_eq!(
            size,
            VideoSize {
                width: 640,
                height: 360
            }
        );
    }

    #[test]
    fn dtvi_video_size_missing_key_is_an_error() {
        let dtvi = dtvi_with_header(&[("width", "640")]);
        let err = dtvi_video_size(&dtvi).expect_err("height が無いのでエラーになるはず");
        assert!(err.to_string().contains("height"));
    }

    #[test]
    fn dtvi_frame_count_reads_header() {
        let dtvi = dtvi_with_header(&[("frame_count", "599")]);
        assert_eq!(dtvi_frame_count(&dtvi).expect("成功するはず"), 599);
    }

    #[test]
    fn dtvi_frame_count_missing_key_is_an_error() {
        let dtvi = dtvi_with_header(&[]);
        let err = dtvi_frame_count(&dtvi).expect_err("frame_count が無いのでエラーになるはず");
        assert!(err.to_string().contains("frame_count"));
    }

    #[test]
    fn dtvi_fps_reads_frame_rate_header() {
        let dtvi = dtvi_with_header(&[("frame_rate_num", "24000"), ("frame_rate_den", "1001")]);
        let fps = dtvi_fps(&dtvi);
        assert!((fps - 24000.0 / 1001.0).abs() < 1e-9);
    }

    #[test]
    fn dtvi_fps_defaults_to_measured_value_when_header_missing() {
        let dtvi = dtvi_with_header(&[]);
        let fps = dtvi_fps(&dtvi);
        assert!((fps - 30000.0 / 1001.0).abs() < 1e-9);
    }

    #[test]
    fn logo_detection_threshold_is_lenient_for_short_videos() {
        assert_eq!(
            logo_detection_threshold(60.0),
            LOGO_DETECTION_THRESHOLD_SHORT
        );
        assert_eq!(
            logo_detection_threshold(LOGO_DETECTION_SHORT_VIDEO_SECONDS),
            LOGO_DETECTION_THRESHOLD_SHORT
        );
    }

    #[test]
    fn logo_detection_threshold_is_default_for_long_videos() {
        assert_eq!(
            logo_detection_threshold(LOGO_DETECTION_SHORT_VIDEO_SECONDS + 1.0),
            LOGO_DETECTION_THRESHOLD
        );
        assert_eq!(logo_detection_threshold(3600.0), LOGO_DETECTION_THRESHOLD);
    }

    // --- inlogo_decision: レビュー指摘（fraction >= threshold でも text が
    // 空になりうる）の防御 ---

    #[test]
    fn inlogo_decision_uses_inlogo_when_fraction_and_text_are_both_ok() {
        assert_eq!(
            inlogo_decision(0.5, LOGO_DETECTION_THRESHOLD, "0 S 0 ALL 0 0\n"),
            InlogoDecision::Use
        );
    }

    #[test]
    fn inlogo_decision_falls_back_when_fraction_below_threshold() {
        assert_eq!(
            inlogo_decision(0.05, LOGO_DETECTION_THRESHOLD, "0 S 0 ALL 0 0\n"),
            InlogoDecision::FallbackBelowThreshold
        );
    }

    #[test]
    fn inlogo_decision_falls_back_when_text_is_empty_even_if_fraction_is_high() {
        // fraction が閾値以上でも text が空（＝build_text が区間を1つも
        // 出力しなかった）なら Use にしてはいけない。
        assert_eq!(
            inlogo_decision(0.96, LOGO_DETECTION_THRESHOLD, ""),
            InlogoDecision::FallbackEmptyText
        );
        // 空白だけの文字列も「空」とみなす。
        assert_eq!(
            inlogo_decision(0.96, LOGO_DETECTION_THRESHOLD, "  \n"),
            InlogoDecision::FallbackEmptyText
        );
    }

    #[test]
    fn inlogo_decision_below_threshold_takes_priority_over_empty_text_label() {
        // fraction が閾値未満で text も空の場合、原因は「閾値未満」であって
        // 「text が空」ではない（メッセージの正しさの確認）。
        assert_eq!(
            inlogo_decision(0.0, LOGO_DETECTION_THRESHOLD, ""),
            InlogoDecision::FallbackBelowThreshold
        );
    }

    /// レビューで再現された不具合の再現テスト: `logo_frames` の数え上げ
    /// （割合 96.2%、閾値 0.1 を大きく上回る）に対して `build_text` の出力
    /// （`text`）が空になるスコア分布を作り、それでも `inlogo_decision` が
    /// `Use` を返さないことを確認する。
    ///
    /// スコアは `scores[i] = (4.4, 0.0)`（21 の倍数の位置）/ `(0.0, 0.0)`
    /// （それ以外）を 600 フレーム分、fps は本ツールの既定値 30000/1001 で
    /// 作る（レビューコメントの再現条件そのもの）。`(4.4, 0.0)` は
    /// `raw = corr0.max(0.0) + corr1.min(0.0) = 4.4` という強い「ロゴあり」
    /// スコアだが、21 フレームに1回しか出ないスパイクのため 1 秒移動平均
    /// （`AVG_DUR_SEC = 1.0`、fps ≈ 30 なので窓は約31フレーム）に均されると
    /// `THRESH = 0.2` を超えず、MinMax 判定（前後 0.5 秒の最大値の小さい方）
    /// だけがロゴありと判定してしまう区間ができる。この食い違いが
    /// `fill_unknown_runs` の穴埋め結果と `build_text` の精緻化の間でずれ、
    /// `s_end >= e_end` になった区間が出力から丸ごと落ちる。
    #[test]
    fn detect_logo_fallback_reproduces_review_case_high_fraction_empty_text() {
        const N: usize = 600;
        let scores: Vec<(f32, f32)> = (0..N)
            .map(|i| if i % 21 == 0 { (4.4, 0.0) } else { (0.0, 0.0) })
            .collect();
        let fps = 30000.0 / 1001.0;

        let result = logo_interval::write_result(&scores, fps);

        // レビューの再現条件どおり: logo_frames の割合は閾値を大きく上回るが
        // text は空になる。
        let fraction = result.logo_frames as f64 / result.total_frames as f64;
        assert!(
            fraction >= LOGO_DETECTION_THRESHOLD,
            "再現条件が崩れている（割合 {fraction} が閾値 {LOGO_DETECTION_THRESHOLD} 未満）"
        );
        assert!(
            result.text.trim().is_empty(),
            "再現条件が崩れている（text が空でない: {:?}）",
            result.text
        );

        // この入力に対して inlogo_decision が Use を返してはいけない
        // （空の logoframe を書いて -inlogo として渡してしまうバグの防御）。
        assert_eq!(
            inlogo_decision(fraction, LOGO_DETECTION_THRESHOLD, &result.text),
            InlogoDecision::FallbackEmptyText,
            "fraction が閾値以上でも text が空なら Use にしてはいけない"
        );
    }

    // --- clear_stale_logoframe: フォールバック時の残骸削除 ---

    #[test]
    fn clear_stale_logoframe_removes_existing_file() {
        let dir = unique_scratch_dir("clear-stale-logoframe");
        let path = dir.join("logoframe.txt");
        fs::write(&path, "0 S 0 ALL 0 0\n0 E 0 ALL 0 0\n").unwrap();
        clear_stale_logoframe(&path);
        assert!(!path.exists(), "既存の logoframe.txt が削除されているはず");
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn clear_stale_logoframe_is_a_noop_when_file_does_not_exist() {
        // ファイルが元々無い場合はエラーにならない（panic しないことの確認）。
        let dir = unique_scratch_dir("clear-stale-logoframe-missing");
        let path = dir.join("logoframe.txt");
        clear_stale_logoframe(&path);
        assert!(!path.exists());
        fs::remove_dir_all(&dir).ok();
    }

    // --- run(): resolve_tool のエラーがそのまま伝播することの確認 ---

    fn unique_scratch_dir(label: &str) -> PathBuf {
        let base = std::env::temp_dir();
        let pid = process::id();
        for attempt in 0..100 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let candidate = base.join(format!(
                "tachikaze-analyze-test-{label}-{pid}-{nanos}-{attempt}"
            ));
            if fs::create_dir_all(&candidate).is_ok() {
                return candidate;
            }
        }
        panic!("scratch dir の作成に失敗しました");
    }

    #[cfg(unix)]
    fn write_executable_script(path: &Path, script: &str) {
        use std::os::unix::fs::PermissionsExt;
        fs::write(path, script).expect("write script");
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).expect("chmod script");
    }

    #[test]
    fn run_propagates_tool_resolution_failure_with_searched_locations() {
        // completion condition: ツールが見つからないとき、どのツールをどこで
        // 探したかが分かるエラーになることを確認する。`resolve_tool` が既に
        // その情報を組み立てているので、ここでは `run` から素通しで伝わる
        // ことだけを確認する。
        //
        // `resolve_tool` は `PATH` だけを探す（`crate::tools` の doc comment）。
        // したがって `docs/toolchain-macos.md` の手順どおりに3ツールを `PATH`
        // へ入れた環境では解決が**成功**してしまい、このテストは「見つからない」
        // 前提を失う。`PATH` をツールの無いディレクトリだけに絞って隔離する
        // （`tools::tests::resolve_tool_reports_all_searched_path_dirs_when_missing`
        // と同じ手法。環境変数はプロセス全体で共有されるので `TOOL_PATH_ENV_LOCK`
        // が要る）。
        let _env_guard = TOOL_PATH_ENV_LOCK.lock().unwrap();
        let path_dir_without_tools = unique_scratch_dir("missing-tools");
        let _path_env = ToolPathEnvGuard::set("PATH", &path_dir_without_tools);

        let output_dir = unique_scratch_dir("missing-tools-output");
        let output = output_dir.join("trim.avs");

        let config = AnalyzeConfig {
            // ツール解決が入力の存在確認より前に走るため、実在しないパスでよい。
            input: PathBuf::from("/nonexistent/input-for-analyze-test.mp4"),
            output: Some(output),
            cache_dir: None,
            jls_set: vec![],
            jl_file: None,
            logo: None,
        };

        let err = run(&config).expect_err("空振りする PATH では解決に失敗するはず");
        let message = err.to_string();
        assert!(
            message.contains(DTVINDEX),
            "エラーメッセージにツール名が含まれていない: {message}"
        );
        assert!(
            message.contains(&path_dir_without_tools.join(DTVINDEX).display().to_string()),
            "エラーメッセージに探索したパスが含まれていない: {message}"
        );

        fs::remove_dir_all(&path_dir_without_tools).ok();
        fs::remove_dir_all(&output_dir).ok();
    }

    #[test]
    fn run_stops_pipeline_and_surfaces_stderr_on_first_failure() {
        // completion condition: どれかが失敗したら以降を実行せず、そのツールの
        // stderr を出す。chapter_exe を失敗させ、(1) エラーメッセージに
        // chapter_exe の stderr が含まれること、(2) 後段の join_logo_scp が
        // 一度も起動されないこと（マーカーファイルが作られない）を確認する。
        //
        // キャッシュの根は `--cache-dir` 相当の `cache_dir` フィールドへ直接
        // 渡すため（環境変数は経由しない）、キャッシュ側の隔離用ロックは不要。
        // 偽ツールを `PATH` 経由で注入するため `TOOL_PATH_ENV_LOCK` だけ要る。
        let cache_root = unique_scratch_dir("stop-on-failure-cache");

        let _path_env_guard = TOOL_PATH_ENV_LOCK.lock().unwrap();
        let fake_tools_dir = unique_scratch_dir("stop-on-failure-tools");
        let input_dir = unique_scratch_dir("stop-on-failure-input");
        let output_dir = unique_scratch_dir("stop-on-failure-output");

        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write dummy input");

        // dtvindex: `-o` の次の引数にダミーの中身を書いて成功する。
        write_executable_script(
            &fake_tools_dir.join(DTVINDEX),
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'dummy' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
        );

        // chapter_exe: 常に失敗し、判定用の stderr を出す。
        write_executable_script(
            &fake_tools_dir.join(CHAPTER_EXE),
            "#!/bin/sh\necho 'FAKE CHAPTER_EXE FAILURE' >&2\nexit 5\n",
        );

        // join_logo_scp: 起動されたらマーカーファイルを作る（呼ばれてはいけない）。
        let marker_path = fake_tools_dir.join("join_logo_scp_was_called.marker");
        write_executable_script(
            &fake_tools_dir.join(JOIN_LOGO_SCP),
            &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker_path.display()),
        );

        // 偽ツール一式だけを `PATH` に置く（`resolve_tool` は `PATH` しか
        // 見ないため、これが唯一の解決先の差し替え手段）。
        let _path_env = ToolPathEnvGuard::set("PATH", &fake_tools_dir);

        let config = AnalyzeConfig {
            input: input_path,
            output: Some(output_dir.join("trim.avs")),
            cache_dir: Some(cache_root.clone()),
            jls_set: vec![],
            jl_file: None,
            logo: None,
        };

        let err = run(&config).expect_err("chapter_exe の失敗で run 全体が失敗するはず");
        let message = err.to_string();
        assert!(
            message.contains("FAKE CHAPTER_EXE FAILURE"),
            "エラーメッセージに chapter_exe の stderr が含まれていない: {message}"
        );
        assert!(
            message.contains("終了コード: 5"),
            "エラーメッセージに終了コードが含まれていない: {message}"
        );
        assert!(
            !marker_path.exists(),
            "chapter_exe が失敗した後に join_logo_scp が起動されてはいけない"
        );

        fs::remove_dir_all(&fake_tools_dir).ok();
        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&output_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    #[test]
    fn run_writes_trim_avs_to_cache_and_optionally_to_explicit_path() {
        // 完了条件: `-o` 省略時（`output: None`）はキャッシュにだけ書き、
        // `AnalyzeOutput::cache_trim_path` でその場所が分かる。`output: Some(path)`
        // 指定時は従来どおりそのパスにも書く。どちらでも `raw_trim` は
        // join_logo_scp が書いた内容と一致する（CLI の `-o -` がこれをそのまま
        // 標準出力に書くため、パース→再構成に頼らず生の内容を保持する）。
        let cache_root = unique_scratch_dir("cache-only-cache");

        let _path_env_guard = TOOL_PATH_ENV_LOCK.lock().unwrap();
        let fake_tools_prefix = unique_scratch_dir("cache-only-tools");
        let bin_dir = fake_tools_prefix.join("bin");
        fs::create_dir_all(&bin_dir).expect("bin_dir を作れること");
        let input_dir = unique_scratch_dir("cache-only-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write dummy input");

        let dtvi_src = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/sample.dtvi");
        write_executable_script(
            &bin_dir.join(DTVINDEX),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    /bin/cp \"{}\" \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
                dtvi_src.display()
            ),
        );
        write_executable_script(
            &bin_dir.join(CHAPTER_EXE),
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'scp placeholder\\n' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
        );
        const TRIM_CONTENT: &str = "Trim(0,19)";
        let detail_jls = "開始 終了 秒数 誤差 ロゴ秒 ラベル\n0 39 1 0 0 :L\n";
        write_executable_script(
            &bin_dir.join(JOIN_LOGO_SCP),
            &format!(
                "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  case \"$prev\" in\n    -o) printf '{}' > \"$a\" ;;\n    -oscp) printf '{}' > \"$a\" ;;\n  esac\n  prev=\"$a\"\ndone\nexit 0\n",
                TRIM_CONTENT, detail_jls
            ),
        );
        // `default_jl_command_file` は `join_logo_scp` の実体パスから
        // `<親の親>/share/join_logo_scp/JL/JL_標準.txt` を導出する（`src/tools.rs`）。
        let jl_dir = fake_tools_prefix
            .join("share")
            .join("join_logo_scp")
            .join("JL");
        fs::create_dir_all(&jl_dir).expect("jl_dir を作れること");
        fs::write(jl_dir.join("JL_標準.txt"), "placeholder\n").expect("JLファイルを書けること");

        let _path_env = ToolPathEnvGuard::set("PATH", &bin_dir);

        // output: None -> キャッシュにだけ書く
        let config_none = AnalyzeConfig {
            input: input_path.clone(),
            output: None,
            cache_dir: Some(cache_root.clone()),
            jls_set: vec![],
            jl_file: None,
            logo: None,
        };
        let out_none = run(&config_none).expect("output なしでも成功するはず");
        assert_eq!(out_none.raw_trim, TRIM_CONTENT);
        assert!(
            out_none.cache_trim_path.is_file(),
            "キャッシュに trim.avs が残っているはず"
        );
        assert_eq!(
            fs::read_to_string(&out_none.cache_trim_path).unwrap(),
            TRIM_CONTENT
        );

        // output: Some(path) -> キャッシュに加えて明示パスにも書く
        let explicit_dir = unique_scratch_dir("cache-only-explicit");
        let explicit_path = explicit_dir.join("trim.avs");
        let config_some = AnalyzeConfig {
            input: input_path.clone(),
            output: Some(explicit_path.clone()),
            cache_dir: Some(cache_root.clone()),
            jls_set: vec![],
            jl_file: None,
            logo: None,
        };
        let out_some = run(&config_some).expect("明示パスでも成功するはず");
        assert_eq!(out_some.raw_trim, TRIM_CONTENT);
        assert_eq!(
            fs::read_to_string(&explicit_path).unwrap(),
            TRIM_CONTENT,
            "明示パスにも書かれているはず"
        );

        fs::remove_dir_all(&fake_tools_prefix).ok();
        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&explicit_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    // --- 統合テスト（実バイナリが必要） ---

    /// 実ファイル + 実バイナリ (dtvindex / chapter_exe / join_logo_scp) を使い、
    /// analyze パイプライン全体が `Trim(...)` を含む `TrimList` を返すことを
    /// 確認する統合テスト。
    ///
    /// 3ツールの実バイナリはリポジトリに含まれず、用意にも時間がかかる
    /// （`docs/toolchain-macos.md` 参照）ため既定では無視する。実行する場合は
    /// 該当手順でビルドし、`PATH` から引けるようにした上で、`tests/fixtures/gen.sh`
    /// でフィクスチャを生成し `cargo test -- --ignored` で回すこと。
    #[test]
    #[ignore = "dtvindex/chapter_exe/join_logo_scp の実バイナリと実サンプルmp4が必要（docs/toolchain-macos.md）"]
    fn analyze_run_produces_trim_list_with_real_tools() {
        // キャッシュの根を明示して、利用者の実際のキャッシュ（`~/.cache/tachikaze`）
        // を汚さないようにする（`cache_dir: None` にすると既定値が使われてしまう）。
        let cache_root = unique_scratch_dir("integration-cache");

        let output_dir = unique_scratch_dir("integration-output");
        let config = AnalyzeConfig {
            // cwd 非依存にする（`external::tests` がプロセスの cwd を一時的に変えるため）。
            input: PathBuf::from(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/tests/fixtures/sample.mp4"
            )),
            output: Some(output_dir.join("trim.avs")),
            cache_dir: Some(cache_root.clone()),
            jls_set: vec![],
            jl_file: None,
            logo: None,
        };

        let output = run(&config).expect("analyze パイプラインが成功するはず");
        assert!(!output.trim.ranges().is_empty());

        fs::remove_dir_all(&output_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }
}
