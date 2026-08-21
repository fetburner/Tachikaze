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
use crate::logo::dict;
use crate::logo::estimate::{self, SampleLabel};
use crate::logo::frames::{self, LogoRect, VideoSize};
use crate::logo::lgd::{self, LogoData};
use crate::logo::scan;
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

/// 自動推定（`--logo`/`--no-logo` 省略時）で、採用列（AUC 順、
/// `estimate::estimate_candidates` の戻り値）の先頭から実際に学習・検出を試す
/// 候補数の上限（issue #135「やること 2-6」）。候補1件あたり [`scan::run`]
/// （実測30分1080pで21.5秒かかるフルデコード）と [`detect_logo`] の2パスが
/// かかるため、候補を無制限に試すと入力1本の処理時間が大きく伸びる。4局の
/// 実測では2番目までの候補で成功しているため、余裕を見て5件に切る（1件目で
/// 失敗する入力（テレビ朝日の4:3再放送、issue #135「罠」）があるため1件では
/// 足りない）。
const MAX_AUTO_TRAINING_CANDIDATES: usize = 5;

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
    /// `--logo`（ロゴ検出に使う `.lgd`）。`Some` のときの挙動は1バイトも
    /// 変わらない（issue #97「解くべき問題」、issue #135「解くべき問題」）。
    /// `None` かつ `no_logo` が `false` のとき（既定）は、自動推定
    /// （[`run_auto_logo_detection`]）が試みられる。`no_logo` と同時に
    /// `Some` にしてはいけない（[`validate_logo_flags`] で検査する）。
    pub logo: Option<PathBuf>,
    /// `--no-logo`。`true` なら自動推定を行わず、常にロゴ無し（`-inlogo` を
    /// 渡さない）で処理する（従来どおりの `--logo` 省略時の挙動、issue
    /// #135「解くべき問題」）。`logo` が `Some` のときに `true` にしてはいけない。
    pub no_logo: bool,
    /// `--logo-dir`（ロゴ辞書ディレクトリの上書き）。`None` なら
    /// `dict::resolve_dict_dir` の既定に従う。`logo`/`no_logo` 経路では
    /// 使わない（自動推定のときだけ参照する）。
    pub logo_dir: Option<PathBuf>,
    /// 自動推定で学習した `.lgd` の辞書ファイル名・`LogoData.name` を作るときに
    /// `input` の代わりに使う元入力の表示名（レビュー指摘、issue #135）。
    ///
    /// `auto` は `prepare` 後のパス（`<cache>/<hash>-<stem>/input_prepared.mp4`）
    /// を `input` としてここへ渡すため、`input` の stem は常に
    /// `"input_prepared"` になる。`dict::save`/`auto_logo_name` がそのまま
    /// `input` を使うと、辞書のファイル名・`LogoData.name` が局に関わらず
    /// 常に `input_prepared`（`-2`、`-3` ...）になり、辞書を見てもどの局の
    /// ロゴか分からず調査ができなくなる（親issue #130「局ごとのロゴ辞書」の
    /// ゴールに反する）。`auto` はユーザーが実際に指定した元の入力パスを
    /// 知っている（`auto::process_one` の引数）ため、それをここに渡す。
    /// `analyze` を直接叩いた場合は `input` が既にユーザー入力そのものなので
    /// `None` のままでよい（[`dict_naming_source`] が `input` にフォールバック
    /// する）。
    pub source_name_hint: Option<PathBuf>,
}

/// ロゴ辞書のファイル名・`LogoData.name` を作るときに使う「元入力」のパスを
/// 返す（[`AnalyzeConfig::source_name_hint`] の doc comment参照）。
/// `source_name_hint` があればそれを、無ければ `input` をそのまま使う。
fn dict_naming_source(config: &AnalyzeConfig) -> &Path {
    config.source_name_hint.as_deref().unwrap_or(&config.input)
}

