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
//! そのまま繋げられるようにしている。
//!
//! ## キャッシュの根の決め方（E12-2）
//!
//! かつては `TACHIKAZE_CACHE_DIR` → `XDG_CACHE_HOME` → `HOME` → `env::temp_dir()`
//! の4段の環境変数フォールバックと、CLI の `--work-dir` / `--no-keep-work` を
//! 合わせて6つの口が同じ「キャッシュの置き場所」を決めていた。どれが効いているか
//! 読んで確かめないと分からず、環境変数を書き換えるテストがプロセス共有の状態を
//! 触るため直列化用の `Mutex` が必要になっていた。
//!
//! 今は CLI のグローバルオプション `--cache-dir <DIR>`（[`crate::cli::Cli::cache_dir`]）
//! → [`cache_root`] の既定値の2段だけにしてある。`--work-dir` / `--no-keep-work` は
//! 削除した（使い捨てにしたい場合は `--cache-dir "$(mktemp -d)"` を使う）。

use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};

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
/// 入力ファイルごとに決まるキャッシュディレクトリ（[`cache_dir_for_input`]）を
/// 使う。処理後も削除しない: 同じ入力を再度 `analyze` すると同じディレクトリを
/// 再利用し、中間ファイルは上書きされる（`dtvindex` / `chapter_exe` /
/// `join_logo_scp` はいずれも既存の出力先へ実害なく上書きすることを実機で
/// 確認済み）。失敗時も原因調査のため中間ファイルを残す。
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
}

impl WorkDir {
    /// 作業ディレクトリ（入力ごとのキャッシュディレクトリ）を用意する。
    ///
    /// - `cache_dir`: `--cache-dir`（キャッシュの根）。`None` なら [`cache_root`]
    ///   の既定値を使う。
    /// - `input`: このキャッシュディレクトリを持つ入力ファイル。絶対パスの
    ///   ハッシュからディレクトリ名を決める（[`cache_dir_for_input`]）。
    pub fn new(cache_dir: Option<&Path>, input: &Path) -> Result<Self> {
        let path = cache_dir_for_input(cache_dir, input)?;
        fs::create_dir_all(&path).with_context(|| {
            format!(
                "キャッシュディレクトリの作成に失敗しました: {}",
                path.display()
            )
        })?;
        // 相対パスのまま保持すると、`external::run` が `current_dir` を
        // このディレクトリに切り替えたあと、引数の `work/work.mp4` などが
        // 二重にネストして解決される。作成直後に絶対化しておく。
        let path = fs::canonicalize(&path).with_context(|| {
            format!(
                "キャッシュディレクトリの絶対パス解決に失敗しました: {}",
                path.display()
            )
        })?;
        Ok(Self { path })
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
    /// キャッシュは既定で残すため、削除は一切行わない（`--no-keep-work` 相当の
    /// 使い捨て経路は削除済み。使い捨てにしたい場合は呼び出し側で
    /// `--cache-dir "$(mktemp -d)"` を使う）。成功時は `cut --dtvi` にそのまま
    /// 渡せる `.dtvi` の場所をログへ出すだけ。
    pub fn finish(self, success: bool) {
        if success {
            eprintln!(
                "[workdir] 中間ファイルを残しました: {}（cut --dtvi {} で使えます）",
                self.path.display(),
                self.dtvi_path().display()
            );
        }
    }
}

/// キャッシュディレクトリの根を決める。
///
/// - `explicit`（`--cache-dir`）があれば、絶対化して使う（[`absolutize_cache_dir`]）。
/// - 無ければ `std::env::home_dir()` から既定値を組み立てる
///   （[`default_cache_root`]）。
///
/// `std::env::home_dir()` の呼び出しをここに1か所だけ持つ理由は
/// [`default_cache_root`] の doc comment参照（テストが `env` を触らずに
/// エラー文言を検証できるよう、ホームを引数で受け取る純粋関数に分離した）。
fn cache_root(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return absolutize_cache_dir(dir);
    }
    default_cache_root(std::env::home_dir().as_deref())
}

