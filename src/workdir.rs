//! 作業ディレクトリの用意と、入力ファイルへの symlink 戦略。
//!
//! `chapter_exe` はメディアファイルの隣に `<media>.dtvi` を自動生成する。入力を
//! 直接渡すと録画フォルダに中間ファイルが散るため、作業ディレクトリに入力への
//! symlink（`work.mp4`）を張り、そちらを外部ツールに渡す。symlink なので
//! 800 MB 級のファイルでもコピーは発生しない。
//!
//! 中間ファイルの名前（`work.mp4` / `work.mp4.dtvi` / `scp.txt` / `trim.avs` /
//! `detail.jls` / `work.mp4.segmap.json`）はこのモジュールに集約し、他のモジュールは
//! `WorkDir` のアクセサ、または `cut` 専用の [`cached_segment_map_path`]（`.dtvi` の
//! [`cached_dtvi_path`] と同じ理由。`cut` は `WorkDir` を作らないため）経由でのみ
//! パスを得る。
//!
//! 中間ファイル（`.dtvi` / `trim.avs` / `detail.jls`）はいずれも `analyze` を
//! 再実行すれば作り直せる**キャッシュ**であり、XDG のキャッシュディレクトリの
//! 定義（消えても再生成できるデータ）と一致する。既定では入力ファイルごとに
//! 決まるキャッシュディレクトリを使い、削除しないことで `cut --dtvi` へ
//! そのまま繋げられるようにしている（`--no-keep-work` で従来の使い捨て
//! 一時ディレクトリに戻せる）。

use std::env;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

const WORK_FILE_NAME: &str = "work.mp4";
const DTVI_FILE_NAME: &str = "work.mp4.dtvi";
const SCP_FILE_NAME: &str = "scp.txt";
const TRIM_FILE_NAME: &str = "trim.avs";
const DETAIL_JLS_FILE_NAME: &str = "detail.jls";
const SEGMENT_MAP_FILE_NAME: &str = "work.mp4.segmap.json";
/// `prepare` が作る、elst 除去・字幕トラック除去後のメディアのファイル名。
const INPUT_PREPARED_FILE_NAME: &str = "input_prepared.mp4";
/// `prepare` が作る字幕サイドカーのベース名。拡張子（`ass` / `srt`）は
/// 抽出元コーデックにより `prepare` 側が決めるため、ここでは持たない。
const SUBS_BASE_NAME: &str = "subs";

/// キャッシュディレクトリ名に使う stem の長さ上限（文字数）。
const SAFE_STEM_MAX_CHARS: usize = 80;

/// analyze / cut の中間ファイルを置く作業ディレクトリ。
///
/// - `--work-dir` で明示された場合: そのディレクトリを使い、処理後も削除しない
///   （中間ファイルを見たい場合があるため）。
/// - 未指定・既定の場合: 入力ファイルごとに決まる XDG キャッシュディレクトリを
///   使い、処理後も削除しない。同じ入力を再度 `analyze` すると同じディレクトリ
///   を再利用し、中間ファイルは上書きされる（`dtvindex` / `chapter_exe` /
///   `join_logo_scp` はいずれも既存の出力先へ実害なく上書きすることを実機で
///   確認済み）。
/// - `--no-keep-work` 指定時: 従来どおり一時ディレクトリを作り、成功時のみ
///   削除する。
///
/// いずれの場合も失敗時は原因調査のため中間ファイルを残す。
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    /// `true` なら `finish` で削除しない（`--work-dir` 指定時、または既定の
    /// キャッシュディレクトリ使用時）。
    keep: bool,
}

