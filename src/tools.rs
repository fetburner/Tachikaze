//! 外部ツール（chapter_exe / join_logo_scp / dtvindex / ffprobe）と
//! JL コマンドファイルの探索。
//!
//! 探索順序（最初に見つかったものを使う）:
//!
//! 1. `--tool-dir <DIR>`（CLI のグローバルオプション、[`Cli::tool_dir`](crate::cli::Cli)）
//! 2. 環境変数 `TACHIKAZE_TOOL_DIR`
//! 3. 自分の実行ファイルと同じディレクトリ（配布形態: 外部ツール群と同じ
//!    ディレクトリに本ツールのバイナリを1つ置く）
//! 4. `PATH`
//!
//! いずれの場所でも見つからない場合は、探した場所を全て列挙したエラーを返す。
//! `ffprobe` のように「無くても致命的ではない」ツールについては、この
//! `Result` を呼び出し側が `.ok()` するなどして握り潰し、警告に変える判断を
//! 行う（`--verify` を使うときだけ必要なため）。

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

/// 環境変数を読み、**空文字なら未設定として扱う**（XDG Base Directory 仕様の作法）。
fn non_empty_env(key: &str) -> Option<String> {
    env::var(key).ok().filter(|v| !v.is_empty())
}

/// `${XDG_DATA_HOME:-$HOME/.local/share}` を返す。`$HOME` も取れない場合は `None`。
fn xdg_data_home() -> Option<PathBuf> {
    non_empty_env("XDG_DATA_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share")))
}

/// `$XDG_DATA_DIRS`（既定 `/usr/local/share:/usr/share`）を絶対パスの列として返す。
///
/// XDG 仕様どおり、相対パスの要素は無視する。
fn xdg_data_dirs() -> Vec<PathBuf> {
    let raw =
        non_empty_env("XDG_DATA_DIRS").unwrap_or_else(|| "/usr/local/share:/usr/share".to_string());
    env::split_paths(&raw).filter(|p| p.is_absolute()).collect()
}

/// 探索中に実際に調べた候補パスを記録しつつ、最初の当たりを返す。
#[derive(Debug, Default)]
struct SearchTrace {
    tried: Vec<PathBuf>,
}

impl SearchTrace {
    fn new() -> Self {
        Self::default()
    }

    /// `dir` が `Some` なら `dir/name` を候補として調べる。
    fn try_dir(&mut self, dir: Option<&Path>, name: &str) -> Option<PathBuf> {
        let dir = dir?;
        let candidate = dir.join(name);
        self.tried.push(candidate.clone());
        is_executable_file(&candidate).then_some(candidate)
    }

    /// `PATH` に列挙された各ディレクトリを順に調べる。
    fn try_path_env(&mut self, name: &str) -> Option<PathBuf> {
        let path_var = env::var_os("PATH")?;
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
    fn not_found_error(&self, name: &str) -> anyhow::Error {
        let tried_list = self
            .tried
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow!(
            "外部ツール `{name}` が見つかりませんでした。以下の場所を探しました:\n{tried_list}\n\
             `--tool-dir` オプション、環境変数 `TACHIKAZE_TOOL_DIR`、または PATH で解決できる\
             場所に `{name}` を置いてください。"
        )
    }
}

/// 読み取り専用データファイルの探索で調べた候補パスを記録する。
///
/// [`SearchTrace`] は「ディレクトリ + 実行ファイル名」の組で候補を作るのに
/// 対し、こちらは候補パスの組み立て方が段ごとに異なる（`JL/` を挟むかどうか
/// など）ため、完成した候補パスをそのまま受け取る。
#[derive(Debug, Default)]
struct DataFileSearchTrace {
    tried: Vec<PathBuf>,
}

impl DataFileSearchTrace {
    fn new() -> Self {
        Self::default()
    }

    /// `candidate` が既存のデータファイルなら `Some` を返す。見つからなくても
    /// 調べた場所として記録する。
    fn try_candidate(&mut self, candidate: PathBuf) -> Option<PathBuf> {
        let found = is_regular_file(&candidate).then(|| candidate.clone());
        self.tried.push(candidate);
        found
    }

