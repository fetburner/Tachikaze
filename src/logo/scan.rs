//! `make-logo` サブコマンドの学習アルゴリズム本体（E14-6）。
//!
//! 入力 mp4 とロゴ矩形から、矩形の外周が単色になっているフレームだけを使って
//! 画素ごとの係数 `a`（傾き）/`b`（切片）を最小二乗で求め、`.lgd`（[`super::lgd`]）
//! として書き出す。
//!
//! ## 重要: MakKi 氏 delogo 由来のコードは訳さない
//!
//! Amatsukaze の `LogoScan::AddFrame` 系（`GetAB()` / `med_average()` /
//! `approxim_line()`）はライセンス不明（MakKi 氏の delogo 由来、配布物・GitHub の
//! いずれにも LICENSE 表記が無い）。**このモジュールはそのコードを一切参照・翻訳
//! していない。** 下記「学習アルゴリズム」節に書いた式（最小二乗の回帰直線、
//! 「ソートして中央半分だけ平均する」という統計処理）は数式そのものであり、
//! 特定の実装から借りる必要が無いため、issue 本文の記述から自分で書き下ろした。
//!
//! ## 学習アルゴリズム
//!
//! 1. 矩形の**外周 1 ピクセル**（[`border_pixel_indices`]）を集め、最小値と最大値の
//!    差が閾値（既定 [`DEFAULT_THRESHOLD`]）を超えるフレームは「ロゴの外側に映像が
//!    写っている」とみなして捨てる（[`estimate_frame_background`]）。
//! 2. 残ったフレームで、外周の値を昇順に並べて中央半分だけを平均し、そのフレームの
//!    背景色（スカラー1個）とする。
//! 3. 画素ごとに (前景 = そのフレームでの観測値, 背景 = 2 の値) の対を全フレーム分
//!    溜め、最小二乗で回帰直線を引いて係数を得る。回帰は観測値→背景の向きと
//!    背景→観測値の向きの両方で行い、平均する（傾き `(A1 + 1/A2) / 2`、切片
//!    `(B1 - B2/A2) / 2`。[`FrameLearner::finish`]）。
//! 4. 有効フレーム数が[`MIN_USABLE_FRAMES`]未満、または係数が NaN / inf / `a == 0`
//!    になったら失敗させる（[`ScanError::TooFewUsableFrames`] /
//!    [`ScanError::InvalidCoefficient`]）。
//!
//! `.lgd`（[`lgd::LogoData`]）の `aY`/`bY` は「`background = a * observed + b * maxv`」
//! という関係にある（`lgd.rs` の doc comment参照。`maxv` は 8bit なら 255）。上の
//! 手順3の回帰は生のピクセル値（0〜255）で行うため、切片は最後に `maxv` で割って
//! 格納する（[`FrameLearner::finish`] 内）。
//!
//! ## クロマ平面は学習しない
//!
//! フレーム供給（[`super::frames::stream_luma_frames`]）は輝度平面のみを流す
//! （E14-5）。そのため `aU`/`bU`/`aV`/`bV` は学習せず、恒等変換（`a=1, b=0`、
//! つまり「観測値をそのまま背景とみなす」= クロマ側の補正を無効化する値）で
//! 埋める。これは最小実装としての判断で、クロマの学習が必要になった場合は
//! 別途フレーム供給を拡張する必要がある。
//!
//! ## ベース部（AviUtl 互換部分）をゼロ埋めする理由
//!
//! `.lgd` のベース部 `LOGO_PIXEL` を実データで埋めるには Amatsukaze の
//! `ToOutLGP()` 相当の処理が要るが、これも上記と同じ理由（ライセンス不明）で
//! 移植しない。ベース部のうち [`lgd`] の読み込みが実際に検証するのは
//! `LOGO_HEADER.w`/`h`（Amatsukaze 独自部の `w`/`h` と食い違うと
//! `BaseExtendedSizeMismatch` で読み込みが失敗する）だけなので、[`build_lgd_bytes`]
//! はこの2値だけ実際の値を書き、他（`LOGO_PIXEL` を含む）はゼロ埋めにする。
//! 結果として AviUtl や logoframe からは使えないファイルになるが、本ツールは
//! それらとの互換性を保証しない方針（issue #95 で決定済み）。

use std::fs;
use std::path::Path;

use anyhow::Context;

use crate::errctx::PathContext as _;
use crate::logo::frames::{self, LogoRect, VideoSize};
use crate::logo::lgd::LogoData;

/// 単色判定の既定閾値（`--threshold` の既定値。CLI 側（`src/cli.rs`）と値を
/// 重複させないため、こちらを正とする）。
pub const DEFAULT_THRESHOLD: u8 = 12;