/// [`AnalyzeConfig::logo`] と [`AnalyzeConfig::no_logo`] が両立しないことを
/// 検査する。
///
/// `--logo` は特定の `.lgd` を明示するオプション、`--no-logo` は自動推定を
/// 行わせないオプションで、両方指定する意味が無い（`--logo` があれば
/// そもそも自動推定は行われない）。CLI（`src/cli.rs`）は `clap` の
/// `conflicts_with` でも同じ組み合わせを弾くが、`AnalyzeConfig`/`AutoConfig`
/// を直接組み立てて `analyze::run`/`auto::run` を呼ぶ経路（プログラム的な
/// 呼び出し、および両モジュールの単体テスト）も同じ規則で弾くため、実処理
/// （`WorkDir::new` や外部ツール解決）より前にここでも検査する。
pub fn validate_logo_flags(logo: Option<&Path>, no_logo: bool) -> Result<()> {
    anyhow::ensure!(
        !(logo.is_some() && no_logo),
        "--logo と --no-logo は同時に指定できません（--logo は特定の .lgd を使う指定、\
         --no-logo は自動推定を行わない指定で、両立しません）"
    );
    Ok(())
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
    validate_logo_flags(config.logo.as_deref(), config.no_logo)?;

    let dtvindex_path = tools::resolve_tool(DTVINDEX)?;
    let chapter_exe_path = tools::resolve_tool(CHAPTER_EXE)?;
    let join_logo_scp_path = tools::resolve_tool(JOIN_LOGO_SCP)?;
    // ffmpeg が要るのは「`--logo` で `.lgd` が明示されたとき」（既存経路）に加え、
    // 「`--no-logo` が指定されていない自動推定モード」（新規、issue #135）。
    // `--logo`/`--no-logo` は排他（`validate_logo_flags` で検査済み）なので、
    // `!config.no_logo` だけで両条件をまとめて表せる。
    let ffmpeg_path = if let Some(lgd_path) = &config.logo {
        fs::metadata(lgd_path).path_ctx("--logo で指定された .lgd の確認", lgd_path)?;
        Some(tools::resolve_tool(FFMPEG)?)
    } else if !config.no_logo {
        Some(tools::resolve_tool(FFMPEG)?)
    } else {
        None
    };
    // ロゴ辞書ディレクトリの解決も、自動推定モードのときだけツール解決と同じ
    // タイミング（`WorkDir::new` より前）で行う。ホームディレクトリが特定できない
    // 場合のエラー（`dict::resolve_dict_dir`）を、入力の存在確認や作業ディレクトリの
    // 作成より前に出すため（`run()` の他の早期検証と同じ方針）。
    let dict_dir = if config.logo.is_none() && !config.no_logo {
        Some(dict::resolve_dict_dir(config.logo_dir.as_deref())?)
    } else {
        None
    };

    let work = WorkDir::new(config.cache_dir.as_deref(), &config.input)?;
    let result = run_pipeline(
        config,
        &work,
        &dtvindex_path,
        &chapter_exe_path,
        &join_logo_scp_path,
        ffmpeg_path.as_deref(),
        dict_dir.as_deref(),
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
    dict_dir: Option<&Path>,
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

    // `--logo` が明示されている場合、または `--no-logo` の場合は従来の流れの
    // まま（join_logo_scp は1回だけ）にする（issue #135「やること 2」）。
    // それ以外（両方省略、既定）が自動推定モード（issue #135「解くべき問題」）。
    let auto_detect = config.logo.is_none() && !config.no_logo;

    let inlogo_path = if auto_detect {
        // 自動推定モード。`run()` が自動推定モードのとき必ず ffmpeg とロゴ辞書
        // ディレクトリを解決してから `run_pipeline` を呼ぶため、ここで `None`
        // になることは無い（`run_pipeline` は `run()` からしか呼ばれない
        // private 関数）。
        let ffmpeg_path = ffmpeg_path.ok_or_else(|| {
            anyhow!(
                "内部エラー: 自動推定モードで ffmpeg_path が解決されていません\
                 （run() の実装を確認してください）"
            )
        })?;
        let dict_dir = dict_dir.ok_or_else(|| {
            anyhow!(
                "内部エラー: 自動推定モードでロゴ辞書ディレクトリが解決されていません\
                 （run() の実装を確認してください）"
            )
        })?;

        // 手順2: join_logo_scp を `-inlogo` 無しで1回走らせ、「ロゴ無しの結果」
        // として保持する（issue #135「やること 2」）。この結果は、後段の
        // どの候補も検出に成功しなかった場合の最終結果としてそのまま使う
        // （2回目の join_logo_scp を走らせない。issue #135「罠」）。
        run_join_logo_scp(
            join_logo_scp_path,
            &scp_path,
            &jl_file,
            &trim_avs_path,
            &detail_jls_path,
            None,
            &config.jls_set,
            work.path(),
        )?;
        eprintln!(
            "[analyze] 自動推定: join_logo_scp を -inlogo 無しで実行しました\
             （ロゴ無しの結果を保持します）。"
        );

        let no_logo_trim_content = fs::read_to_string(&trim_avs_path).path_ctx(
            "join_logo_scp が生成した trim.avs の読み込み（ロゴ無しの結果、自動推定用）",
            &trim_avs_path,
        )?;
        let no_logo_trim = TrimList::parse(&no_logo_trim_content)
            .map_err(|err| anyhow!("生成された trim.avs のパースに失敗しました: {err}"))?;

        let dtvi_content_for_auto = fs::read_to_string(&dtvi_path).path_ctx(
            "dtvindex が生成した .dtvi の読み込み（自動推定用）",
            &dtvi_path,
        )?;
        let dtvi_for_auto = dtvi::parse(&dtvi_content_for_auto)
            .map_err(|err| anyhow!("生成された .dtvi のパースに失敗しました: {err}"))?;

        run_auto_logo_detection(
            ffmpeg_path,
            &work_mp4,
            work.path(),
            &dtvi_for_auto,
            &work.logoframe_path(),
            dict_dir,
            &no_logo_trim,
            dict_naming_source(config),
        )?
    } else {
        // `--logo` があるときだけ動く経路（E14-8、issue #97）。フレーム数の不一致
        // （`frames::stream_luma_frames` が内部で検査する。CLAUDE.md 罠3）は
        // ここで `?` によりエラーとして中断し、この時点ではまだ join_logo_scp を
        // 起動していない（issue #97「罠」: この検査は省略可能なオプションにせず、
        // 必ず join_logo_scp の起動前に済ませる）。
        match &config.logo {
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
        }
    };

    // join_logo_scp の最終実行。`--logo` 明示時・`--no-logo` 時はここで
    // 必ず1回実行する（従来どおり）。自動推定モードでロゴが見つからなかった
    // 場合だけ、1回目の結果（既に `trim_avs_path`/`detail_jls_path` に書かれて
    // いる）をそのまま使い、ここでは実行しない（issue #135「罠」: 2回目を
    // 走らせると「ロゴ無しの現状」と一致しなくなる恐れがある）。
    let need_final_join_logo_scp = !(auto_detect && inlogo_path.is_none());
    if need_final_join_logo_scp {
        run_join_logo_scp(
            join_logo_scp_path,
            &scp_path,
            &jl_file,
            &trim_avs_path,
            &detail_jls_path,
            inlogo_path.as_deref(),
            &config.jls_set,
            work.path(),
        )?;
    } else {
        eprintln!(
            "[analyze] 自動推定: ロゴが見つからなかったため、1回目の join_logo_scp\
             の結果（ロゴ無し）をそのまま使います（2回目は実行しません）。"
        );
    }

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

/// join_logo_scp を実行する（issue #135 で `run_pipeline` から2回呼べるよう
/// 切り出した。`inlogo_path` が `Some` なら `-inlogo` を `-set` 群より前に置く。
/// join_logo_scp はオプションを左から順に処理して同じ項目を上書きするため
/// （issue #97「罠」）。引数の組み立て自体は元々 `run_pipeline` に直接
/// 書かれていたコードと同一で、この切り出しによる挙動の変化は無い。
#[allow(clippy::too_many_arguments)]
fn run_join_logo_scp(
    join_logo_scp_path: &Path,
    scp_path: &Path,
    jl_file: &Path,
    trim_avs_path: &Path,
    detail_jls_path: &Path,
    inlogo_path: Option<&Path>,
    jls_set_overrides: &[(String, String)],
    cwd: &Path,
) -> Result<()> {
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
    if let Some(inlogo_path) = inlogo_path {
        join_logo_scp_args.push(OsStr::new("-inlogo"));
        join_logo_scp_args.push(inlogo_path.as_os_str());
    }
    let set_args = build_jls_set_args(DEFAULT_JLS_SET, jls_set_overrides);
    join_logo_scp_args.extend(set_args.iter().map(|s| OsStr::new(s.as_str())));

    external::run(join_logo_scp_path, &join_logo_scp_args, cwd)?;
    Ok(())
}

/// 自動推定（`--logo`/`--no-logo` 省略時、既定）のロゴ検出経路（issue #135
/// 「解くべき問題」「やること 2」）。
///
/// 呼び出し時点で1回目の join_logo_scp（`-inlogo` 無し）は実行済みで、
/// `no_logo_trim` はその結果（保持区間 = 本編と判定された区間）。次の順で
/// ロゴを決めようとし、**成功した時点で打ち切る**（直列ループ、issue #135
/// 「改訂」）:
///
/// 1. ロゴ辞書（[`dict::select_candidate`]）に解像度が一致する候補があれば、
///    学習を一切行わずその `.lgd` をそのまま [`detect_logo`] に渡す（学習は
///    実測30分1080pで21.5秒かかるため、辞書ヒット判定は学習より前に置く。
///    issue #135「やること 2-5」）。検出まで成功すればそれを採用する。
/// 2. 辞書で決まらなければ（候補なし、または検出の閾値未達）、
///    [`estimate::estimate_candidates`] で候補列（AUC順の採用列）を推定し、
///    先頭から最大 [`MAX_AUTO_TRAINING_CANDIDATES`] 件を順に [`scan::run`] で
///    学習する。学習失敗（回帰係数 NaN 等、issue #135「罠」）は次候補へ進む
///    安全弁で、エラーにはしない。学習できたら [`detect_logo`]（変更なし）を
///    呼び、検出に失敗（logoframe が閾値未満）した場合も次候補へ進む。
///
/// 成功した候補の `.lgd` だけを [`dict::save`] で辞書へ保存する。全候補が
/// 尽きた場合は `Ok(None)`（ロゴ無し。呼び出し側は1回目の結果をそのまま使い、
/// 2回目の join_logo_scp を実行しない）。
///
/// `naming_source` は保存する `.lgd` のファイル名・`LogoData.name` の元にする
/// パス（[`dict_naming_source`] の戻り値）。`work_mp4`（実処理に使う入力、
/// `auto` 経由では `prepare` 後の一時パス）とは別に受け取る理由はレビュー
/// 指摘（issue #135）参照: `work_mp4` をそのまま使うと、`auto` 経由では
/// stem が常に `input_prepared` になり、辞書を見ても局が分からなくなる。
#[allow(clippy::too_many_arguments)]
fn run_auto_logo_detection(
    ffmpeg_path: &Path,
    work_mp4: &Path,
    cwd: &Path,
    dtvi: &Dtvi,
    logoframe_path: &Path,
    dict_dir: &Path,
    no_logo_trim: &TrimList,
    naming_source: &Path,
) -> Result<Option<PathBuf>> {
    let video_size = dtvi_video_size(dtvi)?;
    // `.dtvi` ヘッダの `frame_count` を使う（`dtvi.frames.len()` ではない）。
    // `detect_logo`（`frames::stream_luma_frames` 経由）と `scan::run`
    // （`MakeLogoConfig::frame_count`）はどちらもこのヘッダ値を
    // `expected_frame_count` として使うため、規則を揃える（レビュー指摘）。
    // `dtvi.frames` はテスト用に意図的に切り詰められることがあり
    // （`tests/data/sample.dtvi` 等）、`dtvi.frames.len()` を使うとその場合
    // だけ値が食い違う。
    let total_frames = u32::try_from(dtvi_frame_count(dtvi)?)
        .map_err(|_| anyhow!(".dtvi の frame_count が u32 の範囲を超えています"))?;
    let duration_seconds = total_duration_seconds(dtvi);

    // 1. ロゴ辞書に解像度一致の候補があるか。
    let dict_selection =
        dict::select_candidate(dict_dir, video_size, duration_seconds, |on_frame| {
            frames::stream_keyframe_luma_frames(ffmpeg_path, work_mp4, cwd, video_size, on_frame)
        })?;

    if let Some(selection) = dict_selection {
        eprintln!(
            "[analyze] ロゴ辞書から候補が選ばれました: {}（検出割合 {:.3}）。学習を\
             スキップして検出を試みます。",
            selection.path.display(),
            selection.detected_fraction
        );
        if let Some(inlogo) = detect_logo(
            ffmpeg_path,
            &selection.path,
            work_mp4,
            cwd,
            dtvi,
            logoframe_path,
        )? {
            eprintln!("[analyze] 辞書の候補で検出に成功しました。-inlogo を渡します。");
            return Ok(Some(inlogo));
        }
        eprintln!(
            "[analyze] 辞書の候補は検出の閾値を満たさなかったため、候補列の推定に\
             フォールバックします。"
        );
    } else {
        eprintln!(
            "[analyze] ロゴ辞書（解像度 {}x{}）に該当する候補はありません。候補列の\
             推定を行います。",
            video_size.width, video_size.height
        );
    }

    // 2. 標本の通し番号 → フレーム番号 → 本編/CM の分類器を組み立てる。
    //    実際のキーフレーム位置（`.dtvi` の `frame_number`）から作り、GOP=120の
    //    等間隔仮定では計算しない（CLAUDE.md 罠3、issue #135「罠」）。
    let keyframe_frame_numbers = dtvi_keyframe_frame_numbers(dtvi);
    let cm_ranges = cm_ranges_from_trim(no_logo_trim, total_frames);
    eprintln!(
        "[analyze] 自動推定: キーフレーム{}枚、CM区間{}個（ロゴ無しの結果の補集合）",
        keyframe_frame_numbers.len(),
        cm_ranges.len()
    );

    // `estimate_candidates` に渡す `classify_sample` は「ffmpeg がキーフレームを
    // 流す順の通し番号」しか受け取らない。`.dtvi` のキーパケット（mp4 の同期
    // サンプル由来）と ffmpeg の `-skip_frame nokey`（デコーダの IDR 判定由来）は
    // 別経路のため、**枚数が一致する保証は無い**（レビュー指摘: フィクスチャや
    // 手元の実測クリップで偶然一致していただけ）。対応がずれると静かに間違った
    // 候補が選ばれる（例外は飛ばない、issue #135「罠」・CLAUDE.md 罠3）ため、
    // 「ffmpeg が実際に流したキーフレーム数」と「.dtvi のキーフレーム数」の
    // **両方向**の食い違いを検査する必要がある。
    //
    // `classify_sample` が呼ばれるたびに観測した `serial` の最大値を記録し
    // （`estimate_candidates` は本編/CM の標本数を数える1回目の走査でも
    // `classify_sample` を呼ぶため、そのタイミングで拾える）、呼び出し後に
    // 「観測した最大 serial + 1」を ffmpeg が実際に流したキーフレーム数として
    // `keyframe_frame_numbers.len()` と突き合わせる。範囲外アクセス（ffmpeg 側が
    // 多い場合）は `keyframe_frame_numbers.get` が `None` を返す（ダミー値を返して
    // 続行するが、最大 serial は正しく記録されるため下記の一致検査で必ず捕まる）。
    // ffmpeg 側が少ない場合は `serial` が常に範囲内に収まるため、
    // `estimate_raw` の1回目/2回目の一致検査（`actual == total` の `ensure!`）と
    // 同じ水準の検査がここでも要る。
    let max_serial_seen: std::cell::Cell<Option<u64>> = std::cell::Cell::new(None);
    let classify_sample = |serial: u64| -> SampleLabel {
        let updated = max_serial_seen.get().map_or(serial, |m| m.max(serial));
        max_serial_seen.set(Some(updated));
        match keyframe_frame_numbers.get(serial as usize) {
            Some(&frame_number) => classify_frame_number(frame_number, &cm_ranges),
            None => SampleLabel::Program,
        }
    };

    let candidates = estimate::estimate_candidates(
        video_size,
        |on_frame| {
            frames::stream_keyframe_luma_frames(ffmpeg_path, work_mp4, cwd, video_size, on_frame)
        },
        classify_sample,
    )?;
    verify_keyframe_count_matches_dtvi(max_serial_seen.get(), keyframe_frame_numbers.len())?;

    if candidates.is_empty() {
        eprintln!("[analyze] 推定候補がありませんでした。ロゴ無しとして扱います。");
        return Ok(None);
    }

    let total_candidates = candidates.len();
    let tried = total_candidates.min(MAX_AUTO_TRAINING_CANDIDATES);
    eprintln!(
        "[analyze] 推定候補{total_candidates}件のうち先頭{tried}件を順に学習します\
         （上限 {MAX_AUTO_TRAINING_CANDIDATES} 件、候補1件あたり学習+検出で\
         実測30分1080p相当21.5秒超かかるため）。"
    );

    for (index, candidate) in candidates
        .into_iter()
        .take(MAX_AUTO_TRAINING_CANDIDATES)
        .enumerate()
    {
        let attempt = index + 1;
        eprintln!(
            "[analyze] 候補{attempt}/{tried}: 矩形=(x={}, y={}, w={}, h={}) 最大効果量={:.1} \
             の学習を試みます",
            candidate.estimated_rect.x,
            candidate.estimated_rect.y,
            candidate.estimated_rect.w,
            candidate.estimated_rect.h,
            candidate.max_effect,
        );

        let scan_config = scan::MakeLogoConfig {
            ffmpeg: ffmpeg_path.to_path_buf(),
            input: work_mp4.to_path_buf(),
            cwd: cwd.to_path_buf(),
            rect: candidate.estimated_rect,
            video_size,
            frame_count: u64::from(total_frames),
            threshold: scan::DEFAULT_THRESHOLD,
            name: auto_logo_name(naming_source),
            service_id: scan::UNSPECIFIED_SERVICE_ID,
        };

        // 学習失敗（回帰係数 NaN 等）は「使えない候補」を弾く安全弁であり、
        // 直列ループの前提（issue #135「罠」）。エラーにせず次候補へ進む。
        let scan_output = match scan::run(&scan_config) {
            Ok(output) => output,
            Err(err) => {
                eprintln!("[analyze] 候補{attempt}: 学習に失敗したため次候補へ進みます: {err}");
                continue;
            }
        };

        // 学習した候補を一旦キャッシュ内に `.lgd` として書き、既存の
        // `detect_logo`（変更なし）にそのまま渡す。
        let candidate_lgd_path = cwd.join(format!("logo-candidate-{attempt}.lgd"));
        scan::write_lgd(&scan_output.logo, &candidate_lgd_path)?;

        let detect_result = detect_logo(
            ffmpeg_path,
            &candidate_lgd_path,
            work_mp4,
            cwd,
            dtvi,
            logoframe_path,
        )?;
        // 作業ディレクトリに書いた候補の `.lgd` は使い終わったら消す
        // （レビュー指摘: 失敗した候補ぶんが残ると紛らわしい。成功した場合も
        // `dict::save` が辞書側に別途保存するため、この一時ファイルは不要）。
        remove_scratch_candidate_lgd(&candidate_lgd_path);

        match detect_result {
            Some(inlogo) => {
                eprintln!("[analyze] 候補{attempt}: 検出に成功しました。ロゴ辞書へ保存します。");
                match dict::save(dict_dir, &scan_output.logo, naming_source) {
                    Ok(saved_path) => {
                        eprintln!("[analyze] ロゴ辞書へ保存しました: {}", saved_path.display());
                    }
                    Err(err) => {
                        eprintln!(
                            "[analyze] 警告: ロゴ辞書への保存に失敗しました（続行します）: {err}"
                        );
                    }
                }
                return Ok(Some(inlogo));
            }
            None => {
                eprintln!("[analyze] 候補{attempt}: 検出に失敗したため次候補へ進みます。");
            }
        }
    }

    eprintln!(
        "[analyze] 全ての候補で検出に失敗したため、ロゴ無しとして扱います\
         （1回目の join_logo_scp の結果をそのまま使います）。"
    );
    Ok(None)
}

/// ffmpeg が実際に流したキーフレーム数と `.dtvi` 由来のキーフレーム数が一致する
/// ことを検査する（レビュー指摘、issue #135「罠」・CLAUDE.md 罠3の一般形）。
///
/// `.dtvi` のキーパケット（mp4 の同期サンプル由来）と ffmpeg の
/// `-skip_frame nokey`（デコーダの IDR 判定由来）は別経路のため、**枚数が
/// 一致する保証は無い**（手元のフィクスチャや実測クリップで偶然一致していた
/// だけ）。対応がずれると `classify_sample`（標本の通し番号→フレーム番号→
/// 本編/CM）が静かに間違ったラベルを返し、静かに間違った候補が選ばれる
/// （例外は飛ばない）。
///
/// - `max_serial_seen`: `classify_sample` が観測した `serial`（0始まり、
///   `estimate_candidates` が渡す通し番号）の最大値。一度も呼ばれなかった
///   場合（候補が最初から無い等）は `None` で、その場合は検査のしようが
///   無いためスキップする。
/// - `dtvi_keyframe_count`: `.dtvi` 由来のキーフレーム数
///   （[`dtvi_keyframe_frame_numbers`] の要素数）。
///
/// `ffmpeg が流した実際のキーフレーム数 = max_serial_seen + 1`
/// （`classify_sample` は `0..実際の枚数` の範囲で呼ばれるため）と
/// `dtvi_keyframe_count` を比較する。
///
/// - **ffmpeg 側が多い場合**: `classify_sample` の中で
///   `keyframe_frame_numbers.get(serial)` が範囲外になり `None` を返すが、
///   `max_serial_seen` 自体はその大きい値を正しく記録するため、この関数で
///   不一致として検出できる
/// - **ffmpeg 側が少ない場合**: `serial` は常に `.dtvi` 側の範囲内に収まる
///   ため、範囲外アクセスによる検出はできない。`estimate_raw` の1回目/2回目の
///   フレーム数一致検査（`actual == total` の `ensure!`）と同じ水準の検査が
///   必要で、この関数はその役目を兼ねる
///
/// プロセスを起動せずに検証できるよう、`run_auto_logo_detection` から分離した
/// 純粋関数にしている。
fn verify_keyframe_count_matches_dtvi(
    max_serial_seen: Option<u64>,
    dtvi_keyframe_count: usize,
) -> Result<()> {
    let Some(max_serial) = max_serial_seen else {
        return Ok(());
    };
    let ffmpeg_keyframe_count = max_serial + 1;
    anyhow::ensure!(
        ffmpeg_keyframe_count == dtvi_keyframe_count as u64,
        "ffmpeg が実際に流したキーフレーム数({ffmpeg_keyframe_count}枚、観測した標本の\
         通し番号の最大値+1から算出)が、.dtvi のキーフレーム数\
         ({dtvi_keyframe_count}枚、キーパケットの frame_number の個数)と一致しません。\
         両者は別経路（mp4 の同期サンプル vs ffmpeg の -skip_frame nokey）のため、枚数の\
         一致は保証されていません。対応がずれると本編のフレームが CM 群に混ざったまま\
         採点され、静かに間違った候補が選ばれるため中断します\
         （issue #135「罠」、CLAUDE.md 罠3の一般形）。"
    );
    Ok(())
}

/// 候補の学習結果を一時的に書いた `.lgd`（`logo-candidate-{n}.lgd`）を削除する
/// （レビュー指摘: 成功・失敗どちらでも作業ディレクトリに残ると紛らわしい。
/// 成功した候補は `dict::save` が辞書側に別途保存するため、この一時ファイルは
/// もう要らない）。`clear_stale_logoframe` と同じ扱いで、削除に失敗しても
/// 警告に留め `run_auto_logo_detection` 自体は失敗させない。
fn remove_scratch_candidate_lgd(path: &Path) {
    match fs::remove_file(path) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            eprintln!(
                "[analyze] 警告: 候補の一時 .lgd の削除に失敗しました（処理は続行します）: \
                 {} ({err})",
                path.display()
            );
        }
    }
}

