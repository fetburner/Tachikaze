//! ロゴ相関スコア（`corr0` / `corr1`）の計算。
//!
//! ## 由来
//!
//! Amatsukaze（<https://github.com/nekopanda/Amatsukaze>）`LogoScan.hpp` の
//! 相関方式を移植したもの。ライセンスは MIT、`Copyright (c) 2017-2019 Nekopanda`。
//!
//! 同ファイル内の `approxim_line()` / `GetAB()` / `med_average()` は MakKi 氏の
//! delogo 由来でライセンス不明のため参照していない。この issue（E14-4）で必要な
//! 計算はいずれもこの 3 関数に該当しない。
//!
//! ## 考え方
//!
//! ロゴとの相関を取りたいが、単純に画素を掛け合わせると画像背景の濃淡に影響
//! される。そこで**ロゴのエッジだけを、局所平均を引いてから相関**する。
//!
//! ### 準備（[`LogoMask::new`]、`maskratio = 0.35`）
//!
//! 1. 明るさ 32 段階（`c << 3`, `c = 0..31`）の均一グレー背景にロゴを合成した
//!    画像を作る。合成は逆算式 `Y = (Y - b * maxv) / a`（`a > 0` の画素のみ。
//!    それ以外は背景そのまま、[`synthesize`]）
//! 2. 中央の明るさ（`c = 16`）の画像で、各画素を中心とする 5x5 窓の分散を
//!    計算し、大きい順に `w*h*0.35` 個の画素を「着目点」にする
//! 3. 着目点ごとに 5x5 窓のコピーから平均を引いたカーネル（要素和 0、25 要素）
//!    を作る
//! 4. 着目点 × 明るさ 32 段階のそれぞれについて、その明るさの合成画像に対する
//!    相関値を計算し、`scale = 1/|相関|`（相関 0 なら 0）と
//!    `scale2 = min(1, |相関| / (平均相関 * 0.2))` を持つ。前者は「その明るさの
//!    単色背景なら相関が 1 になる」正規化。後者は相関が小さすぎる着目点の寄与を
//!    弱めるキャップで、「平均相関」は**同じ明るさ段階における着目点間の
//!    `|相関|` の平均**とした（issue の記述はこの軸を明示していないが、
//!    「相関が小さすぎる画素」を判定するには同じ照明条件の他の着目点と比べる
//!    のが自然なため）
//! 5. 明るさ 16（`c = 16 >> 3 = 2`）の画像に対するスコアを `black_score` として
//!    保持し、以後のスコアをこれで割って正規化する
//!
//! ### 1 フレームの評価（[`LogoMask::evaluate`]）
//!
//! - 相関計算: 5x5 窓の平均 `avg` を出し、`sum = Σ kernel[i] * (Y[i] - avg)`
//! - 正規化: `avg` の属する明るさ段階（`avg` を `0..255` にクランプしてから
//!   `>> 3` した段階）の `scale` を掛け、`[-1, 1]` にクランプし、`scale2` を
//!   掛けて着目点すべての和を取る
//! - ロゴ除去つき評価: `bg = a * src + b * maxv`、
//!   `dst = fade * bg + (1 - fade) * src` を計算した画像に対して上の相関を取り、
//!   `black_score` で割る
//! - **`corr0` = `fade = 0`**（そのまま）、**`corr1` = `fade = 1`**（ロゴを完全
//!   に除去）の 2 回評価する。ロゴが本当にあれば除去後の相関は 0 付近、無いのに
//!   除去すると画像にロゴ形の凹みが刻まれて負に振れる。この 2 つを使うことが
//!   誤検出対策の中心で、`corr0` 単独では背景の模様が偶然相関したときに
//!   誤検出する
//!
//! ## 原典との差
//!
//! 原典は評価前に必ず `DeintY`（垂直 1-2-1 のぼかし）をロゴ側・画像側の両方に
//! 掛ける。これはインターレース TS のフィールド構造由来のギザギザを均すための
//! 処理で、progressive な mp4 を前提にする本ツールでは掛けても得るものがなく、
//! 垂直方向の情報を無意味にぼかすだけになる。**掛けない実装にする。**

