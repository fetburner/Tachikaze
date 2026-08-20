//! 学習済み `.lgd` を辞書ディレクトリに蓄積し、解像度が一致する候補をスコアで
//! 自動選択する（E18-4、issue #134）。
//!
//! ## 解くべき問題
//!
//! 学習した `.lgd` を毎回作り直すのは無駄である。**同じ局のロゴは変わらない。**
//! 実測で、BS日テレの2022年のファイルと2026年のファイル（別番組）で、推定矩形が
//! 完全に一致し、`.lgd` を相互に適用した `trim.avs` が両方向でバイト単位一致した。
//!
//! Amatsukaze も同じ構造を持つ。サービスIDごとにロゴ候補を貯め（`LogoSetting`）、
//! 候補を全部渡して `LogoFrame::selectLogo` がスコアで1つ選ぶ。本ツールは mp4 に
//! PMT が無くサービスIDが取れないため、**「解像度が一致するものを全部候補にして
//! スコアで選ぶ」**に置き換える（[`select_candidate`]）。
//!
//! ## 実機での相互適用の検証は #135 に結線した後で行う
//!
//! [`select_candidate`] はこの時点ではどのコマンドからも呼ばれていない
//! （`analyze`/`auto` への結線は別 issue #135 の担当）。モジュール単体のロジック
//! （スコア計算・最小値選択・閾値によるフォールバック）は合成した `(corr0,
//! corr1)` の列で検証済みだが、「同じ局の別ファイルで学習した `.lgd` が実際に
//! 候補として選ばれ、その `.lgd` を使った `trim.avs` が自前学習と一致する」という
//! 実機検証（issue #134「実測で確認するもの」）は、#135 で CLI に結線し、実際の
//! 放送録画で試せるようになってから行う。
//!
//! ## 辞書ディレクトリは `--cache-dir` と別系統（罠）
//!
//! `--cache-dir`（[`crate::cli::Cli::cache_dir`]）は「`analyze` を再実行すれば
//! 作り直せる中間物」の置き場所と定義されている（`src/cli.rs` の doc comment）。
//! `.lgd` は**元の録画を消すと作り直せない**ため、この規約に反する。
//! `~/.cache` に置くと、キャッシュを丸ごと削除する運用（使い捨て運用や
//! ディスク整理）で学習結果ごと消えてしまう。そのため辞書は XDG の
//! **データ**ディレクトリ（`$XDG_DATA_HOME`、未設定なら `~/.local/share`）配下に
//! 置く（[`resolve_dict_dir`]）。
//!
//! ## なぜ辞書だけ `$XDG_DATA_HOME` を読むのか（`workdir.rs` との対比）
//!
//! `src/workdir.rs::default_cache_root` は環境変数を一切読まない方針を取っている
//! （`docs/architecture.md`「パス解決」節「XDG 由来の環境変数を読まないと決めた
//! 理由」）。置き場所を決める口を `--cache-dir` の1本に絞ることで、環境変数と
//! `--cache-dir` が同時に設定されたときどちらが効くか分からなくなる曖昧さを
//! 消すためである。
//!
//! **辞書ディレクトリはこの方針の例外**で、[`resolve_dict_dir`] は実際に
//! `$XDG_DATA_HOME` を読む。理由は2つある。
//!
//! 1. 蓄積データという性質上、XDG のデータディレクトリ規約にそのまま従うのが
//!    利用者にとって自然である。`--cache-dir` が借りているのはディレクトリ
//!    **名**（`~/.cache/tachikaze`）だけだが、辞書は規約そのもの
//!    （`$XDG_DATA_HOME` の値、未設定時のフォールバック）に従う
//! 2. 呼び出し側の明示的な上書き引数（`explicit` 引数、CLI への結線は #135）が
//!    常に最優先で、環境変数より先に効く。優先順位は「上書き引数 > 環境変数 >
//!    既定」で固定されており、`--cache-dir` のときのように複数の口が同時に
//!    設定された場合の曖昧さは生じない
//!
//! ## 既存ファイルを上書きしないこと（罠）
//!
//! 辞書は蓄積するものなので、同じ stem の入力を再処理したときに前回の学習結果を
//! 潰してはいけない（Amatsukaze も局ごとに複数のロゴを保持し、選択で解決している）。
//! [`save`] は既存ファイルがあれば `-2`、`-3` ... と連番を足して衝突を避ける。
//!
//! ## 解像度が違う `.lgd` を候補に入れてはいけない（罠）
//!
//! Amatsukaze も `logo.getImgWidth() != vi.width` なら評価しない。矩形が映像範囲外に
//! なる、または別位置を見ることになり、静かに誤検出する。[`list_candidates`] は
//! `imgw`/`imgh` が対象映像の解像度と一致するものだけを候補として返す。
//!
//! ## 検出割合の絶対閾値を省略しないこと（罠）
//!
//! スコアは候補間の**相対比較**しかしていないので、全部が外れていても最小値は
//! 必ず存在する。スコアだけで選ぶと、同解像度の別局のロゴが「一番マシ」として
//! 必ず1つ選ばれてしまう。[`choose_best`] は最小スコアの候補について、検出割合が
//! 閾値（`src/analyze.rs` の `LOGO_DETECTION_THRESHOLD` / `_SHORT` と同じ値。
//! 非公開のため独立に定義し直す、`lgd.rs`/`scan.rs` の `LOGO_HEADER_LEN`/`MAGIC`
//! 二重定義と同じ流儀）未満なら「該当なし」（`None`）を返す。
//!
//! ## `LogoMask::new` は失敗しうる（罠）
//!
//! マスクに使える画素が足りない（`w < 5` または `h < 5`）等で失敗しうる。失敗した
//! 候補は警告して飛ばし、他の候補の採点は続ける（[`select_candidate`]）。
//!
//! ## 採点（Amatsukaze `LogoFrame::selectLogo` 相当）
//!
//! 候補それぞれについて、キーフレーム標本の各フレームで `corr0`/`corr1`
//! （[`crate::logo::score::LogoMask::evaluate`]）を求め、
//!
//! - `corr0 > 0.2` かつ `|corr1| < 0.2` を満たすフレームを「検出」と数える
//! - スコア = `(検出フレームでの |corr1| の平均) × (全フレーム数 / 検出フレーム数)`。
//!   検出0件なら無限大（[`score_from_corrs`]）
//! - スコア**最小**の候補を選ぶ（[`choose_best`]）。同時に検出フレーム割合も返す
//!
//! 1フレームの全画面から各候補の矩形を切り出して `evaluate` に渡す。候補ごとに
//! 矩形が違うため、**全画面を1回読んで候補ごとに切り出す**（候補ごとに ffmpeg を
//! 起動し直さない。実測でキーフレーム走査は30分1080pで4.25秒なので、1回に抑えれば
//! 無視できる）。

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use anyhow::Result;