impl WorkDir {
    /// 作業ディレクトリを用意する。
    ///
    /// - `explicit` が `Some` の場合: そのディレクトリを使う（無ければ作る）。
    ///   `finish` では削除しない。`input` / `no_keep_work` は無視する。
    /// - `explicit` が `None` の場合:
    ///   - `no_keep_work == true`: OS の一時ディレクトリ配下にユニークな
    ///     ディレクトリを新規作成する。`finish(true)` で削除される
    ///     （`--no-keep-work` 指定時の従来どおりの挙動）。
    ///   - `no_keep_work == false`（既定）: `input` の絶対パスから決まる
    ///     XDG キャッシュディレクトリを使う（無ければ作る）。`finish` では
    ///     削除しない。
    pub fn new(explicit: Option<PathBuf>, input: &Path, no_keep_work: bool) -> Result<Self> {
        match explicit {
            Some(path) => {
                fs::create_dir_all(&path).with_context(|| {
                    format!("作業ディレクトリの作成に失敗しました: {}", path.display())
                })?;
                // 相対パスのまま保持すると、`external::run` が `current_dir` を
                // このディレクトリに切り替えたあと、引数の `work/work.mp4` などが
                // 二重にネストして解決される。作成直後に絶対化しておく。
                let path = fs::canonicalize(&path).with_context(|| {
                    format!(
                        "作業ディレクトリの絶対パス解決に失敗しました: {}",
                        path.display()
                    )
                })?;
                Ok(Self { path, keep: true })
            }
            None if no_keep_work => {
                let path = create_unique_temp_dir()?;
                Ok(Self { path, keep: false })
            }
            None => {
                let path = cache_dir_for_input(input)?;
                fs::create_dir_all(&path).with_context(|| {
                    format!(
                        "キャッシュディレクトリの作成に失敗しました: {}",
                        path.display()
                    )
                })?;
                let path = fs::canonicalize(&path).with_context(|| {
                    format!(
                        "キャッシュディレクトリの絶対パス解決に失敗しました: {}",
                        path.display()
                    )
                })?;
                Ok(Self { path, keep: true })
            }
        }
    }

    /// 作業ディレクトリのパス。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 入力ファイルへの symlink を作業ディレクトリに張り、`work.mp4` のパスを返す。
    ///
    /// 入力の絶対パスを解決してから symlink するため、入力が相対パスでも
    /// カレントディレクトリが変わっても壊れない。入力自体が symlink でも、
    /// その解決先へ張るので問題なく動く。既に `work.mp4` がある場合は張り替える。
    pub fn link_input(&self, input: &Path) -> Result<PathBuf> {
        let absolute_input = fs::canonicalize(input).with_context(|| {
            format!(
                "入力ファイルの絶対パス解決に失敗しました: {}",
                input.display()
            )
        })?;

        let work_path = self.work_path();

        // 既存の work.mp4（前回実行の symlink 等）があれば張り替える。
        match fs::symlink_metadata(&work_path) {
            Ok(_) => {
                fs::remove_file(&work_path).with_context(|| {
                    format!(
                        "既存の {} の削除に失敗しました: {}",
                        WORK_FILE_NAME,
                        work_path.display()
                    )
                })?;
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "{} の状態確認に失敗しました: {}",
                        WORK_FILE_NAME,
                        work_path.display()
                    )
                })
            }
        }

        symlink(&absolute_input, &work_path).with_context(|| {
            format!(
                "symlink の作成に失敗しました: {} -> {}",
                work_path.display(),
                absolute_input.display()
            )
        })?;

        Ok(work_path)
    }

    /// `work.mp4` のパス（`chapter_exe` 等に渡す入力）。
    pub fn work_path(&self) -> PathBuf {
        self.path.join(WORK_FILE_NAME)
    }

    /// `work.mp4.dtvi` のパス（`chapter_exe` が自動生成する）。
    pub fn dtvi_path(&self) -> PathBuf {
        self.path.join(DTVI_FILE_NAME)
    }

    /// `scp.txt` のパス（`chapter_exe -o` の出力）。
    pub fn scp_path(&self) -> PathBuf {
        self.path.join(SCP_FILE_NAME)
    }

    /// `trim.avs` のパス（`join_logo_scp -o` の出力）。
    pub fn trim_path(&self) -> PathBuf {
        self.path.join(TRIM_FILE_NAME)
    }

    /// `detail.jls` のパス（`join_logo_scp -oscp` の出力）。
    pub fn detail_jls_path(&self) -> PathBuf {
        self.path.join(DETAIL_JLS_FILE_NAME)
    }

    /// 処理完了時に呼ぶ。
    ///
    /// - `--work-dir` 指定時、または既定のキャッシュディレクトリ使用時
    ///   （`keep == true`）: ディレクトリは削除しない。成功時は `cut --dtvi`
    ///   にそのまま渡せる `.dtvi` の場所をログへ出す。
    /// - `--no-keep-work` 指定時（`keep == false`）:
    ///   - `success == true`: 一時ディレクトリを削除する。
    ///   - `success == false`: 削除せず、調査用にパスをログへ出す
    ///     （再解析は数秒だが、失敗の調査には中間ファイルが要る）。
    pub fn finish(self, success: bool) {
        if self.keep {
            if success {
                eprintln!(
                    "[workdir] 中間ファイルを残しました: {}（cut --dtvi {} で使えます）",
                    self.path.display(),
                    self.dtvi_path().display()
                );
            }
            return;
        }

        if success {
            if let Err(err) = fs::remove_dir_all(&self.path) {
                eprintln!(
                    "[workdir] 一時ディレクトリの削除に失敗しました: {} ({err})",
                    self.path.display()
                );
            }
        } else {
            eprintln!(
                "[workdir] 処理に失敗したため、調査用に一時ディレクトリを残しました: {}",
                self.path.display()
            );
        }
    }
}