/// `LogoData::service_id` の既定値。対象サービス（放送局）を限定しないことを
/// 表すために本実装が独自に定義した値（Amatsukaze の内部表現を参照・翻訳した
/// ものではない）。読み込み側（[`lgd::parse`](super::lgd::parse)）はこの値を
/// 検証しない。
pub const UNSPECIFIED_SERVICE_ID: i32 = -1;

/// 8bit グレースケールの最大値。`.lgd` の `aY`/`bY` の意味（モジュール doc comment
/// 「学習アルゴリズム」節参照）における `maxv`。`stream_luma_frames` は常に
/// `-pix_fmt gray`（8bit）で読むため固定値でよい。
const MAXV: f64 = 255.0;

/// 学習に使う有効フレーム数の下限（issue #95「やること 3」: 「0件や極端に少ない
/// 場合は失敗させる」）。
///
/// 画素ごとの回帰は観測値(X)側と背景(Y)側それぞれの分散（`denom_x`/`denom_y`）で
/// 割るため、2〜3点程度でも分散が偶然0にならなければ有限値が出てしまい、
/// [`ScanError::InvalidCoefficient`] の NaN/inf 検査では捕まらない
/// （実際に有効3フレームで `.lgd` が黙って書き出される事例を確認済み）。
/// 4点あれば1次式1本を引いた後に自由度が2残る（他の統計処理と同様、経験的な
/// 「小標本すぎる」の目安として最小限の値を採る。これより小さくすべき理由がある
/// と分かれば値を見直す）。
const MIN_USABLE_FRAMES: u64 = 4;

/// クロマの間引き（4:2:0 前提、log2）。CLAUDE.md「前提」の「映像は H.264」を
/// 前提にした値で、対象素材（地上波録画）は 8bit 4:2:0 を想定している。矩形は
/// 呼び出し側が2の倍数に丸める（[`round_rect_to_even`]）ため、この値で割り切れる。
const LOG_UV: i32 = 1;

/// `.lgd` の `LogoHeader`（Amatsukaze 独自部）の合計バイト数。`lgd.rs::LOGO_HEADER_LEN`
/// と同じ値だが、そちらは非公開なのでここで独立に定義する（同じ二進形式の仕様
/// （lgd.rs のモジュール doc comment）から、読み込み側とは独立に書き下ろしたもの。
/// 値が食い違えば往復テスト（`round_trips_through_lgd_parse`）が失敗する）。
const LOGO_HEADER_LEN: usize = 540;
/// `LogoHeader.magic` に入れる値。`lgd.rs::MAGIC` と同じ値（上記と同じ理由で
/// 独立定義）。
const MAGIC: u32 = 0x0001_2345;
/// `LogoHeader.name` のバイト数。
const NAME_LEN: usize = 255;

/// 矩形を2の倍数に丸める。クロマの間引きに合わせるため（Amatsukaze 原典も
/// `imgx`/`imgy`/`w`/`h` をすべて2の倍数にしている、issue #95）。切り下げ
/// （`v & !1`）で丸める。
///
/// 戻り値の2番目は、丸めが実際に発生した場合にだけ `Some` になる通知用の文言
/// （呼び出し側が stderr へ出す。呼び出し側の責務にすることで、この関数自体は
/// 副作用を持たない）。
pub fn round_rect_to_even(rect: LogoRect) -> (LogoRect, Option<String>) {
    let rounded = LogoRect {
        x: rect.x & !1,
        y: rect.y & !1,
        w: rect.w & !1,
        h: rect.h & !1,
    };
    if rounded == rect {
        return (rounded, None);
    }
    let message = format!(
        "矩形を2の倍数に丸めました: (x={}, y={}, w={}, h={}) -> (x={}, y={}, w={}, h={})",
        rect.x, rect.y, rect.w, rect.h, rounded.x, rounded.y, rounded.w, rounded.h
    );
    (rounded, Some(message))
}

/// 矩形（`w`×`h`）の外周1ピクセルの、行優先フレームバッファに対するインデックス
/// 一覧を返す。上下の行を丸ごと、左右は上下の行と重複しない範囲（`1..h-1`）だけ
/// 集める（`w`/`h` が小さいほど「外周」が画像全体に近づくが、重複は生まれない）。
fn border_pixel_indices(w: usize, h: usize) -> Vec<usize> {
    let mut idx = Vec::new();
    if w == 0 || h == 0 {
        return idx;
    }
    for x in 0..w {
        idx.push(x);
    }
    if h > 1 {
        for x in 0..w {
            idx.push((h - 1) * w + x);
        }
    }
    if h > 2 {
        for y in 1..h - 1 {
            idx.push(y * w);
            if w > 1 {
                idx.push(y * w + (w - 1));
            }
        }
    }
    idx
}

