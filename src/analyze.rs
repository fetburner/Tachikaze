// CLI からの配線待ち。配線されたら外す。
#![allow(dead_code)]

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

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

use crate::external;
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
    /// 最終的な `trim.avs` の出力先。
    pub output: PathBuf,
    /// `--tool-dir`（外部ツールの探索ディレクトリ）。
    pub tool_dir: Option<PathBuf>,
    /// `--work-dir`（中間ファイルの置き場所）。未指定なら一時ディレクトリ。
    pub work_dir: Option<PathBuf>,
    /// `--jls-set` で上書き・追加された `(KEY, VALUE)`。
    pub jls_set: Vec<(String, String)>,
    /// `--jl-file`。未指定なら `tools::default_jl_command_file` の既定値を使う。
    pub jl_file: Option<PathBuf>,
}

/// analyze パイプラインを実行し、生成された `trim.avs` をパースして返す。
///
/// どこかの段階が失敗した場合、以降の段階は実行せずエラーを伝播する
/// （`external::run` のエラーにはコマンドライン全体と stderr の末尾が
/// 既に含まれている）。成功・失敗いずれの経路でも `WorkDir::finish` を
/// 呼ぶため、実際の処理は [`run_pipeline`] に分離している。
pub fn run(config: &AnalyzeConfig) -> Result<TrimList> {
    let work_dir = WorkDir::new(config.work_dir.clone())?;
    let result = run_pipeline(config, &work_dir);
    work_dir.finish(result.is_ok());
    result
}

fn run_pipeline(config: &AnalyzeConfig, work_dir: &WorkDir) -> Result<TrimList> {
    // ツールの解決を先に行う。見つからない場合は入力ファイルの存在確認や
    // 作業ディレクトリへの symlink 作成より前に、探索場所を列挙したエラーで
    // 早期に失敗させる。
    let dtvindex_path = tools::resolve_tool(config.tool_dir.as_deref(), DTVINDEX)?;
    let chapter_exe_path = tools::resolve_tool(config.tool_dir.as_deref(), CHAPTER_EXE)?;
    let join_logo_scp_path = tools::resolve_tool(config.tool_dir.as_deref(), JOIN_LOGO_SCP)?;

    let work_mp4 = work_dir.link_input(&config.input)?;
    let dtvi_path = work_dir.dtvi_path();
    let scp_path = work_dir.scp_path();
    let trim_avs_path = work_dir.trim_path();
    let detail_jls_path = work_dir.detail_jls_path();

    // `external::run` のエラーには既にコマンドライン全体と stderr の末尾が
    // 含まれているため、追加の `.context()` で包まずそのまま伝播する
    // （包むと `anyhow::Error` の `Display`（`to_string()`）が外側のメッセージ
    // だけを返し、肝心の stderr が隠れてしまう）。
    external::run(
        require_utf8(&dtvindex_path)?,
        &[
            "build",
            require_utf8(&work_mp4)?,
            "-o",
            require_utf8(&dtvi_path)?,
        ],
        work_dir.path(),
    )?;

    external::run(
        require_utf8(&chapter_exe_path)?,
        &[
            "-v",
            require_utf8(&work_mp4)?,
            "-o",
            require_utf8(&scp_path)?,
        ],
        work_dir.path(),
    )?;

    let jl_file = match &config.jl_file {
        Some(path) => path.clone(),
        None => tools::default_jl_command_file(&join_logo_scp_path)?,
    };

    let set_args = build_jls_set_args(DEFAULT_JLS_SET, &config.jls_set);
    let mut join_logo_scp_args: Vec<&str> = vec![
        "-inscp",
        require_utf8(&scp_path)?,
        "-incmd",
        require_utf8(&jl_file)?,
        "-o",
        require_utf8(&trim_avs_path)?,
        "-oscp",
        require_utf8(&detail_jls_path)?,
    ];
    join_logo_scp_args.extend(set_args.iter().map(String::as_str));

    external::run(
        require_utf8(&join_logo_scp_path)?,
        &join_logo_scp_args,
        work_dir.path(),
    )?;

    fs::copy(&trim_avs_path, &config.output).with_context(|| {
        format!(
            "trim.avs のコピーに失敗しました: {} -> {}",
            trim_avs_path.display(),
            config.output.display()
        )
    })?;

    let output_content = fs::read_to_string(&config.output).with_context(|| {
        format!(
            "コピーした trim.avs の読み込みに失敗しました: {}",
            config.output.display()
        )
    })?;

    TrimList::parse(&output_content)
        .map_err(|err| anyhow!("生成された trim.avs のパースに失敗しました: {err}"))
}