/// OS の一時ディレクトリ配下にユニークな作業ディレクトリを作成し、そのパスを返す。
///
/// `tempfile` クレートに依存せず、PID + ナノ秒タイムスタンプで名前を作り、
/// 衝突時はリトライする素朴な実装（`mkdtemp` 相当）。
fn create_unique_temp_dir() -> Result<PathBuf> {
    let base = std::env::temp_dir();
    let pid = process::id();

    for attempt in 0..100 {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let candidate = base.join(format!("tachikaze-{pid}-{nanos}-{attempt}"));

        match fs::create_dir(&candidate) {
            Ok(()) => return Ok(candidate),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(err).with_context(|| {
                    format!(
                        "一時ディレクトリの作成に失敗しました: {}",
                        candidate.display()
                    )
                })
            }
        }
    }

    anyhow::bail!(
        "一時ディレクトリの作成に{}回失敗しました（{}配下）",
        100,
        base.display()
    );
}

/// 環境変数を読み、**空文字なら未設定として扱う**（XDG Base Directory 仕様の作法）。
fn non_empty_env(key: &str) -> Option<std::ffi::OsString> {
    env::var_os(key).filter(|v| !v.is_empty())
}

/// キャッシュディレクトリの根（`$TACHIKAZE_CACHE_DIR` →
/// `${XDG_CACHE_HOME:-$HOME/.cache}/tachikaze`）を返す。
///
/// `$HOME` が取れない環境（コンテナ等）でも `$TMPDIR` にフォールバックし、
/// エラーにはしない。
fn cache_root() -> PathBuf {
    if let Some(dir) = non_empty_env("TACHIKAZE_CACHE_DIR") {
        return PathBuf::from(dir);
    }
    if let Some(data_home) = non_empty_env("XDG_CACHE_HOME") {
        return PathBuf::from(data_home).join("tachikaze");
    }
    if let Some(home) = non_empty_env("HOME") {
        return PathBuf::from(home).join(".cache").join("tachikaze");
    }
    env::temp_dir().join("tachikaze-cache")
}

/// キャッシュディレクトリ名に使う stem を安全化する。
///
/// 空白・`/`・制御文字は `_` に置き換える。日本語などマルチバイト文字は
/// そのまま残す。`scripts/tachikaze-cmcut` の `safe_stem` と同じ役割だが、
/// こちらを正とする（シェル側はこの規則に合わせる）。
fn sanitize_stem(stem: &str) -> String {
    stem.chars()
        .map(|c| {
            if c.is_control() || c == '/' || c.is_whitespace() {
                '_'
            } else {
                c
            }
        })
        .take(SAFE_STEM_MAX_CHARS)
        .collect()
}

/// バイト列から短い16進ハッシュを計算する（FNV-1a、64bit）。
///
/// 依存を増やさず自前実装で十分という判断（衝突耐性が必要な用途ではなく、
/// 同じ入力パスを同じディレクトリ名に落とせればよい）。
fn fnv1a_hex(bytes: &[u8]) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for &byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

/// 入力ファイルの絶対パスから、この入力専用のキャッシュディレクトリのパスを
/// 求める。`<cache_root>/<入力絶対パスのハッシュ>-<安全化したstem>/`。
///
/// ハッシュだけでなく stem も併記するのは、万が一ハッシュが衝突しても別入力が
/// 同じディレクトリを共有しないようにするため（人間が見て区別しやすくもなる）。
fn cache_dir_for_input(input: &Path) -> Result<PathBuf> {
    let absolute = fs::canonicalize(input).with_context(|| {
        format!(
            "入力ファイルの絶対パス解決に失敗しました: {}",
            input.display()
        )
    })?;
    let hash = fnv1a_hex(absolute.as_os_str().as_bytes());
    let stem = absolute
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("input");
    let dir_name = format!("{hash}-{}", sanitize_stem(stem));
    Ok(cache_root().join(dir_name))
}