/// 1フレーム分の外周ピクセル値から、このフレームを学習に使えるか判定する。
///
/// 外周の最小値・最大値の差が `threshold` を超えるフレームは `None`（ロゴの外側に
/// 映像が写っているとみなして捨てる）。使える場合は、外周の値を昇順に並べた
/// **中央半分**の平均を背景色として返す（モジュール doc comment「学習アルゴリズム」
/// 手順2）。
fn estimate_frame_background(frame: &[u8], border: &[usize], threshold: u8) -> Option<f64> {
    let mut values: Vec<u8> = border.iter().map(|&i| frame[i]).collect();
    if values.is_empty() {
        return None;
    }
    let (min, max) = values
        .iter()
        .fold((u8::MAX, u8::MIN), |(mn, mx), &v| (mn.min(v), mx.max(v)));
    if max - min > threshold {
        return None;
    }

    values.sort_unstable();
    let len = values.len();
    let quarter = len / 4;
    // `len >= 1`（上の `values.is_empty()` チェック済み）なら `quarter < len - quarter`
    // は常に真（`quarter = len/4` は切り捨てなので、`len - quarter` は必ずそれより
    // 大きい）。中央半分の範囲が空になることは無い。
    debug_assert!(quarter < len - quarter);
    let (start, end) = (quarter, len - quarter);
    let mid = &values[start..end];
    let sum: u64 = mid.iter().map(|&v| v as u64).sum();
    Some(sum as f64 / mid.len() as f64)
}

/// 学習に失敗したことを表すエラー。
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ScanError {
    /// 単色判定を通ったフレームが0件だった。
    NoUsableFrames { total_frames: u64 },
    /// 単色判定を通ったフレームが [`MIN_USABLE_FRAMES`] 未満だった（0件より多いが
    /// 「極端に少ない」場合、issue #95「やること 3」）。回帰係数の NaN/inf 検査
    /// （[`ScanError::InvalidCoefficient`]）は少数点でも分散が0にならなければ
    /// 素通りするため、件数そのものを別に検査する。
    TooFewUsableFrames { used_frames: u64, total_frames: u64 },
    /// 画素ごとの回帰係数が NaN / inf / `a == 0` になった（モジュール doc comment
    /// 「学習アルゴリズム」手順4）。原典が `"Insufficient logo frames"` で失敗させて
    /// いるのと同じ状況（フレームが足りない、または背景の明るさがほとんど変化して
    /// いない）。
    InvalidCoefficient {
        pixel_index: usize,
        used_frames: u64,
        a: f64,
        b: f64,
    },
}

impl std::fmt::Display for ScanError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ScanError::NoUsableFrames { total_frames } => write!(
                f,
                "単色判定を通った有効フレームが0件でした（全{total_frames}フレーム中）。\
                 ロゴ周りが単色になっているフレームが見つからないと学習できません \
                 （矩形の位置や --threshold を見直してください）。"
            ),
            ScanError::TooFewUsableFrames {
                used_frames,
                total_frames,
            } => write!(
                f,
                "単色判定を通った有効フレームが{used_frames}件しかありません（全{total_frames}\
                 フレーム中、下限は{MIN_USABLE_FRAMES}件）。件数が極端に少ないと、回帰係数が\
                 偶然有限値になり不正な検出に気づけないため、壊れたロゴデータを書き出さずに\
                 失敗させています（矩形の位置や --threshold を見直してください）。"
            ),
            ScanError::InvalidCoefficient {
                pixel_index,
                used_frames,
                a,
                b,
            } => write!(
                f,
                "画素{pixel_index}個目の回帰係数が不正です（a={a}, b={b}）。有効フレーム\
                 {used_frames}件では学習できません（フレームが足りない、または画素の観測値が\
                 全フレームで同一で分散が0になっている（完全不透明な画素・一様な背景など）\
                 可能性があります）。壊れたロゴデータを書き出さずに失敗させています。"
            ),
        }
    }
}

impl std::error::Error for ScanError {}

/// フレームを1枚ずつ受け取り、画素ごとの回帰係数を溜めるアキュムレータ。
///
/// 背景色（[`estimate_frame_background`]）はフレームごとにスカラー1個なので、
/// 背景側の統計量（`sum_y`/`sum_y2`）は画素ごとに持たず全体で1組だけ持つ
/// （観測値側の統計量 `sum_x`/`sum_x2`/`sum_xy` だけが画素ごとに必要）。
pub struct FrameLearner {
    w: usize,
    h: usize,
    threshold: u8,
    border: Vec<usize>,
    sum_x: Vec<f64>,
    sum_x2: Vec<f64>,
    sum_xy: Vec<f64>,
    sum_y: f64,
    sum_y2: f64,
    used_frames: u64,
    total_frames: u64,
}