/// `.dtvi` のキーパケット（同期サンプル相当）の `frame_number`（表示順）一覧を
/// 昇順で返す。`dtvi.frames` は既に `frame_number` の昇順（0始まり連番）で
/// 並んでいる（`dtvi.rs` のパース時に保証済み）ため、フィルタするだけでよい。
fn dtvi_keyframe_frame_numbers(dtvi: &Dtvi) -> Vec<u32> {
    dtvi.frames
        .iter()
        .filter(|f| f.is_key_packet())
        .map(|f| f.frame_number.0)
        .collect()
}

/// `trim`（保持区間、表示順フレーム番号の半開区間、昇順・非重複）の補集合を
/// CM区間として返す（issue #135「やること 2-3」）。`total_frames` は総フレーム数
/// （`.dtvi` の `frame_number` の総数、`dtvi.frames.len()`）。
fn cm_ranges_from_trim(trim: &TrimList, total_frames: u32) -> Vec<(u32, u32)> {
    let mut ranges = Vec::new();
    let mut cursor = 0u32;
    for range in trim.ranges() {
        let start = range.start().0;
        if cursor < start {
            ranges.push((cursor, start));
        }
        cursor = cursor.max(range.end().0);
    }
    if cursor < total_frames {
        ranges.push((cursor, total_frames));
    }
    ranges
}