/// `cut --dtvi` 省略時に使う、入力ごとのキャッシュディレクトリ内の `.dtvi` の
/// パスを返す。`WorkDir::new` の既定（`--work-dir` 未指定時）が使うキャッシュ
/// パス規則を [`cache_dir_for_input`] 1か所に集約し、`cut` 側もそれをそのまま
/// 参照する（`analyze` が作ったディレクトリと `cut` が探すディレクトリがずれる
/// と、無関係な入力の `.dtvi` を指してしまいかねないため）。
///
/// ディレクトリの作成は行わない。ファイルが存在するかどうかの確認・存在しない
/// 場合の扱いは呼び出し側の責務とする（`.dtvi` が無いのに検証を省略してはい
/// けないため、呼び出し側で明示的に判断させる）。
pub fn cached_dtvi_path(input: &Path) -> Result<PathBuf> {
    Ok(cache_dir_for_input(input)?.join(DTVI_FILE_NAME))
}

/// `cut` が既定で書き出す区間マップ（`work.mp4.segmap.json`）のキャッシュパスを返す。
///
/// [`cached_dtvi_path`] と同じ理由で [`cache_dir_for_input`] 1か所に集約する（`cut` は
/// `.dtvi` と同じ入力ごとのキャッシュディレクトリへ区間マップを書くため、パス規則が
/// ずれると無関係な入力のマップを指しうる）。ディレクトリの作成は行わない
/// （書き込み側で必要なら作る）。
pub fn cached_segment_map_path(input: &Path) -> Result<PathBuf> {
    Ok(cache_dir_for_input(input)?.join(SEGMENT_MAP_FILE_NAME))
}

/// `prepare` が elst 除去・字幕トラック除去後のメディアを書き出すキャッシュパスを返す。
///
/// [`cached_dtvi_path`] と同じキャッシュディレクトリ規則([`cache_dir_for_input`])を
/// 共有する。`analyze` / `cut` / `prepare` がすべて同じ入力に対して同じキャッシュ
/// ディレクトリを使うことで、`cut` が `prepare` の出力を暗黙に見つけられる余地を
/// 残す(現時点では `cut` はこのパスを自動探索しない。呼び出し側が明示的に
/// `prepare` の出力パスを `cut` の入力として渡す)。
///
/// ディレクトリの作成は行わない([`cached_dtvi_path`]と同様、呼び出し側の責務)。
pub fn prepared_input_path(input: &Path) -> Result<PathBuf> {
    Ok(cache_dir_for_input(input)?.join(INPUT_PREPARED_FILE_NAME))
}

/// `prepare` が字幕サイドカーを書き出すキャッシュパスを返す。
///
/// `extension` には `"ass"` / `"srt"` など、`.` を含まない拡張子を渡す
/// (どちらを使うかは字幕トラックのコーデックから `prepare` が決める。
/// `prepare::SubtitleFormat` 参照)。ディレクトリの作成は行わない。
pub fn subs_path(input: &Path, extension: &str) -> Result<PathBuf> {
    Ok(cache_dir_for_input(input)?.join(format!("{SUBS_BASE_NAME}.{extension}")))
}

/// `TACHIKAZE_CACHE_DIR` を書き換えるテストで共有する仕組み。
///
/// `workdir::tests` と `commands::tests`（`cut --dtvi` のキャッシュ自動解決）
/// の両方が同じ環境変数を書き換える。モジュールごとに別々の `Mutex` を持つと
/// 互いの書き換えを直列化できずレースする（実際に
/// `fs::remove_dir_all` が「ディレクトリが空でない」で失敗する形で顕在化した）
/// ため、1つのロックをここに集約して両モジュールから使う。
#[cfg(test)]
pub(crate) mod test_support {
    use std::env;
    use std::sync::Mutex;

    /// `TACHIKAZE_CACHE_DIR` の書き換えを伴うテストを直列化するためのロック
    /// （`cargo test` はテストを並行実行するため）。
    pub(crate) static ENV_LOCK: Mutex<()> = Mutex::new(());

