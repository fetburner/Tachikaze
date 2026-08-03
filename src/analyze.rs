//! analyze コマンド: `dtvindex build` → `chapter_exe -v` → `join_logo_scp` の
//! 3 ツールパイプラインを実行し、`trim.avs` を生成する。
//!
//! 処理の流れ（`docs/pipeline.md` の「全体像」節）:
//!
//! ```text
//! work.mp4 (入力への symlink)
//!   ├─ dtvindex build work.mp4 -o work.mp4.dtvi
//!   ├─ chapter_exe -v work.mp4 -o scp.txt
//!   └─ join_logo_scp -inscp scp.txt -incmd <JL command file> \
//!          -o trim.avs -oscp detail.jls -set autocm_sub 11 -set param_cuttr 1
//! ```
//!
//! `-inlogo` は付けない（`docs/jls-settings.md`）。対象は delogo 済みのため、
//! ロゴ検出は原理的に使えない。省略すると join_logo_scp は全フレームをロゴ
//! 表示中とみなす。

use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::dtvi::{self, Dtvi};
use crate::errctx::PathContext;
use crate::external;
use crate::jls::{self, JlsEntry};
use crate::tools::{self, CHAPTER_EXE, DTVINDEX, JOIN_LOGO_SCP};
use crate::trim::TrimList;
use crate::workdir::WorkDir;

/// join_logo_scp に既定で渡す `-set KEY VALUE`（根拠は `docs/jls-settings.md`）。
///
/// - `autocm_sub=11`: 既定の `10` では「先頭 15 秒単位構成は少数でも CM 化」が
///   無効なままで、番組冒頭の CM 30 秒が残る。
/// - `param_cuttr=1`: 既定の `0` では番宣が `Trailer(cut-cancel)` として残る。
///   `1` にすると末尾 50 秒が除去される。
const DEFAULT_JLS_SET: &[(&str, &str)] = &[("autocm_sub", "11"), ("param_cuttr", "1")];

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
/// が実在しない入力パスでこの順序を検証している）。
pub fn run(config: &AnalyzeConfig) -> Result<AnalyzeOutput> {
    let dtvindex_path = tools::resolve_tool(DTVINDEX)?;
    let chapter_exe_path = tools::resolve_tool(CHAPTER_EXE)?;
    let join_logo_scp_path = tools::resolve_tool(JOIN_LOGO_SCP)?;

    let work = WorkDir::new(config.cache_dir.as_deref(), &config.input)?;
    let result = run_pipeline(
        config,
        &work,
        &dtvindex_path,
        &chapter_exe_path,
        &join_logo_scp_path,
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
        Some(path) => {
            fs::canonicalize(path).path_ctx("JL コマンドファイルの絶対パス解決", path)?
        }
        None => tools::default_jl_command_file(join_logo_scp_path)?,
    };

    let set_args = build_jls_set_args(DEFAULT_JLS_SET, &config.jls_set);
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
    join_logo_scp_args.extend(set_args.iter().map(|s| OsStr::new(s.as_str())));

    external::run(join_logo_scp_path, &join_logo_scp_args, work.path())?;

    // work 内の trim.avs を先に読む。`-o` が work の trim.avs と同じ
    // パスだと `fs::copy(src, src)` が空ファイルを生む（macOS で実測。前回の
    // 手動実行で hit した）。同一パスならコピーを省略する。
    let output_content = fs::read_to_string(&trim_avs_path)
        .path_ctx("join_logo_scp が生成した trim.avs の読み込み", &trim_avs_path)?;
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

    let jls_content = fs::read_to_string(&detail_jls_path)
        .path_ctx("join_logo_scp が生成した detail.jls の読み込み", &detail_jls_path)?;
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
    // comment参照）。E12-2 でキャッシュの根は `--cache-dir`（引数）に一本化した
    // ため、このモジュールはキャッシュ関連の環境変数を一切読み書きしない
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
        // 完了条件(issue #72): `-o` 省略時（`output: None`）はキャッシュにだけ書き、
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
        };

        let output = run(&config).expect("analyze パイプラインが成功するはず");
        assert!(!output.trim.ranges().is_empty());

        fs::remove_dir_all(&output_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }
}
