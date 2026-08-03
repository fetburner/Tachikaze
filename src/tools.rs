//! 外部ツール（chapter_exe / join_logo_scp / dtvindex / ffprobe）と
//! JL コマンドファイルの探索。
//!
//! 外部ツールは `PATH` だけを探す（[`resolve_tool`]）。別の場所に置いている
//! ものを使いたければ `PATH=/opt/jls/bin:$PATH tachikaze ...` のように前置すれば
//! よく、インストールしたくない場合は Docker イメージを使う。
//!
//! `PATH` のどこにも見つからない場合は、探した場所（`PATH` の各要素）を全て
//! 列挙したエラーを返す。`ffprobe` のように「無くても致命的ではない」ツールに
//! ついては、この `Result` を呼び出し側が `.ok()` するなどして握り潰し、警告に
//! 変える判断を行う（`--verify` を使うときだけ必要なため）。

use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Context, Result};

/// `chapter_exe` の実行ファイル名。
pub const CHAPTER_EXE: &str = "chapter_exe";
/// `join_logo_scp` の実行ファイル名。
pub const JOIN_LOGO_SCP: &str = "join_logo_scp";
/// `dtvindex` の実行ファイル名。
pub const DTVINDEX: &str = "dtvindex";
/// `ffprobe` の実行ファイル名（`--verify` でのみ必要）。
pub const FFPROBE: &str = "ffprobe";
/// `ffmpeg` の実行ファイル名（`prepare` の elst 除去・字幕抽出でのみ必要）。
pub const FFMPEG: &str = "ffmpeg";

/// 既定の JL コマンドファイル名（ファイル名自体が日本語）。
pub const DEFAULT_JL_COMMAND_FILE: &str = "JL_標準.txt";

/// パスが実行可能なファイルとして使えるかを判定する。
///
/// 実際に起動を試みるのではなく、存在確認（+ Unix では実行権限の確認）だけ
/// を行う。存在確認だけにすることで、探索中に対象を誤って起動してしまう
/// ことを避ける。
#[cfg(unix)]
fn is_executable_file(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    match std::fs::metadata(path) {
        Ok(meta) => meta.is_file() && meta.permissions().mode() & 0o111 != 0,
        Err(_) => false,
    }
}

#[cfg(not(unix))]
fn is_executable_file(path: &Path) -> bool {
    path.is_file()
}

/// パスが読み取り専用データファイルとして使えるかを判定する。
///
/// 実行権限は問わない（データファイルなので）。[`is_executable_file`] とは
/// 判定基準が異なるため別関数にしている。
fn is_regular_file(path: &Path) -> bool {
    path.is_file()
}

/// `PATH` の探索で実際に調べた候補パスを記録しつつ、最初の当たりを返す。
#[derive(Debug, Default)]
struct SearchTrace {
    tried: Vec<PathBuf>,
    /// `PATH` 環境変数自体が未設定だったか（空文字列で設定されている場合とは
    /// 区別する。`PATH` が唯一の探索手段になった今、この2つを区別しないと
    /// 「探した場所」が空欄のまま出力され、原因がわからないエラーになる）。
    path_env_unset: bool,
}

impl SearchTrace {
    fn new() -> Self {
        Self::default()
    }

    /// `PATH` に列挙された各ディレクトリを順に調べる。
    fn try_path_env(&mut self, name: &str) -> Option<PathBuf> {
        let Some(path_var) = env::var_os("PATH") else {
            self.path_env_unset = true;
            return None;
        };
        for dir in env::split_paths(&path_var) {
            let candidate = dir.join(name);
            self.tried.push(candidate.clone());
            if is_executable_file(&candidate) {
                return Some(candidate);
            }
        }
        None
    }

    /// 見つからなかった場合のエラーを、調べた場所を全て列挙して組み立てる。
    ///
    /// `PATH` が未設定、または（環境変数自体はあっても）空だった場合は候補が
    /// 1件も無いため、その旨を明示する（`tried` が空のまま出力すると空行だけ
    /// が残り、原因が分からないエラーになる。実機で `env -u PATH` を使って
    /// 再現した）。
    fn not_found_error(&self, name: &str) -> anyhow::Error {
        let tried_list = if self.path_env_unset {
            "  (`PATH` 環境変数自体が設定されていません)".to_string()
        } else if self.tried.is_empty() {
            "  (`PATH` が空です)".to_string()
        } else {
            self.tried
                .iter()
                .map(|p| format!("  - {}", p.display()))
                .collect::<Vec<_>>()
                .join("\n")
        };
        anyhow!(
            "外部ツール `{name}` が見つかりませんでした。以下の場所を探しました:\n{tried_list}\n\
             `PATH` の通った場所に `{name}` を置いてください（配置は\
             docs/toolchain-macos.md を参照）。インストールしたくない場合は、\
             コンテナ化した実行環境を用意する方法も検討してください。"
        )
    }
}