    /// 環境変数を書き換え、Drop で元の値に戻すガード（`ENV_LOCK` と併用する）。
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

    /// テスト用に、システムの一時ディレクトリ配下にユニークなディレクトリを作る。
    /// `WorkDir` 自体のテストなので `tempfile` クレートには頼らず、
    /// `create_unique_temp_dir` と同じ素朴な方式で自前実装する。
    fn make_scratch_dir(label: &str) -> PathBuf {
        let base = std::env::temp_dir();
        let pid = process::id();
        for attempt in 0..100 {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let candidate = base.join(format!("tachikaze-test-{label}-{pid}-{nanos}-{attempt}"));
            if fs::create_dir(&candidate).is_ok() {
                return candidate;
            }
        }
        panic!("scratch dir の作成に失敗しました");
    }

    fn dir_entries(dir: &Path) -> Vec<PathBuf> {
        let mut entries: Vec<PathBuf> = fs::read_dir(dir)
            .expect("read_dir")
            .map(|e| e.expect("dir entry").path())
            .collect();
        entries.sort();
        entries
    }

    /// 入力ファイルのあるディレクトリに、analyze 相当の処理（作業ディレクトリ作成 →
    /// symlink 張り → finish）を通しても新規ファイルが作られないことを確認する。
    /// `--work-dir` 未指定・既定（キャッシュディレクトリ使用）での検証。
    #[test]
    fn does_not_create_files_next_to_input() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-no-pollution");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("input-dir");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let before = dir_entries(&input_dir);

        let work_dir = WorkDir::new(None, &input_path, false).expect("create work dir");
        work_dir.link_input(&input_path).expect("link input");
        work_dir.finish(true);