    /// 見つからなかった場合のエラーを、調べた場所を全て列挙して組み立てる。
    fn not_found_error(&self, file_name: &str) -> anyhow::Error {
        let tried_list = self
            .tried
            .iter()
            .map(|p| format!("  - {}", p.display()))
            .collect::<Vec<_>>()
            .join("\n");
        anyhow!(
            "既定の JL コマンドファイル `{file_name}` が見つかりませんでした。以下の場所を探しました:\n{tried_list}\n\
             環境変数 `TACHIKAZE_JL_DIR`、または `--jl-file` オプションで直接指定してください。"
        )
    }
}

/// 優先順位付きのディレクトリ列（`None` はスキップ）→ `PATH` の順に `name` を探す。
///
/// 探索順序そのものを表す共通ロジック。[`resolve_tool`] から呼ばれるほか、
/// 単体テストでも優先順位の検証に使う。
fn resolve_from_dirs(dirs: &[Option<PathBuf>], name: &str) -> Result<PathBuf> {
    let mut trace = SearchTrace::new();

    for dir in dirs {
        if let Some(found) = trace.try_dir(dir.as_deref(), name) {
            return Ok(found);
        }
    }

    if let Some(found) = trace.try_path_env(name) {
        return Ok(found);
    }

    Err(trace.not_found_error(name))
}

/// 外部ツールを探索順序に従って解決する。
///
/// - `tool_dir`: `--tool-dir` CLI オプションの値（最優先）。
/// - `name`: 実行ファイル名（[`CHAPTER_EXE`] などの定数を渡す）。
///
/// 見つかったパスは絶対パスに正規化して返す。`external::run` が作業
/// ディレクトリへ `current_dir` するため、`--tool-dir tools` のような相対
/// 指定のままだと `work/tools/...` を探しにいって起動に失敗する。
///
/// 見つからない場合は、調べた場所を全て列挙したエラーを返す。`ffprobe` の
/// ように必須ではないツールは、呼び出し側で `Result` を見て `.ok()` などに
/// 変換し、警告に留めるかどうかを判断する。
pub fn resolve_tool(tool_dir: Option<&Path>, name: &str) -> Result<PathBuf> {
    let env_dir = env::var_os("TACHIKAZE_TOOL_DIR").map(PathBuf::from);
    let exe_dir = env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));

    let found = resolve_from_dirs(&[tool_dir.map(Path::to_path_buf), env_dir, exe_dir], name)?;
    std::fs::canonicalize(&found).with_context(|| {
        format!(
            "外部ツール `{name}` の絶対パス解決に失敗しました: {}",
            found.display()
        )
    })
}