/// 外部ツールを `PATH` から解決する。
///
/// - `name`: 実行ファイル名（[`CHAPTER_EXE`] などの定数を渡す）。
///
/// 見つかったパスは絶対パスに正規化して返す。`external::run` が作業
/// ディレクトリへ `current_dir` するため、正規化せずに返すと `PATH` 上の相対
/// 表記（`.` を含むエントリなど）が `work/...` を探しにいって起動に失敗する。
///
/// `PATH` のどこにも見つからない場合は、調べた場所（`PATH` の各要素）を全て
/// 列挙したエラーを返す。`ffprobe` のように必須ではないツールは、呼び出し側で
/// `Result` を見て `.ok()` などに変換し、警告に留めるかどうかを判断する。
pub fn resolve_tool(name: &str) -> Result<PathBuf> {
    let mut trace = SearchTrace::new();
    let found = trace
        .try_path_env(name)
        .ok_or_else(|| trace.not_found_error(name))?;
    std::fs::canonicalize(&found).with_context(|| {
        format!(
            "外部ツール `{name}` の絶対パス解決に失敗しました: {}",
            found.display()
        )
    })
}

/// 既定の JL コマンドファイル（`JL_標準.txt`）を、`join_logo_scp` の実体パスから
/// 1段で探す。
///
/// `make install` の配置（`$PREFIX/bin/join_logo_scp` +
/// `$PREFIX/share/join_logo_scp/JL/`、docs/toolchain-macos.md「ビルド後の配置と
/// インストール」節）を前提に、`<join_logo_scp の実体パス>/../../share/
/// join_logo_scp/JL/JL_標準.txt` だけを見る。局別のルールファイル選択は現状
/// 不要（`docs/jls-settings.md` 参照）なので、既定ファイル固定で十分。
/// `--jl-file` が指定された場合はこの関数を呼ばず、呼び出し側がそのパスを
/// そのまま使う。
///
/// `join_logo_scp_path` は [`resolve_tool`] が返す canonicalize 済みのパスを
/// 渡すこと。symlink のまま親を辿ると `../../share` が別の場所を指してしまう。
///
/// 見つからない場合は、`--jl-file` オプションで直接指定するよう案内するエラーを
/// 返す。
pub fn default_jl_command_file(join_logo_scp_path: &Path) -> Result<PathBuf> {
    let candidate = join_logo_scp_path
        .parent()
        .and_then(Path::parent)
        .map(|prefix_dir| {
            prefix_dir
                .join("share")
                .join("join_logo_scp")
                .join("JL")
                .join(DEFAULT_JL_COMMAND_FILE)
        });

    if let Some(candidate) = &candidate {
        if is_regular_file(candidate) {
            return Ok(candidate.clone());
        }
    }

    let tried = candidate
        .as_deref()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|| {
            format!(
                "<{} の親の親>/share/join_logo_scp/JL/{DEFAULT_JL_COMMAND_FILE}",
                join_logo_scp_path.display()
            )
        });

    Err(anyhow!(
        "既定の JL コマンドファイル `{DEFAULT_JL_COMMAND_FILE}` が見つかりませんでした: {tried}\n\
         `--jl-file` オプションで直接指定してください。"
    ))
}

/// `chapter_exe` の起動時ログ（`chapter_exe: AviSynth=enabled, dtvindex=enabled`
/// 相当の行）から `dtvindex` 入力経路が有効かどうかを判定する。
///
/// macOS には AviSynth が無いため、`dtvindex=disabled` なビルドを渡されると
/// 入力経路が存在せず静かに動かなくなる（`docs/toolchain-macos.md`）。判定
/// できない場合は `None` を返す。
pub fn dtvindex_enabled_from_output(output: &str) -> Option<bool> {
    output
        .lines()
        .find(|line| line.contains("dtvindex="))
        .map(|line| line.contains("dtvindex=enabled"))
}

/// `PATH` の書き換えを伴うテストで共有する仕組み。
///
/// `tools.rs` 自身のテストと `analyze.rs` のテストの両方が、`resolve_tool` の
/// 解決結果を制御するために同じ環境変数（`PATH`）を書き換える（`resolve_tool`
/// が `PATH` しか見なくなったため、ツール解決の成功/失敗を作り分けるには
/// `PATH` の書き換えが唯一の手段になった）。モジュールごとに別々の `Mutex` を
/// 持つと、互いの書き換えを直列化できずレースする（`crate::workdir::test_support`
/// の doc comment に、同じ問題が `fs::remove_dir_all` の失敗として実際に顕在化
/// した実例がある）。そのため1つのロックをここに集約し、両モジュールから使う。
#[cfg(test)]
pub(crate) mod test_support {
    use std::env;
    use std::sync::Mutex;

    /// `PATH` の書き換えを伴うテストを直列化するためのロック（`cargo test` は
    /// デフォルトでテストを並行実行するため、プロセス全体で共有される環境変数を
    /// 書き換えるテストは互いに競合してしまう）。
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 環境変数を差し替え、Drop で元の値に戻すガード（`ENV_LOCK` と併用する）。
    pub(crate) struct EnvVarGuard {
        key: &'static str,
        original: Option<std::ffi::OsString>,
    }