impl FrameLearner {
    pub fn new(w: u32, h: u32, threshold: u8) -> Self {
        let w = w as usize;
        let h = h as usize;
        let n_pixels = w * h;
        FrameLearner {
            w,
            h,
            threshold,
            border: border_pixel_indices(w, h),
            sum_x: vec![0.0; n_pixels],
            sum_x2: vec![0.0; n_pixels],
            sum_xy: vec![0.0; n_pixels],
            sum_y: 0.0,
            sum_y2: 0.0,
            used_frames: 0,
            total_frames: 0,
        }
    }

    /// フレームを1枚追加する。`frame` は `w*h` バイトの輝度平面（`stream_luma_frames`
    /// が渡す形式そのまま）。
    ///
    /// 常に `Ok(())` を返す（`stream_luma_frames` の `on_frame` にそのまま渡せる形。
    /// 「フレームが足りない」判定は全フレームを読み終えた後 [`FrameLearner::finish`]
    /// で行う）。
    pub fn add_frame(&mut self, frame: &[u8]) -> anyhow::Result<()> {
        debug_assert_eq!(
            frame.len(),
            self.w * self.h,
            "frame のバイト数が w*h と不一致"
        );
        self.total_frames += 1;

        if let Some(bg) = estimate_frame_background(frame, &self.border, self.threshold) {
            self.sum_y += bg;
            self.sum_y2 += bg * bg;
            for (i, &v) in frame.iter().enumerate() {
                let x = v as f64;
                self.sum_x[i] += x;
                self.sum_x2[i] += x * x;
                self.sum_xy[i] += x * bg;
            }
            self.used_frames += 1;
        }
        Ok(())
    }

    pub fn used_frames(&self) -> u64 {
        self.used_frames
    }

    pub fn total_frames(&self) -> u64 {
        self.total_frames
    }

    /// 画素ごとの回帰係数 (`a_y`, `b_y`) を確定する。`self` を消費しない
    /// （呼び出し側が先に `used_frames`/`total_frames` を報告してから呼べるように
    /// するため）。
    pub fn finish(&self) -> Result<(Vec<f32>, Vec<f32>), ScanError> {
        if self.used_frames == 0 {
            return Err(ScanError::NoUsableFrames {
                total_frames: self.total_frames,
            });
        }
        if self.used_frames < MIN_USABLE_FRAMES {
            return Err(ScanError::TooFewUsableFrames {
                used_frames: self.used_frames,
                total_frames: self.total_frames,
            });
        }

        let n = self.used_frames as f64;
        let mean_y = self.sum_y / n;
        let mean_y2 = self.sum_y2 / n;
        let denom_y = mean_y2 - mean_y * mean_y;

        let mut a_y = Vec::with_capacity(self.sum_x.len());
        let mut b_y = Vec::with_capacity(self.sum_x.len());

        for i in 0..self.sum_x.len() {
            let mean_x = self.sum_x[i] / n;
            let mean_x2 = self.sum_x2[i] / n;
            let mean_xy = self.sum_xy[i] / n;
            let denom_x = mean_x2 - mean_x * mean_x;

            // A1: 背景(Y) = A1*観測値(X) + B1 の回帰。
            let a1 = (mean_xy - mean_x * mean_y) / denom_x;
            let b1 = mean_y - a1 * mean_x;

            // A2: 観測値(X) = A2*背景(Y) + B2 の回帰（XとYを入れ替えた回帰）。
            let a2 = (mean_xy - mean_x * mean_y) / denom_y;
            let b2 = mean_x - a2 * mean_y;

            // 2本の回帰直線を「背景 = a*観測値 + b_raw」の形に揃えて平均する
            // （モジュール doc comment「学習アルゴリズム」手順3）。
            let a = (a1 + 1.0 / a2) / 2.0;
            let b_raw = (b1 - b2 / a2) / 2.0;
            let b = b_raw / MAXV;

            if !a.is_finite() || !b.is_finite() || a == 0.0 {
                return Err(ScanError::InvalidCoefficient {
                    pixel_index: i,
                    used_frames: self.used_frames,
                    a,
                    b,
                });
            }
            a_y.push(a as f32);
            b_y.push(b as f32);
        }

        Ok((a_y, b_y))
    }
}