use crate::logo::lgd::LogoData;

/// 明るさの段階数（`c << 3`, `c = 0..LEVELS-1`）。
const LEVELS: usize = 32;
/// 着目点に選ぶ画素の割合。
const MASKRATIO: f64 = 0.35;
/// 8bit 輝度の最大値。
const MAXV: f32 = 255.0;
/// 5x5 窓の要素数。
const WINDOW: usize = 25;

/// `a`, `b`（`background = a * observed + b * maxv`）から、背景の明るさ
/// `level`（`0.0..=255.0` の均一グレー）にロゴを合成した画像を作る。
///
/// `a > 0` の画素だけ逆算式 `observed = (level - b * maxv) / a` を適用する。
/// それ以外（ロゴの影響が無い画素）は背景そのまま（`level`）とする。
fn synthesize(a: &[f32], b: &[f32], level: f32) -> Vec<f32> {
    a.iter()
        .zip(b)
        .map(|(&ai, &bi)| {
            if ai > 0.0 {
                (level - bi * MAXV) / ai
            } else {
                level
            }
        })
        .collect()
}

/// 幅 `w` の画像上で `(cx, cy)` を中心とする 5x5 窓のフラットインデックス。
///
/// 呼び出し前に `cx in 2..w-2`, `cy in 2..h-2` であることを前提にする
/// （このモジュール内では境界チェックを済ませた着目点にしか使わない）。
fn window_indices(w: usize, cx: usize, cy: usize) -> [usize; WINDOW] {
    let mut idx = [0usize; WINDOW];
    for dy in 0..5 {
        for dx in 0..5 {
            idx[dy * 5 + dx] = (cy - 2 + dy) * w + (cx - 2 + dx);
        }
    }
    idx
}

/// 窓内画素の平均。
fn window_mean(img: &[f32], idx: &[usize; WINDOW]) -> f32 {
    idx.iter().map(|&i| img[i]).sum::<f32>() / WINDOW as f32
}

/// 窓内画素の分散（平均は呼び出し側で計算済みのものを渡す）。
fn window_variance(img: &[f32], idx: &[usize; WINDOW], mean: f32) -> f32 {
    idx.iter()
        .map(|&i| {
            let d = img[i] - mean;
            d * d
        })
        .sum::<f32>()
        / WINDOW as f32
}

/// カーネル（局所平均を引いた 5x5 窓）と画像の相関 `Σ kernel[i] * (Y[i] - avg)`。
fn correlate(img: &[f32], idx: &[usize; WINDOW], kernel: &[f32; WINDOW], mean: f32) -> f32 {
    idx.iter()
        .zip(kernel)
        .map(|(&i, &k)| k * (img[i] - mean))
        .sum()
}

/// ロゴ 1 個ぶんの着目点。
#[derive(Debug, Clone)]
struct AttentionPoint {
    /// この着目点を中心とする 5x5 窓のフラットインデックス。
    idx: [usize; WINDOW],
    /// 局所平均を引いたカーネル（要素和 0）。
    kernel: [f32; WINDOW],
    /// 明るさ段階ごとの正規化係数（「その明るさの単色背景なら相関が 1」）。
    scale: [f32; LEVELS],
    /// 明るさ段階ごとのキャップ係数（相関が小さすぎる着目点の寄与を弱める）。
    scale2: [f32; LEVELS],
}

/// [`LogoData`] から作る、ロゴ相関スコアの計算に必要な状態一式。
///
/// マスク（着目点）・カーネル・明るさ別の正規化係数はロゴデータだけから決まるため
/// [`LogoMask::new`] で前計算しておき、フレームごとの評価（[`LogoMask::evaluate`]）
/// では再利用する。
#[derive(Debug, Clone)]
pub struct LogoMask {
    w: usize,
    h: usize,
    /// Y 平面の係数 `a`（`w*h` 要素、行優先）。
    a: Vec<f32>,
    /// Y 平面の係数 `b`（`w*h` 要素、行優先）。
    b: Vec<f32>,
    points: Vec<AttentionPoint>,
    /// 明るさ 16（`c = 2`）の合成画像に対するスコア。以後のスコアをこれで割る。
    black_score: f32,
}