/// `--cache-dir` の値を絶対パスにする。
///
/// 相対パスのまま `cache_dir_for_input` の戻り値（`<cache_dir>/<入力ハッシュ>-<stem>/`）
/// を使うと、二重解決の罠を踏む: `src/prepare.rs` は `cache_dir_for_input` の
/// 戻り値の親ディレクトリを `external::run` の作業ディレクトリ（`current_dir`）
/// として渡す一方、同じ戻り値を ffmpeg の出力先パスの**引数**としても渡す。
/// `external::run` は自分の cwd 引数だけを絶対化するため、cwd 引数は絶対化
/// されても ffmpeg への出力先引数が相対のままだと、ffmpeg はそれを新しい cwd
/// （既に `<cache_dir>/...` を含む）からの相対として解釈し、`<cache_dir>/...`
/// が二重にネストしたパスを探しに行って `No such file or directory` になる
/// （実機で `tachikaze --cache-dir relcache prepare IN.mp4` で再現した）。
/// ここで根を1か所で絶対化しておけば、そこから導出するあらゆるパス
/// （`cache_dir_for_input` の戻り値を含む）が常に絶対パスになり、この罠を
/// 踏まなくなる。
///
/// 存在しないディレクトリ（まだ作られていないキャッシュの根）も想定されるため、
/// `fs::canonicalize` は使わない（symlink 解決が要らない用途なので、
/// `env::current_dir()` との `join` で十分。`src/external.rs::absolutize_path`
/// の「存在しないパスは cwd を join するだけに留める」と同じ考え方）。
fn absolutize_cache_dir(dir: &Path) -> Result<PathBuf> {
    if dir.is_absolute() {
        return Ok(dir.to_path_buf());
    }
    let cwd = std::env::current_dir().context("カレントディレクトリの取得に失敗しました")?;
    Ok(cwd.join(dir))
}

/// `home` から既定のキャッシュルート（`<home>/.cache/tachikaze`）を組み立てる。
///
/// [`cache_root`] から `std::env::home_dir()` の呼び出しを分離した純粋関数。
/// こうすることで、「ホームディレクトリが特定できない」経路（`home: None`）を
/// 実際に `$HOME` や passwd を触らずに `None` を渡すだけでテストできる
/// （通常の実行環境では passwd にユーザーエントリが無いような状況を作らないと
/// 到達できないため、実環境でのテストが書けない）。
///
/// ## ディレクトリ名は XDG から借りるが、環境変数は読まない
///
/// `.cache/tachikaze` という名前自体は XDG Base Directory 仕様の既定
/// （`${XDG_CACHE_HOME:-~/.cache}`）と同じものを借りている（利用者にとって
/// 見慣れた場所にするため）が、`XDG_CACHE_HOME` / `TACHIKAZE_CACHE_DIR` と
/// いった環境変数は一切読まない。置き場所を決める口を `--cache-dir` 1本に
/// 絞ることが本モジュールの目的（E12-2）であり、環境変数を読む経路を残すと
/// `--cache-dir` を渡しても環境変数が別の場所を指していればどちらが効くか
/// コードを読まないと分からなくなる。
///
/// ## `$TMPDIR` へフォールバックしない理由
///
/// `home` が取れない環境（コンテナ等）でも、`env::temp_dir()`（`$TMPDIR` 相当）
/// へ黙ってフォールバックすることはしない。フォールバックしても「キャッシュが
/// 知らない場所に増える」だけで何も嬉しくなく、むしろ危険: 次に別のプロセスが
/// 別の `$TMPDIR` を引けば同じ入力に対して別のキャッシュディレクトリを掴んでしまい、
/// `analyze` → `cut`（`--dtvi` 省略）の暗黙の受け渡しが**エラーを出さずに**外れる。
/// 「置き場所が決まらない」ことを `--cache-dir` を促すエラーで明示させる方が、
/// 黙って別の場所に作るより安全。
///
/// ## `HOME` が未設定でも、たいていエラーにはならない
///
/// `home` は呼び出し元（[`cache_root`]）が `std::env::home_dir()` の戻り値を
/// そのまま渡す。`std::env::home_dir()` は Windows での挙動の問題から非推奨
/// 扱いだった時期があるが、rustc 1.97.1 時点では非推奨警告が出ず、Unix では
/// `$HOME` 環境変数が unset でも `getpwuid` 経由でホームディレクトリを引ける
/// （実測済み）。つまりこの関数が実際に `None` を受け取る（＝エラーになる）のは
/// 「`$HOME` が無い」だけでは足りず、「呼び出しユーザーの passwd エントリすら
/// 無い」ような環境（コンテナで存在しない UID として動かす等）に限られる。
/// Go の `os.UserCacheDir` は `$HOME` 環境変数の有無だけを見てエラーにするため
/// 挙動が異なる点に注意（あちらは `$HOME` が無ければ即エラー、こちらは
/// passwd 由来のホームまで見るぶん範囲が狭い）。本ツールは macOS 専用
/// （CLAUDE.md「前提」）なので Windows の問題は関係しない。
fn default_cache_root(home: Option<&Path>) -> Result<PathBuf> {
    let home = home.ok_or_else(|| {
        anyhow::anyhow!(
            "ホームディレクトリを特定できませんでした。--cache-dir でキャッシュの\
             置き場所を明示してください（使い捨てにしたい場合は\
             --cache-dir \"$(mktemp -d)\"）"
        )
    })?;
    Ok(home.join(".cache").join("tachikaze"))
}