/// パスを `&str` として取り出す。UTF-8 でないパスは非対応として扱う。
fn require_utf8(path: &Path) -> Result<&str> {
    path.to_str()
        .ok_or_else(|| anyhow!("パスが UTF-8 として扱えません: {}", path.display()))
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
        let empty_tool_dir = unique_scratch_dir("missing-tools");
        let output_dir = unique_scratch_dir("missing-tools-output");
        let output = output_dir.join("trim.avs");

        let config = AnalyzeConfig {
            // ツール解決が入力の存在確認より前に走るため、実在しないパスでよい。
            input: PathBuf::from("/nonexistent/input-for-analyze-test.mp4"),
            output,
            tool_dir: Some(empty_tool_dir.clone()),
            work_dir: None,
            jls_set: vec![],
            jl_file: None,
        };

        let err = run(&config).expect_err("空の tool_dir では解決に失敗するはず");
        let message = err.to_string();
        assert!(
            message.contains(DTVINDEX),
            "エラーメッセージにツール名が含まれていない: {message}"
        );
        assert!(
            message.contains(&empty_tool_dir.join(DTVINDEX).display().to_string()),
            "エラーメッセージに探索したパスが含まれていない: {message}"
        );

        fs::remove_dir_all(&empty_tool_dir).ok();
        fs::remove_dir_all(&output_dir).ok();
    }

    #[test]
    fn run_stops_pipeline_and_surfaces_stderr_on_first_failure() {
        // completion condition: どれかが失敗したら以降を実行せず、そのツールの
        // stderr を出す。chapter_exe を失敗させ、(1) エラーメッセージに
        // chapter_exe の stderr が含まれること、(2) 後段の join_logo_scp が
        // 一度も起動されないこと（マーカーファイルが作られない）を確認する。
        let tool_dir = unique_scratch_dir("stop-on-failure-tools");
        let input_dir = unique_scratch_dir("stop-on-failure-input");
        let output_dir = unique_scratch_dir("stop-on-failure-output");

        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write dummy input");

        // dtvindex: `-o` の次の引数にダミーの中身を書いて成功する。
        write_executable_script(
            &tool_dir.join(DTVINDEX),
            "#!/bin/sh\nprev=\"\"\nfor a in \"$@\"; do\n  if [ \"$prev\" = \"-o\" ]; then\n    printf 'dummy' > \"$a\"\n  fi\n  prev=\"$a\"\ndone\nexit 0\n",
        );

        // chapter_exe: 常に失敗し、判定用の stderr を出す。
        write_executable_script(
            &tool_dir.join(CHAPTER_EXE),
            "#!/bin/sh\necho 'FAKE CHAPTER_EXE FAILURE' >&2\nexit 5\n",
        );

        // join_logo_scp: 起動されたらマーカーファイルを作る（呼ばれてはいけない）。
        let marker_path = tool_dir.join("join_logo_scp_was_called.marker");
        write_executable_script(
            &tool_dir.join(JOIN_LOGO_SCP),
            &format!("#!/bin/sh\ntouch '{}'\nexit 0\n", marker_path.display()),
        );

        let config = AnalyzeConfig {
            input: input_path,
            output: output_dir.join("trim.avs"),
            tool_dir: Some(tool_dir.clone()),
            work_dir: None,
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

        fs::remove_dir_all(&tool_dir).ok();
        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&output_dir).ok();
    }

    // --- 統合テスト（実バイナリが必要） ---

    /// 実ファイル + 実バイナリ (dtvindex / chapter_exe / join_logo_scp) を使い、
    /// analyze パイプライン全体が `Trim(...)` を含む `TrimList` を返すことを
    /// 確認する統合テスト。
    ///
    /// この環境には3ツールの実バイナリがビルドされておらず、用意にも時間が
    /// かかる（`docs/toolchain-macos.md` 参照）ため既定では無視する。実行する
    /// 場合は該当手順でビルドした上で `TACHIKAZE_TOOL_DIR`（または
    /// `--tool-dir` 相当）と実サンプル mp4 のパスを用意し、
    /// `cargo test -- --ignored` で回すこと。
    #[test]
    #[ignore = "dtvindex/chapter_exe/join_logo_scp の実バイナリと実サンプルmp4が必要（docs/toolchain-macos.md）"]
    fn analyze_run_produces_trim_list_with_real_tools() {
        let output_dir = unique_scratch_dir("integration-output");
        let config = AnalyzeConfig {
            input: PathBuf::from("tests/fixtures/sample.mp4"),
            output: output_dir.join("trim.avs"),
            tool_dir: None,
            work_dir: None,
            jls_set: vec![],
            jl_file: None,
        };

        let trim_list = run(&config).expect("analyze パイプラインが成功するはず");
        assert!(!trim_list.ranges().is_empty());

        fs::remove_dir_all(&output_dir).ok();
    }
}