use crate::errctx::PathContext as _;
use crate::logo::frames::VideoSize;
use crate::logo::lgd::{self, LogoData};
use crate::logo::scan;
use crate::logo::score::LogoMask;

/// 辞書ディレクトリの既定の相対パス（`$XDG_DATA_HOME` または `~/.local/share` の
/// 直下に付ける部分）。
fn append_tachikaze_logos(base: &Path) -> PathBuf {
    base.join("tachikaze").join("logos")
}

/// 辞書ディレクトリを解決する。
///
/// - `explicit`（呼び出し側の上書き引数）があれば、それをそのまま使う。
/// - 無ければ `$XDG_DATA_HOME/tachikaze/logos`。`XDG_DATA_HOME` が未設定または
///   空文字列なら `~/.local/share/tachikaze/logos`。
/// - ホームディレクトリが特定できない場合（`XDG_DATA_HOME` も未設定）は、
///   上書き引数を促すエラーで停止する（`src/workdir.rs` の既定キャッシュ解決と
///   同じ流儀）。
pub fn resolve_dict_dir(explicit: Option<&Path>) -> Result<PathBuf> {
    if let Some(dir) = explicit {
        return Ok(dir.to_path_buf());
    }
    let xdg_data_home = std::env::var_os("XDG_DATA_HOME")
        .filter(|v| !v.is_empty())
        .map(PathBuf::from);
    default_dict_dir(xdg_data_home.as_deref(), std::env::home_dir().as_deref())
}

/// [`resolve_dict_dir`] から `$XDG_DATA_HOME` / ホームディレクトリの取得を分離した
/// 純粋関数。テストが実際の環境変数やホームディレクトリを触らずに3経路
/// （`XDG_DATA_HOME` 設定時 / 未設定時 / ホーム不明時）を検証できるようにする
/// （`src/workdir.rs::default_cache_root` と同じ理由）。
fn default_dict_dir(xdg_data_home: Option<&Path>, home: Option<&Path>) -> Result<PathBuf> {
    if let Some(xdg) = xdg_data_home {
        return Ok(append_tachikaze_logos(xdg));
    }
    let home = home.ok_or_else(|| {
        anyhow::anyhow!(
            "ホームディレクトリを特定できず、ロゴ辞書の既定の置き場所\
             （$XDG_DATA_HOME/tachikaze/logos または ~/.local/share/tachikaze/logos）を\
             決められませんでした。呼び出し側の上書き引数で明示してください。"
        )
    })?;
    Ok(append_tachikaze_logos(&home.join(".local").join("share")))
}

/// `logo` を辞書ディレクトリに `.lgd` として保存する。
///
/// ファイル名は `<input のファイル stem>.lgd`。既に同名のファイルがあれば
/// `-2`、`-3` ... と連番を足し、**既存ファイルを上書きしない**（モジュール
/// doc comment「既存ファイルを上書きしないこと」参照）。書き出し自体は
/// [`scan::write_lgd`] を使う。
///
/// 辞書ディレクトリが無ければ作成する。
pub fn save(dict_dir: &Path, logo: &LogoData, input: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dict_dir).path_ctx("ロゴ辞書ディレクトリの作成", dict_dir)?;

    let stem = input.file_stem().and_then(|s| s.to_str()).unwrap_or("logo");

    let mut path = dict_dir.join(format!("{stem}.lgd"));
    let mut suffix = 2u32;
    while path.exists() {
        path = dict_dir.join(format!("{stem}-{suffix}.lgd"));
        suffix += 1;
    }

    scan::write_lgd(logo, &path)?;
    Ok(path)
}