impl LogoMask {
    /// [`LogoData`] の `a_y`/`b_y`/`w`/`h` から準備一式を作る。
    ///
    /// # Panics
    ///
    /// `w < 5` または `h < 5` の場合（5x5 窓の中心になれる画素が存在しない）。
    pub fn new(logo: &LogoData) -> LogoMask {
        let w = logo.w as usize;
        let h = logo.h as usize;
        assert!(
            w >= 5 && h >= 5,
            "LogoMask::new: w={w}, h={h} は 5x5 窓を置くには小さすぎる"
        );
        let a = logo.a_y.clone();
        let b = logo.b_y.clone();

        // 準備 1, 2: 中央の明るさ（c=16, 背景=128）の合成画像で分散マップを作り、
        // 上位 w*h*0.35 画素を着目点にする。境界の 2 画素は 5x5 窓が範囲外参照に
        // なるため対象外（罠）。
        let mid = synthesize(&a, &b, (16u32 << 3) as f32);
        let mut variances: Vec<(usize, usize, f32)> = Vec::new();
        for y in 2..h - 2 {
            for x in 2..w - 2 {
                let idx = window_indices(w, x, y);
                let mean = window_mean(&mid, &idx);
                variances.push((x, y, window_variance(&mid, &idx, mean)));
            }
        }
        variances.sort_by(|p, q| q.2.partial_cmp(&p.2).unwrap());
        let n_points = ((w * h) as f64 * MASKRATIO).round() as usize;
        let n_points = n_points.min(variances.len());

        // 準備 3: 着目点ごとのカーネル（中央明るさの画像から、平均を引いた
        // コピー。要素和は定義から 0 になる）。
        let mut points: Vec<AttentionPoint> = variances[..n_points]
            .iter()
            .map(|&(x, y, _)| {
                let idx = window_indices(w, x, y);
                let mean = window_mean(&mid, &idx);
                let mut kernel = [0f32; WINDOW];
                for (k, &i) in kernel.iter_mut().zip(idx.iter()) {
                    *k = mid[i] - mean;
                }
                AttentionPoint {
                    idx,
                    kernel,
                    scale: [0.0; LEVELS],
                    scale2: [0.0; LEVELS],
                }
            })
            .collect();

        // 準備 4: 着目点 x 明るさ 32段階の相関から scale/scale2 を決める。
        // raw_corr[point][level]。
        let mut raw_corr = vec![[0f32; LEVELS]; points.len()];
        for (c, corr_at_c) in raw_corr_by_level(&a, &b, &points).into_iter().enumerate() {
            for (pi, v) in corr_at_c.into_iter().enumerate() {
                raw_corr[pi][c] = v;
            }
        }
        // 同じ明るさ段階における着目点間の |相関| の平均（scale2 のキャップ基準）。
        let mut avg_abs_corr = [0f32; LEVELS];
        for c in 0..LEVELS {
            if !points.is_empty() {
                let sum: f32 = raw_corr.iter().map(|rc| rc[c].abs()).sum();
                avg_abs_corr[c] = sum / points.len() as f32;
            }
        }
        for (pi, point) in points.iter_mut().enumerate() {
            for c in 0..LEVELS {
                let acorr = raw_corr[pi][c].abs();
                point.scale[c] = if acorr > 0.0 { 1.0 / acorr } else { 0.0 };
                let denom = avg_abs_corr[c] * 0.2;
                point.scale2[c] = if denom > 0.0 {
                    (acorr / denom).min(1.0)
                } else {
                    0.0
                };
            }
        }

        let mut mask = LogoMask {
            w,
            h,
            a,
            b,
            points,
            black_score: 1.0,
        };

        // 準備 5: 明るさ 16（背景そのものの画素値。c = 16 >> 3 = 2）の合成画像を
        // 基準スコアとして保持する。
        let black_img = synthesize(&mask.a, &mask.b, 16.0);
        mask.black_score = mask.raw_score(&black_img);
        mask
    }