        let after = dir_entries(&input_dir);
        assert_eq!(
            before, after,
            "入力ディレクトリの内容が処理前後で変わってはいけない"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// `--work-dir` 指定時は成功しても中間ファイル（symlink 等）が残る。
    #[test]
    fn explicit_work_dir_is_kept_after_success() {
        let input_dir = make_scratch_dir("explicit-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let explicit_dir = make_scratch_dir("explicit-workdir");

        let work_dir =
            WorkDir::new(Some(explicit_dir.clone()), &input_path, false).expect("create work dir");
        let work_mp4 = work_dir.link_input(&input_path).expect("link input");
        assert!(work_mp4.exists(), "symlink 先が存在するはず");
        work_dir.finish(true);

        assert!(
            explicit_dir.exists(),
            "--work-dir 指定時は成功してもディレクトリが残るはず"
        );
        assert!(
            work_mp4.exists(),
            "--work-dir 指定時は成功しても work.mp4 が残るはず"
        );

        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
        fs::remove_dir_all(&explicit_dir).expect("cleanup explicit dir");
    }

    /// 既定（`--work-dir` も `--no-keep-work` も未指定）では、入力ごとのキャッシュ
    /// ディレクトリが使われ、成功しても削除されない。
    #[test]
    fn default_cache_dir_is_kept_after_success() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-kept");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("cache-kept-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work_dir = WorkDir::new(None, &input_path, false).expect("create work dir");
        let path = work_dir.path().to_path_buf();
        // `path` は WorkDir::new 内で canonicalize 済み（macOS では
        // /var → /private/var）なので、比較対象の cache_root も canonicalize する。
        let cache_root_canon = fs::canonicalize(&cache_root).expect("canonicalize cache root");
        assert!(
            path.starts_with(&cache_root_canon),
            "既定のキャッシュディレクトリは TACHIKAZE_CACHE_DIR 配下のはず"
        );
        work_dir.link_input(&input_path).expect("link input");
        work_dir.finish(true);

        assert!(
            path.exists(),
            "既定のキャッシュディレクトリは成功しても削除されないはず"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// 同じ入力に対して2回 `WorkDir::new` すると、同じキャッシュディレクトリを再利用する。
    #[test]
    fn default_cache_dir_is_reused_for_same_input() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-reuse");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("cache-reuse-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let first = WorkDir::new(None, &input_path, false).expect("create work dir (1st)");
        let first_path = first.path().to_path_buf();
        first.finish(true);

        let second = WorkDir::new(None, &input_path, false).expect("create work dir (2nd)");
        let second_path = second.path().to_path_buf();
        second.finish(true);

        assert_eq!(
            first_path, second_path,
            "同じ入力なら同じキャッシュディレクトリを再利用するはず"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// 異なる入力に対しては異なるキャッシュディレクトリが割り当てられる。
    #[test]
    fn default_cache_dir_differs_for_different_inputs() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-differ");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("cache-differ-input");
        let first_input = input_dir.join("FIRST.mp4");
        let second_input = input_dir.join("SECOND.mp4");
        fs::write(&first_input, b"first").expect("write first input");
        fs::write(&second_input, b"second").expect("write second input");

        let first = WorkDir::new(None, &first_input, false).expect("create work dir (1st)");
        let first_path = first.path().to_path_buf();
        first.finish(true);

        let second = WorkDir::new(None, &second_input, false).expect("create work dir (2nd)");
        let second_path = second.path().to_path_buf();
        second.finish(true);

        assert_ne!(
            first_path, second_path,
            "異なる入力なら異なるキャッシュディレクトリのはず"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// `--no-keep-work` 指定時、成功すると従来どおり一時ディレクトリが消える。
    #[test]
    fn no_keep_work_removes_temp_dir_after_success() {
        let input_dir = make_scratch_dir("no-keep-work-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work_dir = WorkDir::new(None, &input_path, true).expect("create work dir");
        let path = work_dir.path().to_path_buf();
        work_dir.link_input(&input_path).expect("link input");
        work_dir.finish(true);

        assert!(!path.exists(), "成功時は一時ディレクトリが削除されるはず");

        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
    }

    /// `--no-keep-work` 指定時、失敗すると一時ディレクトリは残る（調査用）。
    #[test]
    fn no_keep_work_keeps_temp_dir_after_failure() {
        let input_dir = make_scratch_dir("no-keep-work-fail-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work_dir = WorkDir::new(None, &input_path, true).expect("create work dir");
        let path = work_dir.path().to_path_buf();
        work_dir.link_input(&input_path).expect("link input");
        work_dir.finish(false);

        assert!(path.exists(), "失敗時は一時ディレクトリが残るはず");

        fs::remove_dir_all(&path).expect("cleanup work dir");
        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
    }

    /// 入力が既に symlink だった場合も、その解決先へ張り替えて動く。
    #[test]
    fn link_input_works_when_input_is_already_a_symlink() {
        let input_dir = make_scratch_dir("symlink-input");
        let real_path = input_dir.join("REAL.mp4");
        fs::write(&real_path, b"dummy mp4 content").expect("write real input");

        let symlink_input = input_dir.join("IN.mp4");
        symlink(&real_path, &symlink_input).expect("create input symlink");

        let work_dir = WorkDir::new(None, &symlink_input, true).expect("create work dir");
        let work_mp4 = work_dir.link_input(&symlink_input).expect("link input");

        let resolved = fs::canonicalize(&work_mp4).expect("resolve work.mp4");
        let expected = fs::canonicalize(&real_path).expect("resolve real path");
        assert_eq!(
            resolved, expected,
            "symlink の解決先が実ファイルと一致するはず"
        );

        work_dir.finish(true);
        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
    }

    /// 既に work.mp4 がある場合は張り替える。
    #[test]
    fn link_input_replaces_existing_work_mp4() {
        let input_dir = make_scratch_dir("relink-input");
        let first_input = input_dir.join("FIRST.mp4");
        let second_input = input_dir.join("SECOND.mp4");
        fs::write(&first_input, b"first").expect("write first input");
        fs::write(&second_input, b"second").expect("write second input");

        let work_dir = WorkDir::new(None, &first_input, true).expect("create work dir");
        work_dir.link_input(&first_input).expect("link first input");
        let work_mp4 = work_dir
            .link_input(&second_input)
            .expect("link second input");

        let resolved = fs::canonicalize(&work_mp4).expect("resolve work.mp4");
        let expected = fs::canonicalize(&second_input).expect("resolve second input");
        assert_eq!(resolved, expected, "張り替え後は2番目の入力を指すはず");

        work_dir.finish(true);
        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
    }

    /// 各中間ファイルパスの名前が集約されていることを確認する。
    #[test]
    fn intermediate_file_names_are_correct() {
        let work_dir =
            WorkDir::new(None, Path::new("/nonexistent-input.mp4"), true).expect("create work dir");
        assert_eq!(work_dir.work_path().file_name().unwrap(), WORK_FILE_NAME);
        assert_eq!(work_dir.dtvi_path().file_name().unwrap(), DTVI_FILE_NAME);
        assert_eq!(work_dir.scp_path().file_name().unwrap(), SCP_FILE_NAME);
        assert_eq!(work_dir.trim_path().file_name().unwrap(), TRIM_FILE_NAME);
        assert_eq!(
            work_dir.detail_jls_path().file_name().unwrap(),
            DETAIL_JLS_FILE_NAME
        );
        work_dir.finish(true);
    }

    /// `prepared_input_path` / `subs_path` が `cached_dtvi_path` と同じ
    /// キャッシュディレクトリを指すことを確認する(`analyze` / `cut` /
    /// `prepare` が同じ入力に対して同じディレクトリを共有する前提)。
    #[test]
    fn prepared_input_and_subs_paths_share_cache_dir_with_dtvi() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-prepare-paths");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("prepare-paths-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let dtvi = cached_dtvi_path(&input_path).expect("compute dtvi path");
        let prepared = prepared_input_path(&input_path).expect("compute prepared path");
        let subs_ass = subs_path(&input_path, "ass").expect("compute subs.ass path");
        let subs_srt = subs_path(&input_path, "srt").expect("compute subs.srt path");

        assert_eq!(dtvi.parent(), prepared.parent());
        assert_eq!(dtvi.parent(), subs_ass.parent());
        assert_eq!(prepared.file_name().unwrap(), INPUT_PREPARED_FILE_NAME);
        assert_eq!(subs_ass.file_name().unwrap(), "subs.ass");
        assert_eq!(subs_srt.file_name().unwrap(), "subs.srt");

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// `TACHIKAZE_CACHE_DIR` でキャッシュの根を差し替えられる。
    #[test]
    fn tachikaze_cache_dir_overrides_default_root() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-override");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("cache-override-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let dir = cache_dir_for_input(&input_path).expect("compute cache dir");
        assert!(
            dir.starts_with(&cache_root),
            "TACHIKAZE_CACHE_DIR 配下になるはず: {}",
            dir.display()
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    #[test]
    fn sanitize_stem_replaces_whitespace_slash_and_control_chars() {
        assert_eq!(sanitize_stem("hello world"), "hello_world");
        assert_eq!(sanitize_stem("a/b"), "a_b");
        assert_eq!(sanitize_stem("a\tb\nc"), "a_b_c");
        // 日本語はそのまま残る。
        assert_eq!(sanitize_stem("録画ファイル"), "録画ファイル");
    }

    #[test]
    fn sanitize_stem_truncates_to_max_chars() {
        let long_stem = "a".repeat(SAFE_STEM_MAX_CHARS + 20);
        let sanitized = sanitize_stem(&long_stem);
        assert_eq!(sanitized.chars().count(), SAFE_STEM_MAX_CHARS);
    }

    #[test]
    fn cached_segment_map_path_uses_expected_file_name_and_cache_dir() {
        let _env_guard = ENV_LOCK.lock().unwrap();
        let cache_root = make_scratch_dir("cache-root-segmap");
        let _cache_env = EnvVarGuard::set("TACHIKAZE_CACHE_DIR", &cache_root);

        let input_dir = make_scratch_dir("segmap-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let segmap_path =
            cached_segment_map_path(&input_path).expect("compute cached segment map path");
        let dtvi_path = cached_dtvi_path(&input_path).expect("compute cached dtvi path");

        assert_eq!(
            segmap_path.file_name().unwrap(),
            SEGMENT_MAP_FILE_NAME,
            "ファイル名は work.mp4.segmap.json のはず"
        );
        assert_eq!(
            segmap_path.parent(),
            dtvi_path.parent(),
            ".dtvi と同じ入力ごとのキャッシュディレクトリを指すはず"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    #[test]
    fn fnv1a_hex_is_deterministic_and_differs_for_different_input() {
        let a = fnv1a_hex(b"/path/to/IN.mp4");
        let b = fnv1a_hex(b"/path/to/IN.mp4");
        let c = fnv1a_hex(b"/path/to/OTHER.mp4");
        assert_eq!(a, b, "同じ入力なら同じハッシュのはず");
        assert_ne!(a, c, "異なる入力なら異なるハッシュのはず");
    }
}