    impl EnvVarGuard {
        pub(crate) fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let original = env::var_os(key);
            env::set_var(key, value);
            Self { key, original }
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            match &self.original {
                Some(v) => env::set_var(self.key, v),
                None => env::remove_var(self.key),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{EnvVarGuard, ENV_LOCK};
    use super::*;
    use std::fs;

    /// テスト用の一時ディレクトリを作り、Drop で自動的に削除するガード。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            let path = env::temp_dir().join(format!(
                "tachikaze-tools-test-{label}-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            fs::create_dir_all(&path).expect("create temp dir");
            Self { path }
        }

        fn path(&self) -> &Path {
            &self.path
        }

        /// 実行可能なダミーファイル `name` をこのディレクトリに作る。
        #[cfg(unix)]
        fn make_executable(&self, name: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt;
            let file = self.path.join(name);
            fs::write(&file, "#!/bin/sh\nexit 0\n").expect("write dummy tool");
            let mut perms = fs::metadata(&file).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&file, perms).expect("chmod dummy tool");
            file
        }

        #[cfg(not(unix))]
        fn make_executable(&self, name: &str) -> PathBuf {
            let file = self.path.join(name);
            fs::write(&file, "dummy").expect("write dummy tool");
            file
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn is_executable_file_rejects_directories_and_missing_paths() {
        let dir = TempDir::new("exec-check");
        assert!(!is_executable_file(dir.path()));
        assert!(!is_executable_file(&dir.path().join("does-not-exist")));

        let exe = dir.make_executable("some-tool");
        assert!(is_executable_file(&exe));
    }

    #[test]
    fn resolve_tool_finds_binary_via_path() {
        let _guard = ENV_LOCK.lock().unwrap();

        let path_dir = TempDir::new("resolve-tool-path");
        let expected = path_dir.make_executable(CHAPTER_EXE);

        let _path_env = EnvVarGuard::set("PATH", path_dir.path());

        let result = resolve_tool(CHAPTER_EXE);

        // canonicalize 後のパスと比較する（macOS では /var → /private/var）。
        let expected = fs::canonicalize(&expected).expect("canonicalize expected");
        assert_eq!(result.expect("should resolve via PATH"), expected);
    }

    #[test]
    fn resolve_tool_reports_all_searched_path_dirs_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();

        let dir1 = TempDir::new("missing-path-1");
        let dir2 = TempDir::new("missing-path-2");

        let path_value = env::join_paths([dir1.path(), dir2.path()]).unwrap();
        let _path_env = EnvVarGuard::set("PATH", path_value);

        let err =
            resolve_tool("no-such-tool-xyz").expect_err("PATH のどこにも無いので失敗するはず");
        let message = err.to_string();
        assert!(message.contains("no-such-tool-xyz"));
        assert!(message.contains(&dir1.path().join("no-such-tool-xyz").display().to_string()));
        assert!(message.contains(&dir2.path().join("no-such-tool-xyz").display().to_string()));
    }

    /// `<bin_dir>/join_logo_scp` ダミーを作り、そのパスを返す。
    fn make_join_logo_scp(bin_dir: &Path) -> PathBuf {
        fs::create_dir_all(bin_dir).expect("create bin dir");
        let path = bin_dir.join(JOIN_LOGO_SCP);
        fs::write(&path, "dummy").expect("write dummy join_logo_scp");
        path
    }

    fn write_jl_file(dir: &Path) -> PathBuf {
        fs::create_dir_all(dir).expect("create JL dir");
        let file = dir.join(DEFAULT_JL_COMMAND_FILE);
        fs::write(&file, "dummy JL command file").expect("write JL file");
        file
    }

    #[test]
    fn default_jl_command_file_uses_prefix_relative_share_dir() {
        let prefix = TempDir::new("jl-prefix-share");
        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);

        let expected = write_jl_file(&prefix.path().join("share").join("join_logo_scp").join("JL"));

        let found = default_jl_command_file(&join_logo_scp_path).expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn default_jl_command_file_missing_is_reported_clearly() {
        let prefix = TempDir::new("jl-missing-prefix");
        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);

        let err = default_jl_command_file(&join_logo_scp_path)
            .expect_err("JL ファイルが無いので失敗するはず");
        let message = err.to_string();

        let expected_path = prefix
            .path()
            .join("share")
            .join("join_logo_scp")
            .join("JL")
            .join(DEFAULT_JL_COMMAND_FILE);
        assert!(message.contains(&expected_path.display().to_string()));
        assert!(message.contains("--jl-file"));
    }

    #[test]
    fn dtvindex_enabled_from_output_detects_enabled_and_disabled() {
        assert_eq!(
            dtvindex_enabled_from_output("chapter_exe: AviSynth=enabled, dtvindex=enabled"),
            Some(true)
        );
        assert_eq!(
            dtvindex_enabled_from_output("chapter_exe: AviSynth=enabled, dtvindex=disabled"),
            Some(false)
        );
        assert_eq!(dtvindex_enabled_from_output("no relevant line here"), None);
    }
}