    /// 着目点ごとの、明るさ 32 段階それぞれに対する生の相関値
    /// （`Σ kernel[i] * (Y[i] - avg)`、`Y` は各段階の合成画像）。
    ///
    /// 戻り値は `[段階][着目点]`。
    fn raw_score(&self, img: &[f32]) -> f32 {
        let mut total = 0f32;
        for point in &self.points {
            let mean = window_mean(img, &point.idx);
            let sum = correlate(img, &point.idx, &point.kernel, mean);
            let level = (mean.clamp(0.0, 255.0) as usize >> 3).min(LEVELS - 1);
            let scaled = (sum * point.scale[level]).clamp(-1.0, 1.0);
            total += scaled * point.scale2[level];
        }
        total
    }

    /// 1 フレームの輝度プレーン `src`（`w*h` 要素、行優先、8bit）を評価し、
    /// `(corr0, corr1)` を返す。
    ///
    /// `corr0` はそのまま（`fade = 0`）、`corr1` はロゴ除去後（`fade = 1`、
    /// `dst = a * src + b * maxv`）の相関を [`LogoMask::black_score`] で正規化
    /// したもの。ロゴが実際にあれば `corr0` が 1 付近・`corr1` が 0 付近、
    /// 無いのに評価すると `corr0` が 0 付近・`corr1` が負に振れる。
    pub fn evaluate(&self, src: &[u8]) -> (f32, f32) {
        assert_eq!(
            src.len(),
            self.w * self.h,
            "evaluate: src の長さ({})が w*h({})と一致しない",
            src.len(),
            self.w * self.h,
        );
        let src_f: Vec<f32> = src.iter().map(|&v| v as f32).collect();
        let corr0 = self.raw_score(&src_f) / self.black_score;

        let bg: Vec<f32> = self
            .a
            .iter()
            .zip(&self.b)
            .zip(&src_f)
            .map(|((&ai, &bi), &si)| ai * si + bi * MAXV)
            .collect();
        let corr1 = self.raw_score(&bg) / self.black_score;

        (corr0, corr1)
    }
}