/// キャッシュディレクトリ名に使う stem を安全化する。
///
/// 空白・`/`・制御文字は `_` に置き換える。日本語などマルチバイト文字は
/// そのまま残す。かつて存在したシェルラッパー `scripts/tachikaze-cmcut`
/// （`auto` の追加に伴い削除済み、`[E11-7]`）にも同名の `safe_stem` があり、
/// この関数と同じ規則（空白・`/`・制御文字を `_` に置換）を実装していた。
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
/// `cache_dir` は [`cache_root`] にそのまま渡す（`--cache-dir` は**根だけ**を
/// 差し替え、この入力ごとのサブディレクトリ規則自体には触れない。触ると
/// `analyze` → `cut`（`--dtvi` 省略）の受け渡しが**エラーを出さずに**外れる）。
///
/// ハッシュだけでなく stem も併記するのは、万が一ハッシュが衝突しても別入力が
/// 同じディレクトリを共有しないようにするため（人間が見て区別しやすくもなる）。
fn cache_dir_for_input(cache_dir: Option<&Path>, input: &Path) -> Result<PathBuf> {
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
    Ok(cache_root(cache_dir)?.join(dir_name))
}

/// `cut --dtvi` 省略時に使う、入力ごとのキャッシュディレクトリ内の `.dtvi` の
/// パスを返す。`WorkDir::new` が使うキャッシュパス規則を [`cache_dir_for_input`]
/// 1か所に集約し、`cut` 側もそれをそのまま参照する（`analyze` が作った
/// ディレクトリと `cut` が探すディレクトリがずれると、無関係な入力の `.dtvi` を
/// 指してしまいかねないため）。
///
/// ディレクトリの作成は行わない。ファイルが存在するかどうかの確認・存在しない
/// 場合の扱いは呼び出し側の責務とする（`.dtvi` が無いのに検証を省略してはい
/// けないため、呼び出し側で明示的に判断させる）。
pub fn cached_dtvi_path(cache_dir: Option<&Path>, input: &Path) -> Result<PathBuf> {
    Ok(cache_dir_for_input(cache_dir, input)?.join(DTVI_FILE_NAME))
}