/// `make-logo` パイプライン本体（ffmpeg 起動を含む）に必要な設定。
pub struct MakeLogoConfig {
    /// `ffmpeg` の実行ファイルパス（`tools::resolve_tool(tools::FFMPEG)`）。
    pub ffmpeg: std::path::PathBuf,
    pub input: std::path::PathBuf,
    /// `stream_luma_frames` に渡す作業ディレクトリ（`external::spawn_streaming` の
    /// `current_dir`。入力は絶対パス化されるため、実質どこでもよい）。
    pub cwd: std::path::PathBuf,
    /// 既に2の倍数に丸め済みの矩形（[`round_rect_to_even`] は呼び出し側が事前に
    /// 呼ぶ。ここでは丸め済みであることを assert するだけ）。
    pub rect: LogoRect,
    pub video_size: VideoSize,
    /// `.dtvi` は使わない（make-logo は入力 mp4 と ffmpeg だけで完結させる方針、
    /// issue #95「解くべき問題」）。フレーム数は呼び出し側が mp4 のサンプル表
    /// （`mp4io::read::samples`）から数えて渡す。表示順・デコード順のどちらで
    /// 数えても総数は変わらないため、順序の区別は不要。
    pub frame_count: u64,
    pub threshold: u8,
    pub name: String,
    pub service_id: i32,
}

/// [`run`] の成果物。
pub struct MakeLogoOutput {
    pub logo: LogoData,
    pub used_frames: u64,
    pub total_frames: u64,
}

/// `make-logo` パイプライン本体: ffmpeg でロゴ矩形の輝度平面を流し、
/// [`FrameLearner`] で学習し、[`LogoData`] を組み立てる。
///
/// 有効フレーム数（`used_frames`/`total_frames`）は、学習が失敗する場合でも
/// 必ず stderr に出す（issue #95「罠」: 何フレーム使ったか表示しないと
/// 「学習できていない」ことに気付けない）。そのため、フレームを読み終えた
/// 直後にここで出力し、その後の [`FrameLearner::finish`] の成否に関わらず
/// この行は必ず出る。
pub fn run(config: &MakeLogoConfig) -> anyhow::Result<MakeLogoOutput> {
    anyhow::ensure!(
        config.rect.w.is_multiple_of(2) && config.rect.h.is_multiple_of(2),
        "ロゴ矩形の w/h は2の倍数である必要があります（クロマの間引きに合わせるため、\
         round_rect_to_even で丸めてから渡すこと）: w={}, h={}",
        config.rect.w,
        config.rect.h
    );

    let mut learner = FrameLearner::new(config.rect.w, config.rect.h, config.threshold);
    frames::stream_luma_frames(
        &config.ffmpeg,
        &config.input,
        &config.cwd,
        config.rect,
        config.video_size,
        config.frame_count,
        |frame| learner.add_frame(frame),
    )?;

    eprintln!(
        "[make-logo] 有効フレーム: {}/{}",
        learner.used_frames(),
        learner.total_frames()
    );

    let (a_y, b_y) = learner
        .finish()
        .map_err(|err| anyhow::anyhow!("{err}"))
        .context("ロゴの学習に失敗しました")?;

    let wuv = (config.rect.w >> LOG_UV) as usize;
    let huv = (config.rect.h >> LOG_UV) as usize;
    let uv_count = wuv * huv;

    // クロマ平面は学習しない（モジュール doc comment「クロマ平面は学習しない」節）。
    // a=1, b=0 は「観測値をそのまま背景とみなす」恒等変換で、クロマ側の補正を
    // 無効化する値。
    let a_u = vec![1.0f32; uv_count];
    let b_u = vec![0.0f32; uv_count];
    let a_v = vec![1.0f32; uv_count];
    let b_v = vec![0.0f32; uv_count];

    let logo = LogoData {
        w: config.rect.w as i32,
        h: config.rect.h as i32,
        log_uv_x: LOG_UV,
        log_uv_y: LOG_UV,
        imgw: config.video_size.width as i32,
        imgh: config.video_size.height as i32,
        imgx: config.rect.x as i32,
        imgy: config.rect.y as i32,
        name: config.name.clone(),
        service_id: config.service_id,
        a_y,
        b_y,
        a_u,
        b_u,
        a_v,
        b_v,
    };

    Ok(MakeLogoOutput {
        logo,
        used_frames: learner.used_frames(),
        total_frames: learner.total_frames(),
    })
}

/// `name` を `.lgd` の `name[255]`（NUL 終端、UTF-8）に収まるよう、マルチバイト
/// 文字の境界を壊さずに切り詰める。
fn truncate_name_for_lgd(name: &str) -> &str {
    if name.len() <= NAME_LEN {
        return name;
    }
    let mut end = NAME_LEN;
    while end > 0 && !name.is_char_boundary(end) {
        end -= 1;
    }
    &name[..end]
}