/// 明るさ 32 段階それぞれについて、全着目点の生の相関値を計算する
/// （準備段階 4 で使う。`LogoMask` 構築中の借用の都合で `&[AttentionPoint]` を
/// 直接受け取る自由関数にしている）。
///
/// 戻り値は `[段階][着目点]`。
fn raw_corr_by_level(a: &[f32], b: &[f32], points: &[AttentionPoint]) -> Vec<Vec<f32>> {
    (0..LEVELS)
        .map(|c| {
            let level = (c << 3) as f32;
            let img = synthesize(a, b, level);
            points
                .iter()
                .map(|p| {
                    let mean = window_mean(&img, &p.idx);
                    correlate(&img, &p.idx, &p.kernel, mean)
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用の合成ロゴ: `a` は全画素 1.0（定数）、`b` はロゴ形（十字）で
    /// `0.1`、それ以外は `0.0`。
    ///
    /// `a` を定数にすることで、合成画像の局所平均引き後の値が背景の明るさに
    /// 依存しなくなる（`(level - b*maxv)/1 - avg` は `level` 由来の項が
    /// 平均引きで打ち消えるため `b` の形だけが残る）。これにより:
    ///
    /// - 「ロゴあり」画像（[`synthesize`] そのもの）の相関は `kernel` と
    ///   完全に一致し、明るさに関わらず符号・大きさが同じになる
    /// - 「ロゴなし」画像に `bg = a*src + b*maxv` を適用すると `b` の寄与だけが
    ///   `+` の符号で乗り、`kernel`（`b` に `-` の符号で依存）と正確に逆相関する
    ///
    /// という 2 点を式で保証できるため、テストの期待値がロゴ検出の実装詳細に
    /// 依存しない。
    fn cross_logo(w: usize, h: usize) -> LogoData {
        let a = vec![1.0f32; w * h];
        let mut b = vec![0.0f32; w * h];
        let cx = w / 2;
        let cy = h / 2;
        for y in 0..h {
            for x in 0..w {
                let on_cross = x == cx || x + 1 == cx || y == cy || y + 1 == cy;
                if on_cross {
                    // maxv * b = 255 * 0.1 = 25.5。level=50 で合成した観測値
                    // (level - maxv*b)/a = 24.5 が 0..255 に収まるよう、
                    // テストで使う最低の level（50）に対して十分小さい値にする
                    // (罠: to_u8 での 0..255 クランプが式の線形性を壊すため、
                    // テスト用の合成画像自体がクランプされない範囲を選ぶ必要がある)。
                    b[y * w + x] = 0.1;
                }
            }
        }
        LogoData {
            w: w as i32,
            h: h as i32,
            log_uv_x: 1,
            log_uv_y: 1,
            imgw: w as i32,
            imgh: h as i32,
            imgx: 0,
            imgy: 0,
            name: String::new(),
            service_id: 0,
            a_y: a,
            b_y: b,
            a_u: Vec::new(),
            b_u: Vec::new(),
            a_v: Vec::new(),
            b_v: Vec::new(),
        }
    }

    fn to_u8(img: &[f32]) -> Vec<u8> {
        img.iter()
            .map(|&v| v.round().clamp(0.0, 255.0) as u8)
            .collect()
    }

    #[test]
    fn kernel_elements_sum_to_zero() {
        let logo = cross_logo(16, 16);
        let mask = LogoMask::new(&logo);
        assert!(!mask.points.is_empty(), "着目点が選ばれているはず");
        for point in &mask.points {
            let sum: f32 = point.kernel.iter().sum();
            assert!(
                sum.abs() < 1e-3,
                "カーネルの要素和は 0 に近いはず（実際 {sum}）"
            );
        }
    }

    #[test]
    fn logo_present_gives_corr0_near_one_and_small_corr1() {
        let logo = cross_logo(16, 16);
        let mask = LogoMask::new(&logo);

        for &level in &[50.0f32, 200.0f32] {
            let frame = to_u8(&synthesize(&logo.a_y, &logo.b_y, level));
            let (corr0, corr1) = mask.evaluate(&frame);
            assert!(
                (corr0 - 1.0).abs() < 0.1,
                "level={level}: corr0 は 1 付近のはず（実際 {corr0}）"
            );
            assert!(
                corr1.abs() < 0.1,
                "level={level}: |corr1| は小さいはず（実際 {corr1}）"
            );
        }
    }

    #[test]
    fn corr0_is_similar_across_background_brightness() {
        let logo = cross_logo(16, 16);
        let mask = LogoMask::new(&logo);

        let frame_dark = to_u8(&synthesize(&logo.a_y, &logo.b_y, 50.0));
        let frame_bright = to_u8(&synthesize(&logo.a_y, &logo.b_y, 200.0));
        let (corr0_dark, _) = mask.evaluate(&frame_dark);
        let (corr0_bright, _) = mask.evaluate(&frame_bright);

        assert!(
            (corr0_dark - corr0_bright).abs() < 0.05,
            "明るさ別正規化により corr0 は同程度のはず（dark={corr0_dark}, bright={corr0_bright}）"
        );
    }

    #[test]
    fn no_logo_gives_corr0_near_zero_and_negative_corr1() {
        let logo = cross_logo(16, 16);
        let mask = LogoMask::new(&logo);

        // ロゴを合成していない、単なる均一背景。
        let flat = vec![120u8; 16 * 16];
        let (corr0, corr1) = mask.evaluate(&flat);

        assert!(corr0.abs() < 1e-3, "corr0 は 0 付近のはず（実際 {corr0}）");
        assert!(corr1 < -0.1, "corr1 は負のはず（実際 {corr1}）");
    }
}