/// 既定の JL コマンドファイル（`JL_標準.txt`）を探索順序に従って探す。
///
/// 局別のルールファイル選択は現状不要（`docs/jls-settings.md` 参照）なので、
/// 既定ファイル固定で十分。`--jl-file` が指定された場合はこの関数を呼ばず、
/// 呼び出し側がそのパスをそのまま使う。
///
/// 探索順序（最初に見つかったものを使う）:
///
/// 1. `$TACHIKAZE_JL_DIR`（JL ファイルが入っているディレクトリを直接指定）
/// 2. `${XDG_DATA_HOME:-$HOME/.local/share}/tachikaze/JL/`
/// 3. `$XDG_DATA_DIRS`（既定 `/usr/local/share:/usr/share`）の各要素 +
///    `join_logo_scp/JL/`
/// 4. `<join_logo_scp の実体パス>/../share/join_logo_scp/JL/`（bindir の隣の
///    share を推定するリロケータブルな段。`join_logo_scp_path` は
///    [`resolve_tool`] が返す canonicalize 済みのパスを渡すこと。symlink の
///    まま親を辿ると壊れる）
/// 5. `<join_logo_scp と同じディレクトリ>/JL/`（現在の 1 ディレクトリ配布との
///    互換のため最後に残す段）
///
/// いずれの場所でも見つからない場合は、探した場所を全て列挙したエラーを返す。
pub fn default_jl_command_file(join_logo_scp_path: &Path) -> Result<PathBuf> {
    let mut trace = DataFileSearchTrace::new();

    if let Some(dir) = non_empty_env("TACHIKAZE_JL_DIR") {
        let candidate = PathBuf::from(dir).join(DEFAULT_JL_COMMAND_FILE);
        if let Some(found) = trace.try_candidate(candidate) {
            return Ok(found);
        }
    }

    if let Some(data_home) = xdg_data_home() {
        let candidate = data_home
            .join("tachikaze")
            .join("JL")
            .join(DEFAULT_JL_COMMAND_FILE);
        if let Some(found) = trace.try_candidate(candidate) {
            return Ok(found);
        }
    }

    for data_dir in xdg_data_dirs() {
        let candidate = data_dir
            .join("join_logo_scp")
            .join("JL")
            .join(DEFAULT_JL_COMMAND_FILE);
        if let Some(found) = trace.try_candidate(candidate) {
            return Ok(found);
        }
    }

    if let Some(bin_dir) = join_logo_scp_path.parent() {
        if let Some(prefix_dir) = bin_dir.parent() {
            let candidate = prefix_dir
                .join("share")
                .join("join_logo_scp")
                .join("JL")
                .join(DEFAULT_JL_COMMAND_FILE);
            if let Some(found) = trace.try_candidate(candidate) {
                return Ok(found);
            }
        }
    }

    if let Some(dir) = join_logo_scp_path.parent() {
        let candidate = dir.join("JL").join(DEFAULT_JL_COMMAND_FILE);
        if let Some(found) = trace.try_candidate(candidate) {
            return Ok(found);
        }
    }

    Err(trace.not_found_error(DEFAULT_JL_COMMAND_FILE))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::Mutex;

    /// `TACHIKAZE_TOOL_DIR` / `PATH` の書き換えを伴うテストを直列化するための
    /// ロック。`cargo test` はデフォルトでテストを並行実行するため、プロセス
    /// 全体で共有される環境変数を書き換えるテストは互いに競合してしまう。
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 複数の環境変数を差し替え、Drop で元の値に戻すガード（`ENV_LOCK` と併用する）。
    struct EnvVarGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl EnvVarGuard {
        fn new(keys: &[&'static str]) -> Self {
            let saved = keys.iter().map(|&k| (k, env::var_os(k))).collect();
            Self { saved }
        }

        fn set(&self, key: &str, value: impl AsRef<std::ffi::OsStr>) {
            env::set_var(key, value);
        }

        fn remove(&self, key: &str) {
            env::remove_var(key);
        }
    }

    impl Drop for EnvVarGuard {
        fn drop(&mut self) {
            for (k, v) in &self.saved {
                match v {
                    Some(v) => env::set_var(k, v),
                    None => env::remove_var(k),
                }
            }
        }
    }

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
    fn resolve_from_dirs_prefers_earlier_entries_over_later_ones() {
        // 4段のうち先頭3段（tool_dir 相当 / env 相当 / exe_dir 相当）を模して
        // 優先順位を検証する。同名の実行ファイルを全ての段に置き、常に先頭
        // (tool_dir 相当) が選ばれることを確認する。
        let tool_dir = TempDir::new("priority-1-tool-dir");
        let env_dir = TempDir::new("priority-2-env-dir");
        let exe_dir = TempDir::new("priority-3-exe-dir");

        let expected = tool_dir.make_executable("chapter_exe");
        env_dir.make_executable("chapter_exe");
        exe_dir.make_executable("chapter_exe");

        let dirs = vec![
            Some(tool_dir.path().to_path_buf()),
            Some(env_dir.path().to_path_buf()),
            Some(exe_dir.path().to_path_buf()),
        ];
        let found = resolve_from_dirs(&dirs, "chapter_exe").expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn resolve_from_dirs_falls_through_to_later_entries_when_earlier_missing() {
        let tool_dir = TempDir::new("fallthrough-1-tool-dir");
        let env_dir = TempDir::new("fallthrough-2-env-dir");
        let exe_dir = TempDir::new("fallthrough-3-exe-dir");

        // tool_dir には置かない。env_dir にだけ置く。
        let expected = env_dir.make_executable("join_logo_scp");
        exe_dir.make_executable("join_logo_scp");

        let dirs = vec![
            Some(tool_dir.path().to_path_buf()),
            Some(env_dir.path().to_path_buf()),
            Some(exe_dir.path().to_path_buf()),
        ];
        let found = resolve_from_dirs(&dirs, "join_logo_scp").expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn resolve_from_dirs_falls_back_to_path_when_no_dir_matches() {
        let _guard = ENV_LOCK.lock().unwrap();

        let tool_dir = TempDir::new("path-fallback-tool-dir");
        let path_dir = TempDir::new("path-fallback-path-dir");
        let expected = path_dir.make_executable("dtvindex");

        let original_path = env::var_os("PATH");
        env::set_var("PATH", path_dir.path());

        let dirs = vec![Some(tool_dir.path().to_path_buf()), None];
        let result = resolve_from_dirs(&dirs, "dtvindex");

        match original_path {
            Some(p) => env::set_var("PATH", p),
            None => env::remove_var("PATH"),
        }

        let found = result.expect("should resolve via PATH");
        assert_eq!(found, expected);
    }

    #[test]
    fn resolve_from_dirs_reports_all_searched_locations_when_missing() {
        let _guard = ENV_LOCK.lock().unwrap();

        let tool_dir = TempDir::new("missing-tool-dir");
        let env_dir = TempDir::new("missing-env-dir");

        let original_path = env::var_os("PATH");
        // PATH は空にして、tool_dir / env_dir だけを候補にする。
        env::set_var("PATH", "");

        let dirs = vec![
            Some(tool_dir.path().to_path_buf()),
            Some(env_dir.path().to_path_buf()),
        ];
        let result = resolve_from_dirs(&dirs, "no-such-tool-xyz");

        match original_path {
            Some(p) => env::set_var("PATH", p),
            None => env::remove_var("PATH"),
        }

        let err = result.expect_err("should fail when nowhere has the tool");
        let message = err.to_string();
        assert!(message.contains("no-such-tool-xyz"));
        assert!(message.contains(
            &tool_dir
                .path()
                .join("no-such-tool-xyz")
                .display()
                .to_string()
        ));
        assert!(message.contains(
            &env_dir
                .path()
                .join("no-such-tool-xyz")
                .display()
                .to_string()
        ));
    }

    #[test]
    fn resolve_tool_prefers_explicit_tool_dir_over_env_var() {
        let _guard = ENV_LOCK.lock().unwrap();

        let explicit_dir = TempDir::new("resolve-tool-explicit");
        let env_dir = TempDir::new("resolve-tool-env");

        let expected = explicit_dir.make_executable(CHAPTER_EXE);
        env_dir.make_executable(CHAPTER_EXE);

        let original_env = env::var_os("TACHIKAZE_TOOL_DIR");
        env::set_var("TACHIKAZE_TOOL_DIR", env_dir.path());

        let result = resolve_tool(Some(explicit_dir.path()), CHAPTER_EXE);

        match original_env {
            Some(v) => env::set_var("TACHIKAZE_TOOL_DIR", v),
            None => env::remove_var("TACHIKAZE_TOOL_DIR"),
        }

        // canonicalize 後のパスと比較する（macOS では /var → /private/var）。
        let expected = fs::canonicalize(&expected).expect("canonicalize expected");
        assert_eq!(result.expect("should resolve"), expected);
    }

    #[test]
    fn resolve_tool_uses_env_var_when_no_explicit_tool_dir() {
        let _guard = ENV_LOCK.lock().unwrap();

        let env_dir = TempDir::new("resolve-tool-env-only");
        let expected = env_dir.make_executable(JOIN_LOGO_SCP);

        let original_env = env::var_os("TACHIKAZE_TOOL_DIR");
        env::set_var("TACHIKAZE_TOOL_DIR", env_dir.path());

        let result = resolve_tool(None, JOIN_LOGO_SCP);

        match original_env {
            Some(v) => env::set_var("TACHIKAZE_TOOL_DIR", v),
            None => env::remove_var("TACHIKAZE_TOOL_DIR"),
        }

        let expected = fs::canonicalize(&expected).expect("canonicalize expected");
        assert_eq!(result.expect("should resolve"), expected);
    }

    #[test]
    fn resolve_tool_finds_binary_next_to_own_executable() {
        let _guard = ENV_LOCK.lock().unwrap();

        // レベル3（自分の実行ファイルと同じディレクトリ）を実際の
        // `current_exe()` の親ディレクトリで検証する。テストバイナリ自身の
        // ディレクトリに一意な名前のダミーファイルを作り、後片付けする。
        let exe_dir = env::current_exe()
            .expect("current_exe")
            .parent()
            .expect("parent dir")
            .to_path_buf();

        let unique_name = format!(
            "tachikaze-test-dtvindex-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        );
        let dummy_path = exe_dir.join(&unique_name);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&dummy_path, "#!/bin/sh\nexit 0\n").expect("write dummy tool");
            let mut perms = fs::metadata(&dummy_path).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&dummy_path, perms).expect("chmod dummy tool");
        }
        #[cfg(not(unix))]
        {
            fs::write(&dummy_path, "dummy").expect("write dummy tool");
        }

        let original_env = env::var_os("TACHIKAZE_TOOL_DIR");
        env::remove_var("TACHIKAZE_TOOL_DIR");

        let result = resolve_tool(None, &unique_name);
        let expected = fs::canonicalize(&dummy_path).expect("canonicalize dummy");

        match original_env {
            Some(v) => env::set_var("TACHIKAZE_TOOL_DIR", v),
            None => env::remove_var("TACHIKAZE_TOOL_DIR"),
        }
        let _ = fs::remove_file(&dummy_path);

        assert_eq!(result.expect("should resolve via exe dir"), expected);
    }

    /// JL 探索が触る環境変数一式。テストごとに `EnvVarGuard` で保存・復元する。
    const JL_ENV_KEYS: &[&str] = &["TACHIKAZE_JL_DIR", "XDG_DATA_HOME", "XDG_DATA_DIRS"];

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
    fn default_jl_command_file_prefers_tachikaze_jl_dir_over_all_other_stages() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let stage1 = TempDir::new("jl-priority-stage1");
        let stage2_data_home = TempDir::new("jl-priority-stage2");
        let stage3_data_dirs = TempDir::new("jl-priority-stage3");
        let prefix = TempDir::new("jl-priority-prefix");

        // 段 1: TACHIKAZE_JL_DIR は JL ファイルが入っているディレクトリを直接指す。
        let expected = write_jl_file(stage1.path());
        // 他の段にも同名ファイルを置き、それらが選ばれていないことを保証する。
        write_jl_file(&stage2_data_home.path().join("tachikaze").join("JL"));
        write_jl_file(&stage3_data_dirs.path().join("join_logo_scp").join("JL"));
        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);
        write_jl_file(&prefix.path().join("share").join("join_logo_scp").join("JL"));
        write_jl_file(&bin_dir.join("JL"));

        env.set("TACHIKAZE_JL_DIR", stage1.path());
        env.set("XDG_DATA_HOME", stage2_data_home.path());
        env.set(
            "XDG_DATA_DIRS",
            env::join_paths([stage3_data_dirs.path()]).unwrap(),
        );

        let found = default_jl_command_file(&join_logo_scp_path).expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn default_jl_command_file_uses_xdg_data_home_when_no_env_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let data_home = TempDir::new("jl-data-home");
        let data_dirs = TempDir::new("jl-data-home-other-dirs");
        let prefix = TempDir::new("jl-data-home-prefix");

        let expected = write_jl_file(&data_home.path().join("tachikaze").join("JL"));
        write_jl_file(&data_dirs.path().join("join_logo_scp").join("JL"));
        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);
        write_jl_file(&bin_dir.join("JL"));

        env.remove("TACHIKAZE_JL_DIR");
        env.set("XDG_DATA_HOME", data_home.path());
        env.set(
            "XDG_DATA_DIRS",
            env::join_paths([data_dirs.path()]).unwrap(),
        );

        let found = default_jl_command_file(&join_logo_scp_path).expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn default_jl_command_file_uses_xdg_data_dirs_when_no_data_home_match() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let empty_data_home = TempDir::new("jl-data-dirs-empty-home");
        let data_dirs_1 = TempDir::new("jl-data-dirs-1");
        let data_dirs_2 = TempDir::new("jl-data-dirs-2");
        let prefix = TempDir::new("jl-data-dirs-prefix");

        // data_dirs_1 には置かず、2番目の要素 data_dirs_2 にだけ置く。
        let expected = write_jl_file(&data_dirs_2.path().join("join_logo_scp").join("JL"));
        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);
        write_jl_file(&bin_dir.join("JL"));

        env.remove("TACHIKAZE_JL_DIR");
        env.set("XDG_DATA_HOME", empty_data_home.path());
        env.set(
            "XDG_DATA_DIRS",
            env::join_paths([data_dirs_1.path(), data_dirs_2.path()]).unwrap(),
        );

        let found = default_jl_command_file(&join_logo_scp_path).expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn default_jl_command_file_uses_prefix_relative_share_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let empty_data_home = TempDir::new("jl-prefix-empty-home");
        let empty_data_dirs = TempDir::new("jl-prefix-empty-dirs");
        let prefix = TempDir::new("jl-prefix-share");

        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);
        // 段 5（bin と同じディレクトリの JL/）には置かず、段 4（prefix/share/...）にだけ置く。
        let expected = write_jl_file(&prefix.path().join("share").join("join_logo_scp").join("JL"));

        env.remove("TACHIKAZE_JL_DIR");
        env.set("XDG_DATA_HOME", empty_data_home.path());
        env.set(
            "XDG_DATA_DIRS",
            env::join_paths([empty_data_dirs.path()]).unwrap(),
        );

        let found = default_jl_command_file(&join_logo_scp_path).expect("should resolve");
        assert_eq!(found, expected);
    }

    #[test]
    fn default_jl_command_file_falls_back_to_join_logo_scp_sibling_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let empty_data_home = TempDir::new("jl-sibling-empty-home");
        let empty_data_dirs = TempDir::new("jl-sibling-empty-dirs");
        let dir = TempDir::new("jl-command-file-found");

        // 現在の1ディレクトリ配布と同じ構成: join_logo_scp と同じディレクトリの JL/。
        let expected = write_jl_file(&dir.path().join("JL"));
        let join_logo_scp_path = make_join_logo_scp(dir.path());

        env.remove("TACHIKAZE_JL_DIR");
        env.set("XDG_DATA_HOME", empty_data_home.path());
        env.set(
            "XDG_DATA_DIRS",
            env::join_paths([empty_data_dirs.path()]).unwrap(),
        );

        let found = default_jl_command_file(&join_logo_scp_path).expect("should find JL file");
        assert_eq!(found, expected);
    }

    #[test]
    fn default_jl_command_file_missing_is_reported_clearly() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let jl_dir = TempDir::new("jl-missing-env-dir");
        let data_home = TempDir::new("jl-missing-data-home");
        let data_dirs = TempDir::new("jl-missing-data-dirs");
        let prefix = TempDir::new("jl-missing-prefix");

        let bin_dir = prefix.path().join("bin");
        let join_logo_scp_path = make_join_logo_scp(&bin_dir);

        env.set("TACHIKAZE_JL_DIR", jl_dir.path());
        env.set("XDG_DATA_HOME", data_home.path());
        env.set(
            "XDG_DATA_DIRS",
            env::join_paths([data_dirs.path()]).unwrap(),
        );

        let err = default_jl_command_file(&join_logo_scp_path)
            .expect_err("nowhere has the JL file, should fail");
        let message = err.to_string();

        assert!(message.contains(
            &jl_dir
                .path()
                .join(DEFAULT_JL_COMMAND_FILE)
                .display()
                .to_string()
        ));
        assert!(message.contains(
            &data_home
                .path()
                .join("tachikaze")
                .join("JL")
                .join(DEFAULT_JL_COMMAND_FILE)
                .display()
                .to_string()
        ));
        assert!(message.contains(
            &data_dirs
                .path()
                .join("join_logo_scp")
                .join("JL")
                .join(DEFAULT_JL_COMMAND_FILE)
                .display()
                .to_string()
        ));
        assert!(message.contains(
            &prefix
                .path()
                .join("share")
                .join("join_logo_scp")
                .join("JL")
                .join(DEFAULT_JL_COMMAND_FILE)
                .display()
                .to_string()
        ));
        assert!(message.contains(
            &bin_dir
                .join("JL")
                .join(DEFAULT_JL_COMMAND_FILE)
                .display()
                .to_string()
        ));
    }

    #[test]
    fn default_jl_command_file_treats_empty_env_vars_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let env = EnvVarGuard::new(JL_ENV_KEYS);

        let dir = TempDir::new("jl-empty-env-fallback");
        let expected = write_jl_file(&dir.path().join("JL"));
        let join_logo_scp_path = make_join_logo_scp(dir.path());

        // 空文字は未設定として扱われ、既定（$HOME 相当）へフォールバックしたうえで
        // 最終的に段 5（sibling JL/）まで落ちてくるはず。
        env.set("TACHIKAZE_JL_DIR", "");
        env.set("XDG_DATA_HOME", "");
        env.set("XDG_DATA_DIRS", "");

        let found = default_jl_command_file(&join_logo_scp_path).expect("should resolve");
        assert_eq!(found, expected);
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