/// ベース部（AviUtl 互換部分）を組み立てる。`LOGO_PIXEL` を含めゼロ埋めでよい理由は
/// モジュール doc comment「ベース部（AviUtl 互換部分）をゼロ埋めする理由」参照。
/// `LOGO_HEADER.w`/`h` だけは読み込み側（`lgd::parse`）が Amatsukaze 独自部と
/// 食い違いを検査するため、実際の値を書く。
fn build_base_part(w: i32, h: i32) -> anyhow::Result<Vec<u8>> {
    let base_w = i16::try_from(w).context("ロゴ矩形の w が i16 の範囲を超えています")?;
    let base_h = i16::try_from(h).context("ロゴ矩形の h が i16 の範囲を超えています")?;

    let mut buf = Vec::new();
    buf.extend_from_slice(&[0u8; 28]); // str[28]（ゼロ埋め）
    buf.extend_from_slice(&1u32.to_be_bytes()); // logonum（BE、未使用のため固定で1）
    buf.extend_from_slice(&[0u8; 32]); // LOGO_HEADER.name[32]
    buf.extend_from_slice(&0i16.to_le_bytes()); // x
    buf.extend_from_slice(&0i16.to_le_bytes()); // y
    buf.extend_from_slice(&base_h.to_le_bytes()); // h
    buf.extend_from_slice(&base_w.to_le_bytes()); // w
    buf.extend_from_slice(&0i16.to_le_bytes()); // fi
    buf.extend_from_slice(&0i16.to_le_bytes()); // fo
    buf.extend_from_slice(&0i16.to_le_bytes()); // st
    buf.extend_from_slice(&0i16.to_le_bytes()); // ed

    let pixel_count = (base_w.max(0) as usize) * (base_h.max(0) as usize);
    buf.extend_from_slice(&vec![0u8; pixel_count * 12]); // LOGO_PIXEL[h*w]（ゼロ埋め）
    Ok(buf)
}

/// Amatsukaze 独自部（`LogoHeader` + float 部）を組み立てる。
fn build_extended_part(logo: &LogoData) -> Vec<u8> {
    let mut buf = Vec::new();
    buf.extend_from_slice(&MAGIC.to_le_bytes());
    buf.extend_from_slice(&1i32.to_le_bytes()); // version
    buf.extend_from_slice(&logo.w.to_le_bytes());
    buf.extend_from_slice(&logo.h.to_le_bytes());
    buf.extend_from_slice(&logo.log_uv_x.to_le_bytes());
    buf.extend_from_slice(&logo.log_uv_y.to_le_bytes());
    buf.extend_from_slice(&logo.imgw.to_le_bytes());
    buf.extend_from_slice(&logo.imgh.to_le_bytes());
    buf.extend_from_slice(&logo.imgx.to_le_bytes());
    buf.extend_from_slice(&logo.imgy.to_le_bytes());
    debug_assert_eq!(buf.len(), 40);

    let mut name_field = [0u8; NAME_LEN];
    let name = truncate_name_for_lgd(&logo.name);
    name_field[..name.len()].copy_from_slice(name.as_bytes());
    buf.extend_from_slice(&name_field);
    buf.push(0); // name[255] 直後のパディング1バイト（lgd.rs「罠」参照）
    debug_assert_eq!(buf.len(), 296);

    buf.extend_from_slice(&logo.service_id.to_le_bytes());
    buf.extend_from_slice(&[0u8; 240]); // reserved[60]
    debug_assert_eq!(buf.len(), LOGO_HEADER_LEN);

    for plane in [
        &logo.a_y, &logo.b_y, &logo.a_u, &logo.b_u, &logo.a_v, &logo.b_v,
    ] {
        for v in plane {
            buf.extend_from_slice(&v.to_le_bytes());
        }
    }
    buf
}

/// `.lgd` の全バイト列を組み立てる（ファイル I/O を含まない純粋な関数、単体テスト用）。
pub fn build_lgd_bytes(logo: &LogoData) -> anyhow::Result<Vec<u8>> {
    let mut buf = build_base_part(logo.w, logo.h)?;
    buf.extend_from_slice(&build_extended_part(logo));
    Ok(buf)
}