/// 辞書ディレクトリで見つかった、解像度が一致する候補1件。
#[derive(Debug, Clone)]
pub struct DictCandidate {
    pub path: PathBuf,
    pub logo: LogoData,
}

/// 辞書ディレクトリ内の `*.lgd` を列挙し、`imgw`/`imgh` が `video_size` と一致する
/// ものだけを候補として返す（モジュール doc comment「解像度が違う `.lgd` を候補に
/// 入れてはいけない」参照）。
///
/// - 辞書ディレクトリが存在しない場合は空の候補列を返す（初回実行時に自然に
///   起きるので、エラーにはしない）。
/// - 読めないファイル（[`lgd::parse`] が失敗するもの）は警告を出して飛ばす。
///   辞書に壊れたファイルが1つ混ざっただけで全体が止まるのは困る（Amatsukaze も
///   `LogoFrame` のコンストラクタで読み込みエラーを無視している）。
/// - 列挙順はファイルパスの昇順に固定する（ディレクトリの列挙順は OS /
///   ファイルシステム依存で決定的でないため）。
pub fn list_candidates(dict_dir: &Path, video_size: VideoSize) -> Vec<DictCandidate> {
    let entries = match fs::read_dir(dict_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Vec::new(),
        Err(err) => {
            eprintln!(
                "[logo-dict] 警告: 辞書ディレクトリ {} の読み取りに失敗したため、\
                 候補なしとして扱います: {err}",
                dict_dir.display()
            );
            return Vec::new();
        }
    };

    let mut paths: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|e| e.path()))
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("lgd"))
        .collect();
    paths.sort();

    let mut candidates = Vec::new();
    for path in paths {
        match lgd::read(&path) {
            Ok(logo) => {
                let matches_resolution = logo.imgw >= 0
                    && logo.imgh >= 0
                    && logo.imgw as u32 == video_size.width
                    && logo.imgh as u32 == video_size.height;
                if matches_resolution {
                    candidates.push(DictCandidate { path, logo });
                }
            }
            Err(err) => {
                eprintln!(
                    "[logo-dict] 警告: {} の読み込みに失敗したためスキップします: {err}",
                    path.display()
                );
            }
        }
    }
    candidates
}

/// [`score_from_corrs`] が「検出」と数えるフレームの下限（`corr0`）。
const DETECTED_CORR0_MIN: f32 = 0.2;
/// [`score_from_corrs`] が「検出」と数えるフレームの上限（`|corr1|`）。
const DETECTED_CORR1_ABS_MAX: f32 = 0.2;

/// `--logo` フォールバックと同じ検出割合の絶対閾値（`src/analyze.rs` の
/// `LOGO_DETECTION_THRESHOLD` と同じ値。非公開のため独立に定義し直す
/// （`lgd.rs`/`scan.rs` が `LOGO_HEADER_LEN`/`MAGIC` を独立定義するのと同じ理由。
/// 値が食い違えば実測での比較で気付ける）。
const DETECTION_THRESHOLD: f64 = 0.1;
/// 映像長が [`SHORT_VIDEO_SECONDS`] 以下の場合に使う、緩めた閾値
/// （`src/analyze.rs::LOGO_DETECTION_THRESHOLD_SHORT` と同じ値）。
const DETECTION_THRESHOLD_SHORT: f64 = 0.03;
/// 上記の緩い閾値を使う映像長の上限（7分、秒単位。
/// `src/analyze.rs::LOGO_DETECTION_SHORT_VIDEO_SECONDS` と同じ値）。
const SHORT_VIDEO_SECONDS: f64 = 7.0 * 60.0;

/// 検出割合の絶対閾値を決める（`src/analyze.rs::logo_detection_threshold` と
/// 同じ規則）。
fn detection_threshold(duration_seconds: f64) -> f64 {
    if duration_seconds <= SHORT_VIDEO_SECONDS {
        DETECTION_THRESHOLD_SHORT
    } else {
        DETECTION_THRESHOLD
    }
}

/// 候補1件の採点結果。
#[derive(Debug, Clone, Copy, PartialEq)]
struct CandidateScore {
    /// `(検出フレームでの |corr1| の平均) × (全フレーム数 / 検出フレーム数)`。
    /// 検出0件なら無限大。
    score: f64,
    /// 検出フレーム数 / 全フレーム数（全フレーム数0件のときは0.0）。
    detected_fraction: f64,
}

/// `(corr0, corr1)` の列から [`CandidateScore`] を計算する（Amatsukaze
/// `LogoFrame::selectLogo` 相当。モジュール doc comment「採点」参照）。
fn score_from_corrs(corrs: &[(f32, f32)]) -> CandidateScore {
    let total = corrs.len();
    let mut detected_count: usize = 0;
    let mut abs_corr1_sum: f64 = 0.0;
    for &(corr0, corr1) in corrs {
        if corr0 > DETECTED_CORR0_MIN && corr1.abs() < DETECTED_CORR1_ABS_MAX {
            detected_count += 1;
            abs_corr1_sum += corr1.abs() as f64;
        }
    }

    let score = if detected_count == 0 {
        f64::INFINITY
    } else {
        (abs_corr1_sum / detected_count as f64) * (total as f64 / detected_count as f64)
    };
    let detected_fraction = if total == 0 {
        0.0
    } else {
        detected_count as f64 / total as f64
    };

    CandidateScore {
        score,
        detected_fraction,
    }
}