/// `frame_number` が `cm_ranges`（半開区間の一覧）のいずれかに含まれるかで
/// 本編/CMを判定する。
fn classify_frame_number(frame_number: u32, cm_ranges: &[(u32, u32)]) -> SampleLabel {
    let is_cm = cm_ranges
        .iter()
        .any(|&(start, end)| frame_number >= start && frame_number < end);
    if is_cm {
        SampleLabel::Cm
    } else {
        SampleLabel::Program
    }
}

/// `.dtvi` の総フレーム数と fps から映像長（秒）を求める（[`dict::select_candidate`]
/// の `duration_seconds` 引数向け。`detect_logo` 内の同種の計算と同じ規則）。
fn total_duration_seconds(dtvi: &Dtvi) -> f64 {
    let fps = dtvi_fps(dtvi);
    let total_frames = dtvi.frames.len() as f64;
    if fps > 0.0 {
        total_frames / fps
    } else {
        f64::INFINITY
    }
}

/// 自動推定で学習した [`LogoData`] の `name` フィールド。入力ファイル名
/// （拡張子を除く）を使う（`commands::logo_name_from_input`（`make-logo`
/// サブコマンド）と同じ考え方だが、`commands.rs` に依存しないようこの
/// モジュールで独立に定義する）。
fn auto_logo_name(input: &Path) -> String {
    input
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("tachikaze-auto-logo")
        .to_string()
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
/// この関数の主コストは上記のフルデコード（実測30分1080pで約19秒・CPU約162秒）。
/// ロゴ・入力が変わらない再実行（jls-settings の調整反復、`auto` のやり直し）
/// でも毎回払うため、corr スコア列（`scores`）を `cwd`（キャッシュディレクトリ、
/// `--cache-dir` 配下）に `logo-scores-<キー>.bin` として保存し、次回以降は
/// デコードそのものを省略する（issue #152「解くべき問題」）。キーは
/// [`score_cache_key`] が `.lgd` のバイト列 + `expected_frame_count` に加え、
/// `.dtvi` ヘッダの `source_size`/`source_mtime_ns`/`source_fingerprint`
/// （入力ファイルそのものの識別子）から作る。**レビュー指摘**: 当初は
/// `.lgd` + `expected_frame_count` だけだったため、`workdir::cache_dir_for_input`
/// と同じ「同じパスに別内容のファイルが置かれた場合を区別できない」リスク
/// （`src/auto.rs` モジュール doc comment「キャッシュを短絡しない」）をこの
/// スコアキャッシュ自身が持っていた（同一パスへの録画の上書き後、古いロゴ
/// 区間で誤った logoframe を書く実害を実測で確認）。上記3項目を鍵に含めた
/// ことで、入力の実体が変われば必ず別キーになる。
///
/// キャッシュファイルには magic・形式版・[`ScoreCacheDerivation`]（スコア列の
/// 導出方法）も書き込む（レビュー指摘: `.lgd`/フレーム数/入力識別子が同じでも、
/// スコア列を作る**アルゴリズム自体**が変わった場合に古いキャッシュを黙って
/// 再利用しないため）。いずれか一致しない場合はキャッシュを無視してフルパスへ
/// 落とす（[`read_score_cache`] 参照）。
///
/// ヒット時に読み取った要素数が `expected_frame_count` と一致しない場合も
/// キャッシュを無視してフルパスへ落とす（CLAUDE.md 罠3の一般形）。キャッシュは
/// 最適化であり無くても正しく動くため、書き込み失敗は警告に留めて続行する。
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

    let lgd_bytes = fs::read(lgd_path).path_ctx(".lgd の読み込み（キャッシュキー用）", lgd_path)?;
    let score_cache_file =
        score_cache_path(cwd, score_cache_key(&lgd_bytes, expected_frame_count, dtvi));

    let mut scores: Vec<(f32, f32)> = Vec::new();
    let mut loaded_from_cache = false;
    if let Some(cached) = read_score_cache(&score_cache_file, expected_frame_count) {
        eprintln!(
            "[analyze] corr スコア列をキャッシュから読み込みました（デコードを省略します）: {}",
            score_cache_file.display()
        );
        scores = cached;
        loaded_from_cache = true;
    }

    if !loaded_from_cache {
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

        if let Err(err) = write_score_cache(&score_cache_file, &scores) {
            eprintln!(
                "[analyze] 警告: corr スコア列のキャッシュ書き込みに失敗しました\
                 （最適化のためのキャッシュであり、無くても処理は続行します）: {} ({err})",
                score_cache_file.display()
            );
        }
    }

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

// --- corr スコア列キャッシュ（issue #152、レビュー指摘で入力識別子と
// フォーマット検査を追加） ---
//
// `detect_logo` の主コストは `frames::stream_luma_frames` による全編フル
// デコード（実測30分1080pで約19秒）。その出力である `scores`（corr スコア列）
// を `cwd`（キャッシュディレクトリ）内のファイルへ保存し、ロゴ・入力とも
// 変わらない再実行ではデコードそのものを丸ごと省く。

/// FNV-1a（64bit）の実装。`std::collections::hash_map::DefaultHasher` は
/// バージョン間の安定性が保証されない（`Hasher` トレイトのドキュメントに
/// 明記されている）ため、プロセスを跨いで永続化するキャッシュファイル名には
/// 使えない。依存クレートを増やさずに済み、仕様が枯れていて実装も数行で済む
/// FNV-1a を自前で書く。
fn fnv1a_64(bytes: &[u8]) -> u64 {
    const OFFSET_BASIS: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET_BASIS;
    for &b in bytes {
        hash ^= u64::from(b);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// キャッシュキー: `.lgd` のバイト列 + `expected_frame_count` + 入力ファイル
/// 自体の識別子（`.dtvi` ヘッダの `source_size`/`source_mtime_ns`/
/// `source_fingerprint`）から作る（issue #152「やること 2」、レビュー指摘）。
///
/// **レビュー指摘で発覚した実害**: 当初は `.lgd` + `expected_frame_count` だけ
/// だったため、キャッシュファイルのパス（`score_cache_path`）は `cwd`
/// （入力ごとの作業ディレクトリ）に置かれるとはいえ、`cwd` 自体が
/// `workdir::cache_dir_for_input` の「入力絶対パスのハッシュ」だけで決まる
/// （`src/auto.rs` モジュール doc comment「キャッシュを短絡しない」）ため、
/// **同じパスに別内容の録画ファイルが上書きされ、かつ偶然フレーム数まで
/// 一致する場合**、古いスコア列を新しい入力に対して誤って使ってしまう
/// （実測で確認: 上書き後も検出フレーム数が古い動画の値のまま変わらず、
/// 誤った logoframe を書いた）。`source_size`/`source_mtime_ns`/
/// `source_fingerprint` は入力ファイルの実体を識別する値（`.dtvi` ヘッダ、
/// `dtvi.rs` の doc comment参照）なので、これらを鍵に含めれば
/// 「同じパスだが別内容」を確実に別キーにできる。
///
/// `.lgd` が変わる（再学習・別ロゴ）、`.dtvi` の `frame_count` が変わる、
/// または入力の実体が変わると必ず別キーになる。ヘッダにキーが無い場合
/// （将来の `.dtvi` フォーマット変更等）も、値の有無自体を区別できるよう
/// 存在フラグを1バイト挟んで連結する（「キーが無い」と「キーの値が空文字列」
/// が同じバイト列に化けないようにするため）。全体を1回の FNV-1a に通す
/// （FNV-1a はストリーミング可能で、別々にハッシュしてから混ぜるより衝突
/// 耐性の議論が単純）。
fn score_cache_key(lgd_bytes: &[u8], expected_frame_count: u64, dtvi: &Dtvi) -> u64 {
    let mut buf = Vec::with_capacity(lgd_bytes.len() + 8);
    buf.extend_from_slice(lgd_bytes);
    buf.extend_from_slice(&expected_frame_count.to_le_bytes());
    for key in ["source_size", "source_mtime_ns", "source_fingerprint"] {
        match dtvi.header_value(key) {
            Some(value) => {
                buf.push(1);
                buf.extend_from_slice(value.as_bytes());
            }
            None => buf.push(0),
        }
        // フィールドの区切り。値のバイト列の中に紛れ込んでも次のフィールドの
        // 存在フラグ（0/1）が続くため境界の曖昧さは生じない。
        buf.push(0xff);
    }
    fnv1a_64(&buf)
}

/// キャッシュファイルのパス（`cwd` = キャッシュディレクトリ内、
/// `docs/architecture.md`「パス解決」の「キャッシュ（再生成可能な中間物）」に
/// 合致する置き場所）。
///
/// 同じ入力で `.lgd` を再学習すると鍵が変わり、古いキーのファイルは
/// `cwd` に残り続ける。`analyze` を再実行するたびに他候補分も含めて掃除する
/// と、`run_auto_logo_detection` の候補直列ループが同じ `cwd` で複数の `.lgd`
/// を順に試す最中に自分自身の直前の結果まで消してしまいかねず、掃除の範囲を
/// 「今回のキー以外」に絞る判定はこの関数の外（呼び出し元）の知識が要って
/// 複雑になる。`--cache-dir` 配下は元々「消えても `analyze` の再実行で
/// 作り直せる中間物」という規約（`docs/architecture.md`「パス解決」）のため、
/// 古いキーのファイルが溜まる問題は自動掃除を作らず、キャッシュディレクトリ
/// ごと消す既存の運用に委ねる（issue #152「やること 6」）。
fn score_cache_path(cwd: &Path, key: u64) -> PathBuf {
    cwd.join(format!("logo-scores-{key:016x}.bin"))
}

/// キャッシュファイルの先頭に置く magic バイト列。
const SCORE_CACHE_MAGIC: [u8; 4] = *b"TKSC";

/// キャッシュファイルの構造上のバージョン（レビュー指摘、要修正5）。
///
/// ここでいう「構造」は、このファイル自体のバイナリレイアウト（フィールドの
/// 並び・型）を指す。[`SCORE_CACHE_DERIVATION`] とは独立に管理する（後方の
/// フィールド1個を増やすような変更はここだけを上げれば済み、スコア列を作る
/// アルゴリズム自体は変わらないケースがあるため）。
const SCORE_CACHE_FORMAT_VERSION: u32 = 1;

/// キャッシュに保存された `scores` を作った**アルゴリズム自体**の識別子
/// （レビュー指摘、要修正5）。
///
/// `.lgd`・フレーム数・入力識別子がすべて一致していても、`scores` を組み立てる
/// アルゴリズムが変わった場合（例: 全編フルデコードから、キーフレーム走査＋
/// 部分デコードの階層化方式（issue #154）への変更）は古いキャッシュを黙って
/// 再利用してはならない。もし新旧アルゴリズムの出力が理論上同一になるはずでも、
/// 実装のバグ等で食い違う可能性を残さないため、アルゴリズムを変えたら必ず
/// この値も変える。値そのものに意味はなく、[`read_score_cache`] で現在の
/// コードが期待する値と完全一致するかだけを見る。
///
/// `0` = 全編フルデコード（`frames::stream_luma_frames`、issue #152 導入時点の
/// 唯一の経路）。E18-9 で階層化方式を導入する際に新しい値へ切り替える。
const SCORE_CACHE_DERIVATION: u32 = 0;

/// キャッシュファイルのヘッダ長（magic 4バイト + 形式版4バイト + 導出方法
/// タグ4バイト + 要素数8バイト）。
const SCORE_CACHE_HEADER_LEN: usize = 4 + 4 + 4 + 8;

/// `scores` をキャッシュファイルへ書く。フォーマット: magic（4バイト）+
/// 形式版（`u32`, LE）+ 導出方法タグ（`u32`, LE）+ 要素数（`u64`, LE）+
/// `(f32, f32)` のペアを LE で並べたもの。
fn write_score_cache(path: &Path, scores: &[(f32, f32)]) -> std::io::Result<()> {
    let mut buf = Vec::with_capacity(SCORE_CACHE_HEADER_LEN + scores.len() * 8);
    buf.extend_from_slice(&SCORE_CACHE_MAGIC);
    buf.extend_from_slice(&SCORE_CACHE_FORMAT_VERSION.to_le_bytes());
    buf.extend_from_slice(&SCORE_CACHE_DERIVATION.to_le_bytes());
    buf.extend_from_slice(&(scores.len() as u64).to_le_bytes());
    for (a, b) in scores {
        buf.extend_from_slice(&a.to_le_bytes());
        buf.extend_from_slice(&b.to_le_bytes());
    }
    fs::write(path, buf)
}

/// キャッシュファイルから `scores` を復元する。次のいずれかに該当する場合は
/// `None` を返し、呼び出し側はフルパス（フルデコード）に落ちる:
///
/// - ファイルが無い・読めない
/// - magic が一致しない（このツールが書いたキャッシュではない）
/// - [`SCORE_CACHE_FORMAT_VERSION`] または [`SCORE_CACHE_DERIVATION`] が
///   現在のコードの値と一致しない（レビュー指摘、要修正5: `scores` を作る
///   アルゴリズムが変わった後に古いキャッシュを黙って使わないための検査）
/// - 保存されている要素数が `expected_frame_count` と一致しない、または
///   ファイルサイズが要素数から導ける期待値と食い違う（壊れている）
///
/// **要素数検査を省くと静かに壊れる**（CLAUDE.md 罠3の一般形、issue
/// #152「罠」）: フレーム数が違う `.dtvi` に対して古いスコア列を使うと、
/// 例外を出さずロゴ区間がずれた trim が出る。
fn read_score_cache(path: &Path, expected_frame_count: u64) -> Option<Vec<(f32, f32)>> {
    let bytes = fs::read(path).ok()?;
    if bytes.len() < SCORE_CACHE_HEADER_LEN {
        return None;
    }
    if bytes[0..4] != SCORE_CACHE_MAGIC {
        return None;
    }
    let format_version = u32::from_le_bytes(bytes[4..8].try_into().ok()?);
    if format_version != SCORE_CACHE_FORMAT_VERSION {
        return None;
    }
    let derivation = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
    if derivation != SCORE_CACHE_DERIVATION {
        return None;
    }
    let count = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
    if count != expected_frame_count {
        return None;
    }
    let count_usize = usize::try_from(count).ok()?;
    let expected_len = SCORE_CACHE_HEADER_LEN.checked_add(count_usize.checked_mul(8)?)?;
    if bytes.len() != expected_len {
        return None;
    }
    let mut scores = Vec::with_capacity(count_usize);
    let mut offset = SCORE_CACHE_HEADER_LEN;
    for _ in 0..count_usize {
        let a = f32::from_le_bytes(bytes[offset..offset + 4].try_into().ok()?);
        let b = f32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().ok()?);
        scores.push((a, b));
        offset += 8;
    }
    Some(scores)
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

    // --- 自動推定（issue #135）の純粋関数: プロセス起動なしで検証できる ---

    #[test]
    fn validate_logo_flags_rejects_logo_and_no_logo_together() {
        let err = validate_logo_flags(Some(Path::new("/tmp/logo.lgd")), true)
            .expect_err("--logo と --no-logo の併用は拒否するはず");
        assert!(err.to_string().contains("--logo"));
        assert!(err.to_string().contains("--no-logo"));
    }

    #[test]
    fn validate_logo_flags_allows_logo_alone() {
        validate_logo_flags(Some(Path::new("/tmp/logo.lgd")), false)
            .expect("--logo 単独は許可されるはず");
    }

    #[test]
    fn validate_logo_flags_allows_no_logo_alone() {
        validate_logo_flags(None, true).expect("--no-logo 単独は許可されるはず");
    }

    #[test]
    fn validate_logo_flags_allows_both_omitted() {
        validate_logo_flags(None, false).expect("両方省略（自動推定）も許可されるはず");
    }

    /// `frame_number`（表示順）だけを指定して最小限の `DtviFrame` を作る
    /// （`is_key_packet`/`is_leading_sample` 用の `flags` は呼び出し側が渡す）。
    fn dtvi_frame(frame_number: u32, flags: u8) -> crate::dtvi::DtviFrame {
        crate::dtvi::DtviFrame {
            frame_number: crate::order::DisplayIdx(frame_number),
            sample_number: crate::order::DecodeIdx(frame_number),
            random_access_sample: crate::order::DecodeIdx(0),
            file_offset: 0,
            pts: 0,
            dts: 0,
            duration: 1,
            flags,
        }
    }

    #[test]
    fn dtvi_keyframe_frame_numbers_filters_key_packets_in_ascending_order() {
        let dtvi = Dtvi {
            format_version: 1,
            header: std::collections::HashMap::new(),
            frames: vec![
                dtvi_frame(0, crate::dtvi::FLAG_KEY_PACKET),
                dtvi_frame(1, 0),
                dtvi_frame(2, 0),
                dtvi_frame(3, crate::dtvi::FLAG_KEY_PACKET),
                dtvi_frame(4, 0),
            ],
        };
        assert_eq!(dtvi_keyframe_frame_numbers(&dtvi), vec![0, 3]);
    }

    #[test]
    fn dtvi_keyframe_frame_numbers_is_empty_when_no_frame_is_a_key_packet() {
        let dtvi = Dtvi {
            format_version: 1,
            header: std::collections::HashMap::new(),
            frames: vec![dtvi_frame(0, 0), dtvi_frame(1, 0)],
        };
        assert!(dtvi_keyframe_frame_numbers(&dtvi).is_empty());
    }

    #[test]
    fn cm_ranges_from_trim_computes_complement_of_kept_ranges() {
        // Trim(10,19) ++ Trim(30,39) を total_frames=50 の下で解釈すると、
        // 半開区間は [10,20) と [30,40) になる。補集合(CM)は
        // [0,10) [20,30) [40,50) の3区間のはず。
        let trim = TrimList::parse("Trim(10,19) ++ Trim(30,39)").expect("パースできるはず");
        let cm_ranges = cm_ranges_from_trim(&trim, 50);
        assert_eq!(cm_ranges, vec![(0, 10), (20, 30), (40, 50)]);
    }

    #[test]
    fn cm_ranges_from_trim_is_empty_when_trim_covers_all_frames() {
        let trim = TrimList::parse("Trim(0,49)").expect("パースできるはず");
        assert!(cm_ranges_from_trim(&trim, 50).is_empty());
    }

    #[test]
    fn cm_ranges_from_trim_includes_leading_gap_before_first_range() {
        let trim = TrimList::parse("Trim(5,49)").expect("パースできるはず");
        assert_eq!(cm_ranges_from_trim(&trim, 50), vec![(0, 5)]);
    }

    #[test]
    fn classify_frame_number_marks_frames_inside_cm_ranges_as_cm() {
        let cm_ranges = vec![(0u32, 10u32), (20, 30)];
        assert_eq!(classify_frame_number(5, &cm_ranges), SampleLabel::Cm);
        assert_eq!(classify_frame_number(25, &cm_ranges), SampleLabel::Cm);
        // 半開区間の終端は含まない。
        assert_eq!(classify_frame_number(10, &cm_ranges), SampleLabel::Program);
        assert_eq!(classify_frame_number(15, &cm_ranges), SampleLabel::Program);
        assert_eq!(classify_frame_number(30, &cm_ranges), SampleLabel::Program);
    }

    #[test]
    fn classify_frame_number_is_program_when_no_cm_ranges() {
        assert_eq!(classify_frame_number(0, &[]), SampleLabel::Program);
    }

    // --- verify_keyframe_count_matches_dtvi: レビュー指摘（最重要）。
    // ffmpeg が実際に流したキーフレーム数と .dtvi のキーフレーム数の食い違いを
    // 両方向とも検出できることを、プロセスを起動せずに固定する。---

    #[test]
    fn verify_keyframe_count_matches_dtvi_accepts_matching_counts() {
        // classify_sample が 通し番号 0..=4（5枚）まで観測し、.dtvi 側も5枚。
        verify_keyframe_count_matches_dtvi(Some(4), 5).expect("枚数が一致するので成功するはず");
    }

    #[test]
    fn verify_keyframe_count_matches_dtvi_skips_when_never_observed() {
        // classify_sample が一度も呼ばれなかった場合（候補が最初から無い等）は
        // 検査のしようが無いのでスキップする。
        verify_keyframe_count_matches_dtvi(None, 5)
            .expect("観測が無い場合は検査をスキップして成功するはず");
    }

    #[test]
    fn verify_keyframe_count_matches_dtvi_rejects_when_ffmpeg_reports_more_keyframes() {
        // ffmpeg が .dtvi より多くのキーフレームを流した場合（レビュー指摘が
        // 修正前から検出できていた方向）。観測した最大 serial=6 → ffmpeg 側7枚、
        // .dtvi 側は5枚しか無い。
        let err = verify_keyframe_count_matches_dtvi(Some(6), 5)
            .expect_err("ffmpeg 側が多い食い違いはエラーになるはず");
        let message = err.to_string();
        assert!(
            message.contains('7'),
            "ffmpeg 側の枚数(7)が含まれるはず: {message}"
        );
        assert!(
            message.contains('5'),
            ".dtvi 側の枚数(5)が含まれるはず: {message}"
        );
    }

    #[test]
    fn verify_keyframe_count_matches_dtvi_rejects_when_ffmpeg_reports_fewer_keyframes() {
        // 【最重要・レビュー指摘】ffmpeg が .dtvi より少ないキーフレームしか
        // 流さなかった場合。この方向は修正前の実装では `serial` が常に
        // `.dtvi` 側の範囲内に収まるため検出できず、素通りしていた
        // （CLAUDE.md 罠3: 「本編のフレームを CM 群に混ぜたまま採点され、
        // 静かに間違った候補が選ばれる」その現象そのもの）。観測した最大
        // serial=2 → ffmpeg 側3枚しか無いのに、.dtvi 側は5枚ある。
        let err = verify_keyframe_count_matches_dtvi(Some(2), 5)
            .expect_err("ffmpeg 側が少ない食い違いもエラーになるはず");
        let message = err.to_string();
        assert!(
            message.contains('3'),
            "ffmpeg 側の枚数(3)が含まれるはず: {message}"
        );
        assert!(
            message.contains('5'),
            ".dtvi 側の枚数(5)が含まれるはず: {message}"
        );
    }

    #[test]
    fn total_duration_seconds_uses_frame_count_and_fps() {
        let dtvi = dtvi_with_header(&[("frame_rate_num", "30000"), ("frame_rate_den", "1001")]);
        // frames が空でも frame_rate ヘッダは読める（このテストは frames.len()
        // による分子側だけを確認したいので、別テストで非空の frames を使う）。
        assert_eq!(total_duration_seconds(&dtvi), 0.0);

        let mut dtvi_with_frames = dtvi;
        dtvi_with_frames.frames = vec![dtvi_frame(0, 0); 599];
        let seconds = total_duration_seconds(&dtvi_with_frames);
        assert!(
            (seconds - 599.0 / (30000.0 / 1001.0)).abs() < 1e-9,
            "seconds={seconds}"
        );
    }

    #[test]
    fn auto_logo_name_uses_file_stem() {
        assert_eq!(
            auto_logo_name(Path::new("/rec/BS日テレ.mp4")),
            "BS日テレ".to_string()
        );
    }

    #[test]
    fn auto_logo_name_falls_back_when_stem_is_unavailable() {
        assert_eq!(auto_logo_name(Path::new("/")), "tachikaze-auto-logo");
    }

    // --- corr スコア列キャッシュ（issue #152）: プロセス起動なしの純粋ロジックのテスト ---

    #[test]
    fn score_cache_round_trip_restores_scores() {
        let dir = unique_scratch_dir("score-cache-roundtrip");
        let path = dir.join("logo-scores-test.bin");
        let scores: Vec<(f32, f32)> = vec![(0.1, 0.2), (-1.5, 3.25), (0.0, 0.0)];

        write_score_cache(&path, &scores).expect("キャッシュの書き込みに成功するはず");
        let restored =
            read_score_cache(&path, scores.len() as u64).expect("キャッシュの復元に成功するはず");
        assert_eq!(restored, scores);

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_mismatched_frame_count_is_rejected() {
        // 保存されている要素数と expected_frame_count が食い違う場合は、
        // 壊れたキャッシュとして無視してフルパスに落とす（None を返す）。
        let dir = unique_scratch_dir("score-cache-mismatch");
        let path = dir.join("logo-scores-test.bin");
        let scores: Vec<(f32, f32)> = vec![(0.1, 0.2), (-1.5, 3.25), (0.0, 0.0)];

        write_score_cache(&path, &scores).expect("キャッシュの書き込みに成功するはず");
        let restored = read_score_cache(&path, (scores.len() as u64) + 1);
        assert!(restored.is_none(), "要素数不一致なら None を返すはず");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_truncated_file_is_rejected() {
        // ファイルサイズが半端（要素数どおりのバイト数に届かない）な場合も
        // 壊れたキャッシュとして None を返す。
        let dir = unique_scratch_dir("score-cache-truncated");
        let path = dir.join("logo-scores-test.bin");
        let scores: Vec<(f32, f32)> = vec![(0.1, 0.2), (-1.5, 3.25), (0.0, 0.0)];
        write_score_cache(&path, &scores).expect("キャッシュの書き込みに成功するはず");

        let mut bytes = fs::read(&path).unwrap();
        bytes.truncate(bytes.len() - 3);
        fs::write(&path, &bytes).unwrap();

        assert!(read_score_cache(&path, scores.len() as u64).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_missing_file_is_rejected() {
        let dir = unique_scratch_dir("score-cache-missing");
        let path = dir.join("logo-scores-does-not-exist.bin");
        assert!(read_score_cache(&path, 0).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_wrong_magic_is_rejected() {
        // このツールが書いたキャッシュではないファイル（magic不一致）は
        // 無視してフルパスに落とす。
        let dir = unique_scratch_dir("score-cache-wrong-magic");
        let path = dir.join("logo-scores-test.bin");
        let scores: Vec<(f32, f32)> = vec![(0.1, 0.2)];
        write_score_cache(&path, &scores).expect("キャッシュの書き込みに成功するはず");

        let mut bytes = fs::read(&path).unwrap();
        bytes[0..4].copy_from_slice(b"XXXX");
        fs::write(&path, &bytes).unwrap();

        assert!(read_score_cache(&path, scores.len() as u64).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_wrong_format_version_is_rejected() {
        // レビュー指摘（要修正5）: 形式版が現在のコードの値と食い違うキャッシュ
        // は黙って再利用せず、フルパスに落とす。
        let dir = unique_scratch_dir("score-cache-wrong-format-version");
        let path = dir.join("logo-scores-test.bin");
        let scores: Vec<(f32, f32)> = vec![(0.1, 0.2)];
        write_score_cache(&path, &scores).expect("キャッシュの書き込みに成功するはず");

        let mut bytes = fs::read(&path).unwrap();
        let bumped = SCORE_CACHE_FORMAT_VERSION.wrapping_add(1);
        bytes[4..8].copy_from_slice(&bumped.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        assert!(read_score_cache(&path, scores.len() as u64).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_wrong_derivation_is_rejected() {
        // レビュー指摘（要修正5）: `scores` を作るアルゴリズム自体が変わった
        // ことを示す導出方法タグが食い違うキャッシュは黙って再利用しない
        // （E18-9 で階層化方式を導入した際にこの検査が効く）。
        let dir = unique_scratch_dir("score-cache-wrong-derivation");
        let path = dir.join("logo-scores-test.bin");
        let scores: Vec<(f32, f32)> = vec![(0.1, 0.2)];
        write_score_cache(&path, &scores).expect("キャッシュの書き込みに成功するはず");

        let mut bytes = fs::read(&path).unwrap();
        let bumped = SCORE_CACHE_DERIVATION.wrapping_add(1);
        bytes[8..12].copy_from_slice(&bumped.to_le_bytes());
        fs::write(&path, &bytes).unwrap();

        assert!(read_score_cache(&path, scores.len() as u64).is_none());
        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn score_cache_key_differs_when_lgd_bytes_differ() {
        let dtvi = dtvi_with_header(&[]);
        let key_a = score_cache_key(b"lgd-a", 100, &dtvi);
        let key_b = score_cache_key(b"lgd-b", 100, &dtvi);
        assert_ne!(key_a, key_b, ".lgd が変われば別キーになるはず");
    }

    #[test]
    fn score_cache_key_differs_when_frame_count_differs() {
        let dtvi = dtvi_with_header(&[]);
        let key_a = score_cache_key(b"same-lgd", 100, &dtvi);
        let key_b = score_cache_key(b"same-lgd", 101, &dtvi);
        assert_ne!(key_a, key_b, "frame_count が変われば別キーになるはず");
    }

    #[test]
    fn score_cache_key_differs_when_source_identity_differs() {
        // レビュー指摘の実害（同じパスに別内容の録画が上書きされた場合）を
        // 固定するテスト: source_size/source_mtime_ns/source_fingerprint の
        // いずれか1つでも変われば別キーになる。
        let dtvi_a = dtvi_with_header(&[
            ("source_size", "1000"),
            ("source_mtime_ns", "111"),
            ("source_fingerprint", "aaaa"),
        ]);
        let dtvi_b = dtvi_with_header(&[
            ("source_size", "2000"),
            ("source_mtime_ns", "111"),
            ("source_fingerprint", "aaaa"),
        ]);
        let dtvi_c = dtvi_with_header(&[
            ("source_size", "1000"),
            ("source_mtime_ns", "111"),
            ("source_fingerprint", "bbbb"),
        ]);
        let key_a = score_cache_key(b"same-lgd", 100, &dtvi_a);
        let key_b = score_cache_key(b"same-lgd", 100, &dtvi_b);
        let key_c = score_cache_key(b"same-lgd", 100, &dtvi_c);
        assert_ne!(key_a, key_b, "source_size が変われば別キーになるはず");
        assert_ne!(
            key_a, key_c,
            "source_fingerprint が変われば別キーになるはず"
        );
    }

    #[test]
    fn score_cache_key_differs_when_source_identity_header_is_missing_vs_empty() {
        // ヘッダに全く無い場合と、値が空文字列の場合が同じバイト列に化けない
        // ことを確認する（score_cache_key の存在フラグの目的）。
        let dtvi_missing = dtvi_with_header(&[]);
        let dtvi_empty = dtvi_with_header(&[("source_size", "")]);
        let key_missing = score_cache_key(b"lgd", 100, &dtvi_missing);
        let key_empty = score_cache_key(b"lgd", 100, &dtvi_empty);
        assert_ne!(
            key_missing, key_empty,
            "ヘッダに無い場合と空文字列の場合は別キーになるはず"
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
            no_logo: true,
            logo_dir: None,
            source_name_hint: None,
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
            no_logo: true,
            logo_dir: None,
            source_name_hint: None,
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
            no_logo: true,
            logo_dir: None,
            source_name_hint: None,
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
            no_logo: true,
            logo_dir: None,
            source_name_hint: None,
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
            no_logo: true,
            logo_dir: None,
            source_name_hint: None,
        };

        let output = run(&config).expect("analyze パイプラインが成功するはず");
        assert!(!output.trim.ranges().is_empty());

        fs::remove_dir_all(&output_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }
}