/// `cut` が既定で書き出す区間マップ（`work.mp4.segmap.json`）のキャッシュパスを返す。
///
/// [`cached_dtvi_path`] と同じ理由で [`cache_dir_for_input`] 1か所に集約する（`cut` は
/// `.dtvi` と同じ入力ごとのキャッシュディレクトリへ区間マップを書くため、パス規則が
/// ずれると無関係な入力のマップを指しうる）。ディレクトリの作成は行わない
/// （書き込み側で必要なら作る）。
pub fn cached_segment_map_path(cache_dir: Option<&Path>, input: &Path) -> Result<PathBuf> {
    Ok(cache_dir_for_input(cache_dir, input)?.join(SEGMENT_MAP_FILE_NAME))
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
pub fn prepared_input_path(cache_dir: Option<&Path>, input: &Path) -> Result<PathBuf> {
    Ok(cache_dir_for_input(cache_dir, input)?.join(INPUT_PREPARED_FILE_NAME))
}

/// `prepare` が字幕サイドカーを書き出すキャッシュパスを返す。
///
/// `extension` には `"ass"` / `"srt"` など、`.` を含まない拡張子を渡す
/// (どちらを使うかは字幕トラックのコーデックから `prepare` が決める。
/// `prepare::SubtitleFormat` 参照)。ディレクトリの作成は行わない。
pub fn subs_path(cache_dir: Option<&Path>, input: &Path, extension: &str) -> Result<PathBuf> {
    Ok(cache_dir_for_input(cache_dir, input)?.join(format!("{SUBS_BASE_NAME}.{extension}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// テスト用に、システムの一時ディレクトリ配下にユニークなディレクトリを作る。
    /// `WorkDir` 自体のテストなので `tempfile` クレートには頼らず、素朴な方式で
    /// 自前実装する。`--cache-dir` を明示的に渡すことでプロセス共有の環境変数を
    /// 一切触らずに済むため（E12-2 以前は `TACHIKAZE_CACHE_DIR` の書き換えを
    /// 直列化するための `Mutex` が必要だったが、根を引数で受け取る形にしたことで
    /// 不要になった）、テストは並行実行しても競合しない。
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
    #[test]
    fn does_not_create_files_next_to_input() {
        let cache_root = make_scratch_dir("cache-root-no-pollution");

        let input_dir = make_scratch_dir("input-dir");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let before = dir_entries(&input_dir);

        let work = WorkDir::new(Some(&cache_root), &input_path).expect("create work dir");
        work.link_input(&input_path).expect("link input");
        work.finish(true);

        let after = dir_entries(&input_dir);
        assert_eq!(
            before, after,
            "入力ディレクトリの内容が処理前後で変わってはいけない"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// キャッシュディレクトリは成功しても削除されない。
    #[test]
    fn default_cache_dir_is_kept_after_success() {
        let cache_root = make_scratch_dir("cache-root-kept");

        let input_dir = make_scratch_dir("cache-kept-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work = WorkDir::new(Some(&cache_root), &input_path).expect("create work dir");
        let path = work.path().to_path_buf();
        // `path` は WorkDir::new 内で canonicalize 済み（macOS では
        // /var → /private/var）なので、比較対象の cache_root も canonicalize する。
        let cache_root_canon = fs::canonicalize(&cache_root).expect("canonicalize cache root");
        assert!(
            path.starts_with(&cache_root_canon),
            "既定のキャッシュディレクトリは指定した --cache-dir 配下のはず"
        );
        work.link_input(&input_path).expect("link input");
        work.finish(true);

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
        let cache_root = make_scratch_dir("cache-root-reuse");

        let input_dir = make_scratch_dir("cache-reuse-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let first = WorkDir::new(Some(&cache_root), &input_path).expect("create work dir (1st)");
        let first_path = first.path().to_path_buf();
        first.finish(true);

        let second = WorkDir::new(Some(&cache_root), &input_path).expect("create work dir (2nd)");
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
        let cache_root = make_scratch_dir("cache-root-differ");

        let input_dir = make_scratch_dir("cache-differ-input");
        let first_input = input_dir.join("FIRST.mp4");
        let second_input = input_dir.join("SECOND.mp4");
        fs::write(&first_input, b"first").expect("write first input");
        fs::write(&second_input, b"second").expect("write second input");

        let first = WorkDir::new(Some(&cache_root), &first_input).expect("create work dir (1st)");
        let first_path = first.path().to_path_buf();
        first.finish(true);

        let second = WorkDir::new(Some(&cache_root), &second_input).expect("create work dir (2nd)");
        let second_path = second.path().to_path_buf();
        second.finish(true);

        assert_ne!(
            first_path, second_path,
            "異なる入力なら異なるキャッシュディレクトリのはず"
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// 入力が既に symlink だった場合も、その解決先へ張り替えて動く。
    #[test]
    fn link_input_works_when_input_is_already_a_symlink() {
        let cache_root = make_scratch_dir("symlink-cache");
        let input_dir = make_scratch_dir("symlink-input");
        let real_path = input_dir.join("REAL.mp4");
        fs::write(&real_path, b"dummy mp4 content").expect("write real input");

        let symlink_input = input_dir.join("IN.mp4");
        symlink(&real_path, &symlink_input).expect("create input symlink");

        let work = WorkDir::new(Some(&cache_root), &symlink_input).expect("create work dir");
        let work_mp4 = work.link_input(&symlink_input).expect("link input");

        let resolved = fs::canonicalize(&work_mp4).expect("resolve work.mp4");
        let expected = fs::canonicalize(&real_path).expect("resolve real path");
        assert_eq!(
            resolved, expected,
            "symlink の解決先が実ファイルと一致するはず"
        );

        work.finish(true);
        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// 既に work.mp4 がある場合は張り替える。
    #[test]
    fn link_input_replaces_existing_work_mp4() {
        let cache_root = make_scratch_dir("relink-cache");
        let input_dir = make_scratch_dir("relink-input");
        let first_input = input_dir.join("FIRST.mp4");
        let second_input = input_dir.join("SECOND.mp4");
        fs::write(&first_input, b"first").expect("write first input");
        fs::write(&second_input, b"second").expect("write second input");

        let work = WorkDir::new(Some(&cache_root), &first_input).expect("create work dir");
        work.link_input(&first_input).expect("link first input");
        let work_mp4 = work.link_input(&second_input).expect("link second input");

        let resolved = fs::canonicalize(&work_mp4).expect("resolve work.mp4");
        let expected = fs::canonicalize(&second_input).expect("resolve second input");
        assert_eq!(resolved, expected, "張り替え後は2番目の入力を指すはず");

        work.finish(true);
        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// 各中間ファイルパスの名前が集約されていることを確認する。
    ///
    /// `WorkDir::new` は入力の絶対パス解決（`fs::canonicalize`）を必ず行うため
    /// （使い捨て一時ディレクトリの経路は削除済み、E12-2）、実在する入力が要る。
    #[test]
    fn intermediate_file_names_are_correct() {
        let cache_root = make_scratch_dir("names-cache");
        let input_dir = make_scratch_dir("names-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work = WorkDir::new(Some(&cache_root), &input_path).expect("create work dir");
        assert_eq!(work.work_path().file_name().unwrap(), WORK_FILE_NAME);
        assert_eq!(work.dtvi_path().file_name().unwrap(), DTVI_FILE_NAME);
        assert_eq!(work.scp_path().file_name().unwrap(), SCP_FILE_NAME);
        assert_eq!(work.trim_path().file_name().unwrap(), TRIM_FILE_NAME);
        assert_eq!(
            work.detail_jls_path().file_name().unwrap(),
            DETAIL_JLS_FILE_NAME
        );
        work.finish(true);
        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// `prepared_input_path` / `subs_path` が `cached_dtvi_path` と同じ
    /// キャッシュディレクトリを指すことを確認する(`analyze` / `cut` /
    /// `prepare` が同じ入力に対して同じディレクトリを共有する前提)。
    #[test]
    fn prepared_input_and_subs_paths_share_cache_dir_with_dtvi() {
        let cache_root = make_scratch_dir("cache-root-prepare-paths");

        let input_dir = make_scratch_dir("prepare-paths-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let dtvi = cached_dtvi_path(Some(&cache_root), &input_path).expect("compute dtvi path");
        let prepared =
            prepared_input_path(Some(&cache_root), &input_path).expect("compute prepared path");
        let subs_ass =
            subs_path(Some(&cache_root), &input_path, "ass").expect("compute subs.ass path");
        let subs_srt =
            subs_path(Some(&cache_root), &input_path, "srt").expect("compute subs.srt path");

        assert_eq!(dtvi.parent(), prepared.parent());
        assert_eq!(dtvi.parent(), subs_ass.parent());
        assert_eq!(prepared.file_name().unwrap(), INPUT_PREPARED_FILE_NAME);
        assert_eq!(subs_ass.file_name().unwrap(), "subs.ass");
        assert_eq!(subs_srt.file_name().unwrap(), "subs.srt");

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// `--cache-dir`（明示的な根）でキャッシュの根を差し替えられる。
    #[test]
    fn explicit_cache_dir_overrides_default_root() {
        let cache_root = make_scratch_dir("cache-root-override");

        let input_dir = make_scratch_dir("cache-override-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let dir = cache_dir_for_input(Some(&cache_root), &input_path).expect("compute cache dir");
        assert!(
            dir.starts_with(&cache_root),
            "--cache-dir 配下になるはず: {}",
            dir.display()
        );

        fs::remove_dir_all(&input_dir).ok();
        fs::remove_dir_all(&cache_root).ok();
    }

    /// 完了条件（レビュー指摘）: 相対パスの `--cache-dir` は呼び出し元の
    /// カレントディレクトリを基準に絶対化される。`absolutize_cache_dir` の
    /// doc comment参照（絶対化しないと `prepare` が二重にネストしたパスを
    /// 探しに行って `No such file or directory` になる実機バグがあった）。
    #[test]
    fn absolutize_cache_dir_joins_relative_path_onto_current_dir() {
        let cwd = std::env::current_dir().expect("カレントディレクトリを取得できるはず");
        let resolved = absolutize_cache_dir(Path::new("relcache")).expect("絶対化に失敗しないはず");
        assert_eq!(resolved, cwd.join("relcache"));
        assert!(resolved.is_absolute());
    }

    #[test]
    fn absolutize_cache_dir_leaves_absolute_path_unchanged() {
        let absolute = Path::new("/tmp/some-absolute-cache-dir");
        let resolved = absolutize_cache_dir(absolute).expect("絶対化に失敗しないはず");
        assert_eq!(resolved, absolute);
    }

    /// `--cache-dir` に相対パスを渡しても、そこから導出する入力ごとの
    /// キャッシュディレクトリが絶対パスになることを確認する（`cache_root` が
    /// `absolutize_cache_dir` を経由することの統合的な確認）。
    #[test]
    fn cache_dir_for_input_is_absolute_even_with_relative_explicit_cache_dir() {
        let cwd = std::env::current_dir().expect("カレントディレクトリを取得できるはず");
        let input_dir = make_scratch_dir("relative-cache-dir-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let relative_cache_dir = Path::new("tachikaze-test-relative-cache-dir-unused");
        let dir =
            cache_dir_for_input(Some(relative_cache_dir), &input_path).expect("compute cache dir");
        assert!(dir.is_absolute(), "絶対パスになるはず: {}", dir.display());
        assert!(dir.starts_with(cwd.join(relative_cache_dir)));

        fs::remove_dir_all(&input_dir).ok();
    }

    /// `--cache-dir` 未指定時は `$HOME/.cache/tachikaze` になる。実際にホーム
    /// ディレクトリを汚さないよう、ディレクトリを作らず計算結果だけ確認する。
    #[test]
    fn cache_root_defaults_to_home_cache_tachikaze_when_no_explicit_dir() {
        let home = std::env::home_dir().expect("このテスト環境には HOME があるはず");
        let root = cache_root(None).expect("既定のキャッシュルートを計算できるはず");
        assert_eq!(root, home.join(".cache").join("tachikaze"));
    }

    /// `default_cache_root` はホームを引数で受け取る純粋関数なので、実際に
    /// `$HOME` や passwd を触らずに「ホームディレクトリが特定できない」経路
    /// （`cache_root` が `std::env::home_dir()` から `None` を受け取る場合）を
    /// 検証できる（`default_cache_root` の doc comment参照）。
    #[test]
    fn default_cache_root_errors_with_cache_dir_hint_when_home_is_none() {
        let err = default_cache_root(None).expect_err("home が無ければエラーのはず");
        let message = err.to_string();
        assert!(
            message.contains("--cache-dir"),
            "--cache-dir を促すメッセージのはず: {message}"
        );
    }

    #[test]
    fn default_cache_root_joins_cache_tachikaze_onto_given_home() {
        let home = Path::new("/home/example-user");
        let root = default_cache_root(Some(home)).expect("home があれば成功するはず");
        assert_eq!(root, home.join(".cache").join("tachikaze"));
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
        let cache_root = make_scratch_dir("cache-root-segmap");

        let input_dir = make_scratch_dir("segmap-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let segmap_path = cached_segment_map_path(Some(&cache_root), &input_path)
            .expect("compute cached segment map path");
        let dtvi_path =
            cached_dtvi_path(Some(&cache_root), &input_path).expect("compute cached dtvi path");

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