/// スコア最小の候補を選び、検出割合が閾値未満なら「該当なし」（`None`）にする
/// （モジュール doc comment「検出割合の絶対閾値を省略しないこと」参照）。
///
/// `scores` が空なら `None`。同値タイは `scores` の先頭側（インデックスが小さい方、
/// [`list_candidates`] が返すパス昇順に対応）を選ぶ。
///
/// 戻り値は `(選ばれた候補のインデックス, 検出割合)`。
fn choose_best(scores: &[CandidateScore], threshold: f64) -> Option<(usize, f64)> {
    let (best_idx, best) = scores.iter().enumerate().min_by(|(_, a), (_, b)| {
        a.score
            .partial_cmp(&b.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    })?;

    if best.detected_fraction < threshold {
        return None;
    }
    Some((best_idx, best.detected_fraction))
}

/// [`LogoData`] のロゴ矩形（`imgx`/`imgy`/`w`/`h`）を、全画面フレームからの
/// 切り出し範囲として検証する。`imgx`/`imgy` が負、または矩形が `imgw`/`imgh` の
/// 範囲外に出ている（壊れた `.lgd` を想定）場合は `None` を返す。
fn candidate_crop_rect(logo: &LogoData) -> Option<(usize, usize, usize, usize)> {
    if logo.imgx < 0 || logo.imgy < 0 || logo.w <= 0 || logo.h <= 0 {
        return None;
    }
    let (x, y, w, h) = (
        i64::from(logo.imgx),
        i64::from(logo.imgy),
        i64::from(logo.w),
        i64::from(logo.h),
    );
    let (imgw, imgh) = (i64::from(logo.imgw), i64::from(logo.imgh));
    if x + w > imgw || y + h > imgh {
        return None;
    }
    Some((x as usize, y as usize, w as usize, h as usize))
}

/// 全画面フレーム（`frame_width` x 任意の高さ、行優先、8bit）から、`(x, y, w, h)`
/// の矩形を切り出す。
fn crop_frame(frame: &[u8], frame_width: usize, x: usize, y: usize, w: usize, h: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(w * h);
    for row in 0..h {
        let start = (y + row) * frame_width + x;
        out.extend_from_slice(&frame[start..start + w]);
    }
    out
}

/// 辞書から選ばれた候補。
#[derive(Debug, Clone)]
pub struct DictSelection {
    pub path: PathBuf,
    pub logo: LogoData,
    /// 検出フレーム割合（[`choose_best`] が返したもの、閾値以上であることが
    /// 保証されている）。
    pub detected_fraction: f64,
}

/// 辞書ディレクトリから、`video_size` の映像に使うロゴデータを1つ選ぶ
/// （Amatsukaze `LogoFrame::selectLogo` 相当。モジュール doc comment「採点」参照）。
///
/// - `duration_seconds`: 検出割合の絶対閾値を決めるための映像長
///   （`src/analyze.rs::logo_detection_threshold` と同じ規則、[`detection_threshold`]）。
/// - `stream_keyframes`: キーフレームだけを流す供給関数（典型的には
///   [`crate::logo::frames::stream_keyframe_luma_frames`] を束縛したクロージャ）。
///   全画面（クロップ前）の輝度平面を1フレームずつ渡す。**候補ごとに呼び直さない**
///   （モジュール doc comment「採点」末尾の「全画面を1回読んで候補ごとに切り出す」
///   参照）。
///
/// 候補が1件も無い、または [`LogoMask::new`] が全候補で失敗した場合は `Ok(None)`
/// を返す（フレーム走査自体を行わない）。
pub fn select_candidate(
    dict_dir: &Path,
    video_size: VideoSize,
    duration_seconds: f64,
    mut stream_keyframes: impl FnMut(&mut dyn FnMut(&[u8]) -> anyhow::Result<()>) -> anyhow::Result<u64>,
) -> anyhow::Result<Option<DictSelection>> {
    let candidates = list_candidates(dict_dir, video_size);
    if candidates.is_empty() {
        return Ok(None);
    }

    struct Prepared {
        path: PathBuf,
        logo: LogoData,
        mask: LogoMask,
        rect: (usize, usize, usize, usize),
    }

    let mut prepared: Vec<Prepared> = Vec::new();
    for candidate in candidates {
        let Some(rect) = candidate_crop_rect(&candidate.logo) else {
            eprintln!(
                "[logo-dict] 警告: {} のロゴ矩形が画像範囲外のためスキップします",
                candidate.path.display()
            );
            continue;
        };
        match LogoMask::new(&candidate.logo) {
            Ok(mask) => prepared.push(Prepared {
                path: candidate.path,
                logo: candidate.logo,
                mask,
                rect,
            }),
            Err(err) => {
                eprintln!(
                    "[logo-dict] 警告: {} のマスク生成に失敗したためスキップします: {err}",
                    candidate.path.display()
                );
            }
        }
    }
    if prepared.is_empty() {
        return Ok(None);
    }

    let frame_width = video_size.width as usize;
    let expected_frame_len = frame_width * video_size.height as usize;
    let mut corrs: Vec<Vec<(f32, f32)>> = vec![Vec::new(); prepared.len()];
    stream_keyframes(&mut |frame: &[u8]| {
        // `stream_keyframes` は呼び出し側が渡すクロージャで、`video_size` とは
        // 別の値として受け取っている。ffmpeg 呼び出しと `video_size` の対応は
        // 呼び出し側の責務だが、ここで食い違いを検査しないと `crop_frame` が
        // 誤ったオフセットで画素を切り出し、エラーを出さずに別位置を検出結果と
        // して返しかねない（CLAUDE.md 罠3の一般形）。
        anyhow::ensure!(
            frame.len() == expected_frame_len,
            "stream_keyframes が渡したフレームのバイト数({})が video_size\
             ({}x{}={}バイト)と一致しません。呼び出し側の video_size 指定が\
             実際のフレームと食い違っています。",
            frame.len(),
            video_size.width,
            video_size.height,
            expected_frame_len,
        );
        for (i, p) in prepared.iter().enumerate() {
            let (x, y, w, h) = p.rect;
            let cropped = crop_frame(frame, frame_width, x, y, w, h);
            corrs[i].push(p.mask.evaluate(&cropped));
        }
        Ok(())
    })?;

    let scores: Vec<CandidateScore> = corrs.iter().map(|c| score_from_corrs(c)).collect();
    for (p, s) in prepared.iter().zip(&scores) {
        if s.score.is_finite() {
            eprintln!(
                "[logo-dict] 候補 {}: スコア={:.4} 検出割合={:.3}",
                p.path.display(),
                s.score,
                s.detected_fraction
            );
        } else {
            eprintln!(
                "[logo-dict] 候補 {}: スコア=inf(検出0件) 検出割合={:.3}",
                p.path.display(),
                s.detected_fraction
            );
        }
    }

    let threshold = detection_threshold(duration_seconds);
    let Some((best_idx, best_fraction)) = choose_best(&scores, threshold) else {
        eprintln!("[logo-dict] 検出割合が閾値({threshold:.3})未満のため該当なしとします");
        return Ok(None);
    };

    let chosen = &prepared[best_idx];
    eprintln!(
        "[logo-dict] 採用: {}（検出割合 {best_fraction:.3}）",
        chosen.path.display()
    );
    Ok(Some(DictSelection {
        path: chosen.path.clone(),
        logo: chosen.logo.clone(),
        detected_fraction: best_fraction,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// テスト用に、システムの一時ディレクトリ配下にユニークなディレクトリを作る
    /// （`src/workdir.rs` のテストヘルパーと同じ流儀。プロセス共有の環境変数を
    /// 一切触らずに済むため、テストは並行実行しても競合しない）。
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

    fn sample_logo(imgw: i32, imgh: i32) -> LogoData {
        LogoData {
            w: 4,
            h: 2,
            log_uv_x: 1,
            log_uv_y: 1,
            imgw,
            imgh,
            imgx: 2,
            imgy: 2,
            name: "テストロゴ".to_string(),
            service_id: scan::UNSPECIFIED_SERVICE_ID,
            a_y: vec![1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8],
            b_y: vec![0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08],
            a_u: vec![1.0, 1.0],
            b_u: vec![0.0, 0.0],
            a_v: vec![1.0, 1.0],
            b_v: vec![0.0, 0.0],
        }
    }

    // ---------------------------------------------------------------
    // resolve_dict_dir / default_dict_dir
    // ---------------------------------------------------------------

    #[test]
    fn default_dict_dir_uses_xdg_data_home_when_set() {
        let xdg = Path::new("/custom/xdg-data-home");
        let home = Path::new("/home/example-user");
        let dir = default_dict_dir(Some(xdg), Some(home)).expect("成功するはず");
        assert_eq!(dir, xdg.join("tachikaze").join("logos"));
    }

    #[test]
    fn default_dict_dir_falls_back_to_home_local_share_when_xdg_unset() {
        let home = Path::new("/home/example-user");
        let dir = default_dict_dir(None, Some(home)).expect("成功するはず");
        assert_eq!(
            dir,
            home.join(".local")
                .join("share")
                .join("tachikaze")
                .join("logos")
        );
    }

    #[test]
    fn default_dict_dir_errors_when_home_is_unknown_and_xdg_unset() {
        let err = default_dict_dir(None, None).expect_err("ホームが無ければエラーのはず");
        let message = err.to_string();
        assert!(
            message.contains("上書き引数"),
            "上書き引数を促すメッセージのはず: {message}"
        );
    }

    #[test]
    fn resolve_dict_dir_uses_explicit_override_without_touching_env() {
        let explicit = Path::new("/explicit/override/dir");
        let dir = resolve_dict_dir(Some(explicit)).expect("成功するはず");
        assert_eq!(dir, explicit);
    }

    // ---------------------------------------------------------------
    // save
    // ---------------------------------------------------------------

    #[test]
    fn save_twice_with_same_stem_keeps_both_files_without_overwriting() {
        let dict_dir = make_scratch_dir("dict-save");
        let input = Path::new("/recordings/BS日テレ.mp4");

        let logo1 = sample_logo(1920, 1080);
        let mut logo2 = sample_logo(1920, 1080);
        logo2.name = "2回目の学習".to_string();

        let path1 = save(&dict_dir, &logo1, input).expect("1回目の保存は成功するはず");
        let path2 = save(&dict_dir, &logo2, input).expect("2回目の保存は成功するはず");

        assert_ne!(path1, path2, "同じstemでも別ファイルになるはず");
        assert!(path1.exists());
        assert!(path2.exists());

        let reread1 = lgd::read(&path1).expect("1回目のファイルは読めるはず");
        let reread2 = lgd::read(&path2).expect("2回目のファイルは読めるはず");
        assert_eq!(
            reread1.name, "テストロゴ",
            "1回目の内容が保たれているはず(上書きされていない)"
        );
        assert_eq!(reread2.name, "2回目の学習");

        fs::remove_dir_all(&dict_dir).ok();
    }

    // ---------------------------------------------------------------
    // list_candidates
    // ---------------------------------------------------------------

    #[test]
    fn list_candidates_excludes_mismatched_resolution() {
        let dict_dir = make_scratch_dir("dict-list-resolution");

        let matching = sample_logo(1920, 1080);
        let mismatched = sample_logo(1280, 720);
        scan::write_lgd(&matching, &dict_dir.join("matching.lgd")).expect("write matching");
        scan::write_lgd(&mismatched, &dict_dir.join("mismatched.lgd")).expect("write mismatched");

        let candidates = list_candidates(
            &dict_dir,
            VideoSize {
                width: 1920,
                height: 1080,
            },
        );

        assert_eq!(candidates.len(), 1, "解像度が一致する1件だけが残るはず");
        assert_eq!(candidates[0].path.file_name().unwrap(), "matching.lgd");

        fs::remove_dir_all(&dict_dir).ok();
    }

    #[test]
    fn list_candidates_skips_corrupted_file_but_returns_others() {
        let dict_dir = make_scratch_dir("dict-list-corrupted");

        let valid = sample_logo(1920, 1080);
        scan::write_lgd(&valid, &dict_dir.join("valid.lgd")).expect("write valid");
        fs::write(dict_dir.join("broken.lgd"), b"not a valid lgd file").expect("write broken");

        let candidates = list_candidates(
            &dict_dir,
            VideoSize {
                width: 1920,
                height: 1080,
            },
        );

        assert_eq!(
            candidates.len(),
            1,
            "壊れたファイルはスキップされ、有効な1件だけが残るはず"
        );
        assert_eq!(candidates[0].path.file_name().unwrap(), "valid.lgd");

        fs::remove_dir_all(&dict_dir).ok();
    }

    #[test]
    fn list_candidates_returns_empty_when_dict_dir_does_not_exist() {
        let missing = std::env::temp_dir().join("tachikaze-test-dict-nonexistent-dir-xyz");
        let candidates = list_candidates(
            &missing,
            VideoSize {
                width: 1920,
                height: 1080,
            },
        );
        assert!(candidates.is_empty());
    }

    // ---------------------------------------------------------------
    // score_from_corrs / choose_best
    // ---------------------------------------------------------------

    #[test]
    fn score_from_corrs_computes_expected_score_and_detected_fraction() {
        // 4フレーム中2フレームが検出条件を満たす(corr0>0.2 かつ |corr1|<0.2)。
        // 検出フレームの|corr1|平均 = (0.1+0.05)/2 = 0.075。
        // スコア = 0.075 * (4/2) = 0.15。
        let corrs = [(0.9, 0.1), (0.05, 0.5), (0.8, 0.05), (0.1, 0.9)];
        let score = score_from_corrs(&corrs);
        assert!((score.score - 0.15).abs() < 1e-6, "score={}", score.score);
        assert!(
            (score.detected_fraction - 0.5).abs() < 1e-9,
            "detected_fraction={}",
            score.detected_fraction
        );
    }

    #[test]
    fn score_from_corrs_is_infinite_when_no_frame_is_detected() {
        let corrs = [(0.05, 0.5), (0.1, -0.9), (0.0, 0.3)];
        let score = score_from_corrs(&corrs);
        assert!(score.score.is_infinite());
        assert!((score.detected_fraction - 0.0).abs() < 1e-9);
    }

    #[test]
    fn choose_best_picks_minimum_score_and_excludes_zero_detection_candidate() {
        // 候補0: 検出0件(スコア無限大)。候補1: スコア0.15、検出割合0.5。
        // 候補2: スコア0.4(候補1より悪い)、検出割合0.8。
        let scores = [
            CandidateScore {
                score: f64::INFINITY,
                detected_fraction: 0.0,
            },
            CandidateScore {
                score: 0.15,
                detected_fraction: 0.5,
            },
            CandidateScore {
                score: 0.4,
                detected_fraction: 0.8,
            },
        ];

        let (idx, fraction) = choose_best(&scores, 0.1).expect("閾値以上の候補があるはず");
        assert_eq!(
            idx, 1,
            "検出0件の候補ではなく、スコア最小の候補1が選ばれるはず"
        );
        assert!((fraction - 0.5).abs() < 1e-9);
    }

    #[test]
    fn choose_best_returns_none_when_best_detected_fraction_is_below_threshold() {
        let scores = [CandidateScore {
            score: 0.15,
            detected_fraction: 0.05,
        }];

        let result = choose_best(&scores, 0.1);
        assert!(
            result.is_none(),
            "検出割合(0.05)が閾値(0.1)未満なので該当なしになるはず"
        );
    }

    #[test]
    fn choose_best_returns_none_for_empty_scores() {
        assert!(choose_best(&[], 0.1).is_none());
    }

    // ---------------------------------------------------------------
    // detection_threshold
    // ---------------------------------------------------------------

    #[test]
    fn detection_threshold_is_lenient_for_short_videos() {
        assert_eq!(detection_threshold(60.0), DETECTION_THRESHOLD_SHORT);
        assert_eq!(
            detection_threshold(SHORT_VIDEO_SECONDS),
            DETECTION_THRESHOLD_SHORT
        );
    }

    #[test]
    fn detection_threshold_is_default_for_long_videos() {
        assert_eq!(
            detection_threshold(SHORT_VIDEO_SECONDS + 1.0),
            DETECTION_THRESHOLD
        );
        assert_eq!(detection_threshold(3600.0), DETECTION_THRESHOLD);
    }

    // ---------------------------------------------------------------
    // candidate_crop_rect
    // ---------------------------------------------------------------

    #[test]
    fn candidate_crop_rect_rejects_rect_extending_past_image_bounds() {
        let mut logo = sample_logo(1920, 1080);
        // w=4なのでimgx=1918だと右端が1922になり、imgw=1920をはみ出す。
        logo.imgx = 1918;
        assert!(candidate_crop_rect(&logo).is_none());
    }

    #[test]
    fn candidate_crop_rect_accepts_rect_within_bounds() {
        let logo = sample_logo(1920, 1080);
        let rect = candidate_crop_rect(&logo).expect("範囲内なので成功するはず");
        assert_eq!(rect, (2, 2, 4, 2));
    }

    // ---------------------------------------------------------------
    // crop_frame
    // ---------------------------------------------------------------

    #[test]
    fn crop_frame_extracts_expected_rectangle() {
        // 4x3の全画面から (x=1, y=1, w=2, h=2) を切り出す。
        #[rustfmt::skip]
        let frame: Vec<u8> = vec![
            0, 1, 2, 3,
            4, 5, 6, 7,
            8, 9, 10, 11,
        ];
        let cropped = crop_frame(&frame, 4, 1, 1, 2, 2);
        assert_eq!(cropped, vec![5, 6, 9, 10]);
    }

    // ---------------------------------------------------------------
    // select_candidate（ffmpeg を起動しない。`stream_keyframes` はクロージャで
    // 合成フレームを渡す）
    // ---------------------------------------------------------------

    /// `score.rs` のテストヘルパー `alpha_ab` と同じ式
    /// （`background = a*observed + b*maxv` が `observed = alpha*color +
    /// (1-alpha)*background` の逆になるように定めたもの）。テスト専用に
    /// このモジュール内で独立に再現する（サブissueの自己完結の方針）。
    fn alpha_ab(alpha: f32, color: f32) -> (f32, f32) {
        let a = 1.0 / (1.0 - alpha);
        let b = -alpha * color / (255.0 * (1.0 - alpha));
        (a, b)
    }

    /// `score.rs` の `cross_logo` と同じ十字パターン（縦棒・横棒で色が違う）を、
    /// 任意の `imgx`/`imgy`/`imgw`/`imgh` を持つ [`LogoData`] として組み立てる。
    fn cross_logo_data(w: usize, h: usize, imgw: i32, imgh: i32, imgx: i32, imgy: i32) -> LogoData {
        let mut a = vec![1.0f32; w * h];
        let mut b = vec![0.0f32; w * h];
        let (cx, cy) = (w / 2, h / 2);
        let (a_v, b_v) = alpha_ab(0.5, 255.0);
        let (a_h, b_h) = alpha_ab(0.5, 80.0);
        for y in 0..h {
            for x in 0..w {
                let on_vertical = x == cx || x + 1 == cx;
                let on_horizontal = y == cy || y + 1 == cy;
                if on_vertical {
                    a[y * w + x] = a_v;
                    b[y * w + x] = b_v;
                } else if on_horizontal {
                    a[y * w + x] = a_h;
                    b[y * w + x] = b_h;
                }
            }
        }
        // クロマ平面は識別変換(a=1, b=0)で埋める。`scan::run`がクロマを学習しない
        // ときと同じ値(`scan.rs`モジュールdoc comment「クロマ平面は学習しない」)。
        // `lgd::parse`はw/h/log_uv_x/log_uv_yからクロマ平面の要素数(wUV*hUV)を
        // 計算するため、空のままだと`.lgd`の書き出し↔読み込みの往復でサイズが
        // 合わずエラーになる。
        let (wuv, huv) = (w >> 1, h >> 1);
        LogoData {
            w: w as i32,
            h: h as i32,
            log_uv_x: 1,
            log_uv_y: 1,
            imgw,
            imgh,
            imgx,
            imgy,
            name: String::new(),
            service_id: scan::UNSPECIFIED_SERVICE_ID,
            a_y: a,
            b_y: b,
            a_u: vec![1.0f32; wuv * huv],
            b_u: vec![0.0f32; wuv * huv],
            a_v: vec![1.0f32; wuv * huv],
            b_v: vec![0.0f32; wuv * huv],
        }
    }

    /// `a`/`b`（`w*h`要素）から、背景の明るさ`level`にロゴを合成した`w*h`バイトの
    /// 画像を作る（`score.rs`の`synthesize`+`to_u8`相当をテスト用に再現）。
    fn synthesize_u8(a: &[f32], b: &[f32], level: f32) -> Vec<u8> {
        a.iter()
            .zip(b)
            .map(|(&ai, &bi)| {
                let v = if ai > 0.0 {
                    (level - bi * 255.0) / ai
                } else {
                    level
                };
                v.round().clamp(0.0, 255.0) as u8
            })
            .collect()
    }

    #[test]
    fn select_candidate_rejects_frame_length_mismatch_from_stream_keyframes() {
        let dict_dir = make_scratch_dir("dict-select-length-mismatch");
        let video_size = VideoSize {
            width: 40,
            height: 20,
        };
        let logo = cross_logo_data(16, 16, 40, 20, 0, 0);
        scan::write_lgd(&logo, &dict_dir.join("station.lgd")).expect("write logo");

        // video_size(40x20=800バイト)と食い違う長さのフレームを渡す。
        let err = select_candidate(&dict_dir, video_size, 3600.0, |on_frame| {
            let wrong_length_frame = vec![0u8; 10];
            on_frame(&wrong_length_frame)?;
            Ok(1)
        })
        .expect_err("フレーム長がvideo_sizeと食い違うのでエラーになるはず");

        let message = err.to_string();
        assert!(
            message.contains("video_size"),
            "video_sizeとの食い違いを示すメッセージのはず: {message}"
        );

        fs::remove_dir_all(&dict_dir).ok();
    }

    #[test]
    fn select_candidate_picks_the_candidate_whose_rect_matches_the_true_overlay_position() {
        let dict_dir = make_scratch_dir("dict-select-candidate");
        let video_size = VideoSize {
            width: 40,
            height: 20,
        };
        let (w, h) = (16usize, 16usize);

        // 候補A: 矩形(0,0,16,16)。候補B: 矩形(20,0,16,16)。同じ十字ロゴだが
        // 位置(imgx/imgy)が異なる。全画面フレームの左側(x=0..16)にだけ実際に
        // 十字ロゴを合成し、右側(x=20..36)は無地(128)のままにする。矩形の
        // 切り出しオフセットを取り違えると、候補Bも誤って候補Aの領域を読んで
        // しまい、実際には無地なのに検出してしまう(このテストが検出したい回帰)。
        let logo_a = cross_logo_data(
            w,
            h,
            video_size.width as i32,
            video_size.height as i32,
            0,
            0,
        );
        let logo_b = cross_logo_data(
            w,
            h,
            video_size.width as i32,
            video_size.height as i32,
            20,
            0,
        );
        let path_a = dict_dir.join("station_a.lgd");
        let path_b = dict_dir.join("station_b.lgd");
        scan::write_lgd(&logo_a, &path_a).expect("write logo a");
        scan::write_lgd(&logo_b, &path_b).expect("write logo b");

        let (frame_w, frame_h) = (video_size.width as usize, video_size.height as usize);
        let levels = [24.0f32, 50.0f32];
        let mut call_count = 0u32;

        let result = select_candidate(&dict_dir, video_size, 3600.0, |on_frame| {
            call_count += 1;
            for &level in &levels {
                let overlay = synthesize_u8(&logo_a.a_y, &logo_a.b_y, level);
                let mut frame = vec![128u8; frame_w * frame_h];
                for ry in 0..h {
                    for rx in 0..w {
                        frame[ry * frame_w + rx] = overlay[ry * w + rx];
                    }
                }
                on_frame(&frame)?;
            }
            Ok(levels.len() as u64)
        })
        .expect("select_candidate は成功するはず");

        assert_eq!(
            call_count, 1,
            "stream_keyframes は1回だけ呼ばれるはず(全画面を1回読んで候補ごとに切り出す)"
        );

        let selection = result.expect("検出割合が閾値を超える候補(候補A)が選ばれるはず");
        assert_eq!(
            selection.path, path_a,
            "実際にオーバーレイがある位置(候補A)が選ばれるはず。候補Bが選ばれた場合は\
             矩形の切り出しオフセットを取り違えている疑いがある"
        );
        assert!(
            selection.detected_fraction > 0.0,
            "候補Aは少なくとも一部のフレームで検出されるはず: {}",
            selection.detected_fraction
        );

        fs::remove_dir_all(&dict_dir).ok();
    }
}