/// `.lgd` をパスに書き出す。
pub fn write_lgd(logo: &LogoData, path: &Path) -> anyhow::Result<()> {
    let bytes = build_lgd_bytes(logo)?;
    fs::write(path, bytes).path_ctx(".lgd の書き出し", path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logo::lgd;

    // ---------------------------------------------------------------
    // round_rect_to_even
    // ---------------------------------------------------------------

    #[test]
    fn even_rect_is_unchanged_and_not_reported() {
        let rect = LogoRect {
            x: 10,
            y: 20,
            w: 100,
            h: 40,
        };
        let (rounded, message) = round_rect_to_even(rect);
        assert_eq!(rounded, rect);
        assert_eq!(message, None);
    }

    #[test]
    fn odd_rect_is_rounded_down_and_reported() {
        let rect = LogoRect {
            x: 11,
            y: 21,
            w: 101,
            h: 41,
        };
        let (rounded, message) = round_rect_to_even(rect);
        assert_eq!(
            rounded,
            LogoRect {
                x: 10,
                y: 20,
                w: 100,
                h: 40
            }
        );
        let message = message.expect("丸めが発生したので通知があるはず");
        assert!(message.contains("11"), "message={message}");
        assert!(message.contains("10"), "message={message}");
    }

    // ---------------------------------------------------------------
    // FrameLearner: 既知の a, b で合成したフレーム列からの回帰
    // ---------------------------------------------------------------

    /// `w`×`h` の合成フレームを1枚作る。外周は `bg` そのもの（=そのフレームの
    /// 背景がそのまま見えている前提）、内側（矩形の中でロゴが乗っている部分）は
    /// 「`bg = a*observed + b*maxv`」の逆算式から求めた観測値にする。
    fn synth_frame(w: usize, h: usize, bg: f64, a: f64, b: f64) -> Vec<u8> {
        let observed_inside = ((bg - b * MAXV) / a).round().clamp(0.0, 255.0) as u8;
        let bg_u8 = bg.round().clamp(0.0, 255.0) as u8;
        let border: std::collections::HashSet<usize> =
            border_pixel_indices(w, h).into_iter().collect();
        (0..w * h)
            .map(|i| {
                if border.contains(&i) {
                    bg_u8
                } else {
                    observed_inside
                }
            })
            .collect()
    }

    #[test]
    fn recovers_known_coefficients_from_synthetic_frames() {
        let (w, h) = (6, 4);
        let a_true = 1.5;
        let b_true = 0.02;
        let backgrounds = [60.0, 90.0, 120.0, 150.0, 180.0, 210.0];

        let mut learner = FrameLearner::new(w as u32, h as u32, DEFAULT_THRESHOLD);
        for &bg in &backgrounds {
            let frame = synth_frame(w, h, bg, a_true, b_true);
            learner.add_frame(&frame).expect("add_frame は常に成功する");
        }

        assert_eq!(learner.total_frames(), backgrounds.len() as u64);
        assert_eq!(learner.used_frames(), backgrounds.len() as u64);

        let (a_y, b_y) = learner
            .finish()
            .expect("十分なフレームがあるので成功するはず");

        // 内側（ロゴが乗っている画素）で係数を確認する。境界の画素は
        // 「観測値=背景」という別の(自明な a=1,b=0 に近い)関係になるため対象外。
        let border: std::collections::HashSet<usize> =
            border_pixel_indices(w, h).into_iter().collect();
        for i in 0..w * h {
            if border.contains(&i) {
                continue;
            }
            assert!(
                (a_y[i] as f64 - a_true).abs() < 0.02,
                "pixel {i}: a_y={} (true={a_true})",
                a_y[i]
            );
            assert!(
                (b_y[i] as f64 - b_true).abs() < 0.01,
                "pixel {i}: b_y={} (true={b_true})",
                b_y[i]
            );
        }
    }

    #[test]
    fn zero_usable_frames_is_an_error() {
        let (w, h) = (6, 4);
        let mut learner = FrameLearner::new(w as u32, h as u32, DEFAULT_THRESHOLD);

        // 外周の最小値・最大値の差が閾値を超える(単色でない)フレームだけを与える。
        for _ in 0..5 {
            let mut frame = vec![128u8; w * h];
            // 外周の一部だけ極端な値にして単色判定を必ず落とす。
            frame[0] = 0;
            frame[1] = 255;
            learner.add_frame(&frame).expect("add_frame は常に成功する");
        }

        assert_eq!(learner.used_frames(), 0);
        let err = learner
            .finish()
            .expect_err("有効フレームが0件なのでエラーになるはず");
        assert_eq!(err, ScanError::NoUsableFrames { total_frames: 5 });
    }

    #[test]
    fn too_few_usable_frames_is_an_error_even_with_finite_coefficients() {
        // 有効フレームが極端に少ない（ただし0件ではない）場合。issue #95
        // 「やること 3」（有効フレームが0件や極端に少ない場合は失敗させる）の
        // 「極端に少ない」側を検査する。背景を変化させているため回帰係数自体は
        // 有限値になり得るが、MIN_USABLE_FRAMES 未満なので
        // InvalidCoefficient（NaN/inf 検査）では捕まらないことを確認する。
        let (w, h) = (6, 4);
        let backgrounds = [60.0, 120.0, 180.0];
        assert!((backgrounds.len() as u64) < MIN_USABLE_FRAMES);

        let mut learner = FrameLearner::new(w as u32, h as u32, DEFAULT_THRESHOLD);
        for &bg in &backgrounds {
            let frame = synth_frame(w, h, bg, 1.5, 0.02);
            learner.add_frame(&frame).expect("add_frame は常に成功する");
        }

        assert_eq!(learner.used_frames(), backgrounds.len() as u64);
        let err = learner
            .finish()
            .expect_err("有効フレームが下限未満なのでエラーになるはず");
        assert_eq!(
            err,
            ScanError::TooFewUsableFrames {
                used_frames: backgrounds.len() as u64,
                total_frames: backgrounds.len() as u64,
            }
        );
    }

    #[test]
    fn constant_background_yields_non_finite_coefficient_error() {
        let (w, h) = (6, 4);
        let mut learner = FrameLearner::new(w as u32, h as u32, DEFAULT_THRESHOLD);

        // 背景の明るさが全フレームで同一だと、観測値→背景の回帰の分散が0になり
        // 係数が NaN/inf になる（モジュール doc comment「学習アルゴリズム」手順4）。
        for _ in 0..5 {
            let frame = synth_frame(w, h, 128.0, 1.5, 0.02);
            learner.add_frame(&frame).expect("add_frame は常に成功する");
        }

        assert_eq!(learner.used_frames(), 5);
        let err = learner
            .finish()
            .expect_err("背景が変化しないので係数が不正になり失敗するはず");
        match err {
            ScanError::InvalidCoefficient { a, b, .. } => {
                assert!(!a.is_finite() || !b.is_finite() || a == 0.0, "a={a}, b={b}");
            }
            other => panic!("InvalidCoefficient を期待したが {other:?}"),
        }
    }

    // ---------------------------------------------------------------
    // .lgd の書き出し ↔ lgd::parse の往復
    // ---------------------------------------------------------------

    fn sample_logo() -> LogoData {
        LogoData {
            w: 4,
            h: 2,
            log_uv_x: 1,
            log_uv_y: 1,
            imgw: 640,
            imgh: 360,
            imgx: 620,
            imgy: 4,
            name: "テストロゴ".to_string(),
            service_id: UNSPECIFIED_SERVICE_ID,
            a_y: vec![1.1, 1.2, 1.3, 1.4, 1.5, 1.6, 1.7, 1.8],
            b_y: vec![0.01, 0.02, 0.03, 0.04, 0.05, 0.06, 0.07, 0.08],
            a_u: vec![1.0, 1.0],
            b_u: vec![0.0, 0.0],
            a_v: vec![1.0, 1.0],
            b_v: vec![0.0, 0.0],
        }
    }

    #[test]
    fn round_trips_through_lgd_parse() {
        let logo = sample_logo();
        let bytes = build_lgd_bytes(&logo).expect("組み立てに失敗しないはず");

        let parsed = lgd::parse(&bytes).expect(".lgd としてパースできるはず");
        assert_eq!(parsed, logo);
    }

    #[test]
    fn long_name_is_truncated_at_utf8_boundary_not_mid_character() {
        // 3バイト文字(日本語)を88回繰り返すと264バイトになり、255バイトの
        // name[255] に収まらない。文字境界を壊さずに切り詰められることを確認する。
        let mut logo = sample_logo();
        logo.name = "あ".repeat(88);
        assert!(logo.name.len() > NAME_LEN);

        let bytes = build_lgd_bytes(&logo).expect("組み立てに失敗しないはず");
        let parsed = lgd::parse(&bytes).expect(".lgd としてパースできるはず（パニックしない）");
        assert!(parsed.name.len() <= NAME_LEN);
        // 切り詰めた結果が元の文字列の先頭に一致する(文字境界で切れている)こと。
        assert!(logo.name.starts_with(&parsed.name));
    }

    #[test]
    fn oversized_w_is_a_clean_error_not_a_panic() {
        let mut logo = sample_logo();
        logo.w = i32::MAX;
        let err = build_lgd_bytes(&logo).expect_err("i16 の範囲を超えるのでエラーになるはず");
        assert!(err.to_string().contains("w"), "err={err}");
    }
}
