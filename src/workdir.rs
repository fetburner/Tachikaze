//! 作業ディレクトリの用意と、入力ファイルへの symlink 戦略。
//!
//! `chapter_exe` はメディアファイルの隣に `<media>.dtvi` を自動生成する。入力を
//! 直接渡すと録画フォルダに中間ファイルが散るため、作業ディレクトリに入力への
//! symlink（`work.mp4`）を張り、そちらを外部ツールに渡す。symlink なので
//! 800 MB 級のファイルでもコピーは発生しない。
//!
//! 中間ファイルの名前（`work.mp4` / `work.mp4.dtvi` / `scp.txt` / `trim.avs` /
//! `detail.jls`）はこのモジュールに集約し、他のモジュールは `WorkDir` の
//! アクセサ経由でのみパスを得る。

use std::fs;
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

/// analyze / cut の中間ファイルを置く作業ディレクトリ。
///
/// `--work-dir` で明示された場合はそのディレクトリを使い、処理後も削除しない
/// （中間ファイルを見たい場合があるため）。未指定の場合は一時ディレクトリを
/// 作り、成功時のみ削除する。失敗時は原因調査のため中間ファイルを残す。
#[derive(Debug)]
pub struct WorkDir {
    path: PathBuf,
    /// `true` なら `finish` で削除しない（`--work-dir` 指定時）。
    keep: bool,
}

impl WorkDir {
    /// 作業ディレクトリを用意する。
    ///
    /// - `explicit` が `Some` の場合: そのディレクトリを使う（無ければ作る）。
    ///   `finish` では削除しない。
    /// - `explicit` が `None` の場合: OS の一時ディレクトリ配下にユニークな
    ///   ディレクトリを新規作成する。`finish(true)` で削除される。
    pub fn new(explicit: Option<PathBuf>) -> Result<Self> {
        match explicit {
            Some(path) => {
                fs::create_dir_all(&path).with_context(|| {
                    format!("作業ディレクトリの作成に失敗しました: {}", path.display())
                })?;
                Ok(Self { path, keep: true })
            }
            None => {
                let path = create_unique_temp_dir()?;
                Ok(Self { path, keep: false })
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
    /// - `--work-dir` 指定時（`keep == true`）: 何もしない。中間ファイルを見たい
    ///   場合があるため、成功・失敗にかかわらず残す。
    /// - 未指定時（`keep == false`）:
    ///   - `success == true`: 一時ディレクトリを削除する。
    ///   - `success == false`: 削除せず、調査用にパスをログへ出す
    ///     （再解析は数秒だが、失敗の調査には中間ファイルが要る）。
    pub fn finish(self, success: bool) {
        if self.keep {
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

#[cfg(test)]
mod tests {
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
    #[test]
    fn does_not_create_files_next_to_input() {
        let input_dir = make_scratch_dir("input-dir");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let before = dir_entries(&input_dir);

        let work_dir = WorkDir::new(None).expect("create work dir");
        work_dir.link_input(&input_path).expect("link input");
        work_dir.finish(true);

        let after = dir_entries(&input_dir);
        assert_eq!(
            before, after,
            "入力ディレクトリの内容が処理前後で変わってはいけない"
        );

        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
    }

    /// `--work-dir` 指定時は成功しても中間ファイル（symlink 等）が残る。
    #[test]
    fn explicit_work_dir_is_kept_after_success() {
        let input_dir = make_scratch_dir("explicit-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let explicit_dir = make_scratch_dir("explicit-workdir");

        let work_dir = WorkDir::new(Some(explicit_dir.clone())).expect("create work dir");
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

    /// `--work-dir` 未指定時、成功すると一時ディレクトリが消える。
    #[test]
    fn implicit_work_dir_is_removed_after_success() {
        let input_dir = make_scratch_dir("implicit-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work_dir = WorkDir::new(None).expect("create work dir");
        let path = work_dir.path().to_path_buf();
        work_dir.link_input(&input_path).expect("link input");
        work_dir.finish(true);

        assert!(!path.exists(), "成功時は一時ディレクトリが削除されるはず");

        fs::remove_dir_all(&input_dir).expect("cleanup input dir");
    }

    /// `--work-dir` 未指定時、失敗すると一時ディレクトリは残る（調査用）。
    #[test]
    fn implicit_work_dir_is_kept_after_failure() {
        let input_dir = make_scratch_dir("implicit-fail-input");
        let input_path = input_dir.join("IN.mp4");
        fs::write(&input_path, b"dummy mp4 content").expect("write input");

        let work_dir = WorkDir::new(None).expect("create work dir");
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

        let work_dir = WorkDir::new(None).expect("create work dir");
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

        let work_dir = WorkDir::new(None).expect("create work dir");
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
        let work_dir = WorkDir::new(None).expect("create work dir");
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
}
