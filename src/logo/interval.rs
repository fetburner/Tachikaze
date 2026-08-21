//! フレームごとの `(corr0, corr1)` からロゴ表示区間を決め、logoframe 形式の
//! テキストを書く。
//!
//! ## 由来
//!
//! Amatsukaze（<https://github.com/nekopanda/Amatsukaze>）`LogoScan.hpp` の
//! `LogoFrame::writeResult` を移植したもの。ライセンスは MIT
//! (<http://opensource.org/licenses/mit-license.php>)、
//! `Copyright (c) 2017-2019 Nekopanda`。詳細は `THIRD-PARTY-NOTICES.md`
//! 「移植したコード」節。
//!
//! ## 考え方
//!
//! 生のスコアは動きの多い映像で激しく上下するため、そのまま閾値で切ると
//! ロゴ表示区間が細かく分断される。そこで「直前 0.5 秒・直後 0.5 秒の
//! 最大値」（[`Judgement`] の MinMax 判定。現在フレームの値が悪くても前後の
//! どちらかの窓が良ければ救済される）と「1 秒移動平均」（薄いが安定した表示を
//! 識別する）の 2 つの判定を出し、食い違えば不明とする。不明の連続区間は
//! 前後の判定が同じならそれで埋める。
//!
//! ### 手順
//!
//! 1. 生スコア `raw[n] = max(0, corr0) + min(0, corr1)`（`corr0` のマイナスと
//!    `corr1` のプラスはノイズなので捨てる）
//! 2. 両端を端の値で埋める（[`write_result`] 内、窓の半分ぶん外側まで。原典
//!    `LogoScan.hpp` の `winFrames/2`）。**忘れると先頭・末尾に偽の境界が出る**
//!    （窓が配列の外に出た分をゼロ扱いすると符号が変わってしまう）
//! 3. 各フレームで 2 つの判定を出す（[`judge_frames`]）。閾値は
//!    `THRESH = 0.2`、`threshL = 0.5`
//!    - MinMax: 直前 0.5 秒の最大値と直後 0.5 秒の最大値の小さい方。
//!      `|v| < threshL` なら不明、`v < 0` ならロゴなし、それ以外はロゴあり
//!    - 1 秒移動平均: `|avg| < THRESH` なら不明、`avg < 0` ならロゴなし、
//!      それ以外はロゴあり
//!    - 2 つが食い違ったら不明
//! 4. 不明の連続区間は、前後の判定が同じならその値で埋める（違えば不明のまま。
//!    [`fill_unknown_runs`]）。区間が配列の先頭・末尾に接している場合、
//!    存在しない側の判定は「ロゴなし」とみなす（原典が `frameResult.begin()` /
//!    `end()` で `0` を使うのに合わせる）
//! 5. 境界精緻化用に、各フレームの 0.5 秒メディアンを別に持つ
//!    （[`judge_frames`] の戻り値の一部。移動平均には時間差があるため）
//! 6. 判定がロゴありになる区間ごとに、開始・終了それぞれの「最良位置」と
//!    「範囲（左端・右端）」を、メディアン値が `THRESH` を跨ぐ位置まで前後に
//!    探索して精緻化する（[`build_text`]）
//! 7. 出力（1 区間 2 行）:
//!    ```text
//!    <best> S 0 ALL <範囲左> <範囲右>
//!    <best> E 0 ALL <範囲左> <範囲右>
//!    ```
//!    フレーム番号は 0 始まりの表示順。`0` は fade（この関数は常に `0`）、
//!    `ALL` は interlace（常に `ALL`）。`<best>` はその境界の「最良位置」、
//!    `<範囲左>`・`<範囲右>` は精緻化で探索した可能性の範囲。区間が 1 つも
//!    無ければ何も書かない。**`S` 行と `E` 行は必ずセットで出す**
//!    （join_logo_scp のパーサは開始だけ・終了だけの行を捨てる）
//!
//! ## 呼び出し側への委譲
//!
//! 出力が空のとき「空ファイルを書く」のと「ファイルを作らない」のは意味が
//! 違う（join_logo_scp は `-inlogo` を渡されてロゴ情報が無ければ警告を出して
//! 全フレームをロゴ表示中として扱う）。この関数は文字列と検出フレーム数・
//! 全フレーム数を返すだけにし、書くかどうか・`-inlogo` を渡すかどうかの判断は
//! 呼び出し側（後続 issue）に委ねる。
//!
//! ## 原典との差
//!
//! 原典はロゴ候補が複数ある前提で `selectLogo()` が選んだ1本の `logoIndex` の
//! スコア列を使う。本ツールはロゴ1本の運用を前提にしており、複数ロゴからの
//! 選択（`selectLogo` 相当）は呼び出し側の責務にする。この関数はすでに1本に
//! 決まった `(corr0, corr1)` の列だけを受け取る。
//!
//! [`build_text`] 内の `s_end` の後方精緻化は、原典（`LogoScan.hpp`
//! L1779-1781、`std::find_if(std::make_reverse_iterator(sEnd), frameResult.rend(), ...)`）
//! が配列の先頭 (0) まで戻るのに対し、本実装は呼び出し元が渡す `it`
//! （直前の区間の終端）までしか戻らない。意図的な逸脱で、原典側がこの下限を
//! 超えて戻る入力では逆イテレータ範囲が反転して UB になる（本実装はその
//! ケースで安全に `it` で止まる）。

/// 1フレームの `(corr0, corr1)` から生スコアを求める（[`write_result`] 手順1と
/// 同じ式）。`corr0` のマイナスと `corr1` のプラスはノイズなので捨てる。
///
/// ロゴ検出の階層化方式（`src/logo/hier.rs`、issue #154）がキーフレーム単体を
/// 窓なしで粗く判定する際にもこの式を使うため、複製せず `pub(crate)` で共有する。
pub(crate) fn raw_score(corr0: f32, corr1: f32) -> f32 {
    corr0.max(0.0) + corr1.min(0.0)
}

/// 窓を使わない瞬時判定。[`Judgement::from_threshold`] を移動平均判定と同じ
/// 閾値 [`THRESH`] で生スコアに直接適用したもの。`None` は不明
/// （[`Judgement::Unknown`]）。
///
/// `write_result` 本体はこの関数を使わない（窓（MinMax・移動平均）を通した
/// 判定だけを使う）。窓を作れない孤立した1フレームだけの粗い判定
/// （ロゴ検出の階層化方式のキーフレーム走査、issue #154）のために公開する。
pub(crate) fn instant_label(raw: f32) -> Option<bool> {
    match Judgement::from_threshold(raw, THRESH) {
        Judgement::HasLogo => Some(true),
        Judgement::NoLogo => Some(false),
        Judgement::Unknown => None,
    }
}

/// フレーム1個の判定値。**数値のまま扱わず列挙型にする**
/// （原典は `int`（0/1/2）で、取り違えても例外が飛ばない）。
///
/// 判定値の意味: `NoLogo` = ロゴなし（原典 `0`）、`Unknown` = 不明（原典 `1`）、
/// `HasLogo` = ロゴあり（原典 `2`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Judgement {
    NoLogo,
    Unknown,
    HasLogo,
}

impl Judgement {
    /// `|v| < thresh` なら不明、`v < 0` ならロゴなし、それ以外はロゴあり。
    fn from_threshold(v: f32, thresh: f32) -> Judgement {
        if v.abs() < thresh {
            Judgement::Unknown
        } else if v < 0.0 {
            Judgement::NoLogo
        } else {
            Judgement::HasLogo
        }
    }
}

/// 移動平均判定の閾値（不明とみなす絶対値の上限）。境界精緻化の閾値にも使う。
const THRESH: f32 = 0.2;
/// MinMax判定の閾値（不明とみなす絶対値の上限）。
const THRESH_L: f32 = 0.5;
/// 移動平均の窓の長さ（秒）。
const AVG_DUR_SEC: f64 = 1.0;
/// メディアンの窓の長さ（秒）。
const MEDIAN_DUR_SEC: f64 = 0.5;

/// [`write_result`] の戻り値。
#[derive(Debug, Clone, PartialEq)]
pub struct LogoIntervals {
    /// logoframe 形式のテキスト（区間が1つも無ければ空文字列）。
    pub text: String,
    /// 判定がロゴあり（不明区間の穴埋め後）になったフレーム数。
    pub logo_frames: usize,
    /// 入力の全フレーム数。
    pub total_frames: usize,
}

/// `(corr0, corr1)` の列と `fps` から、ロゴ表示区間を判定し logoframe 形式の
/// テキストを書く（Amatsukaze `LogoFrame::writeResult` 相当。手順はモジュール
/// doc comment 参照）。
///
/// `scores` が空の場合は `text` が空文字列、`logo_frames` / `total_frames` が
/// 0 の [`LogoIntervals`] を返す。
///
/// `fps` は有限の値を渡すこと。`f64::INFINITY` を渡すと `half_avg_frames` が
/// `usize::MAX` に飽和し、直後の `* 2` で（debug build では）overflow panic
/// する。
pub fn write_result(scores: &[(f32, f32)], fps: f64) -> LogoIntervals {
    let n = scores.len();
    if n == 0 {
        return LogoIntervals {
            text: String::new(),
            logo_frames: 0,
            total_frames: 0,
        };
    }

    // 手順1: 生スコア。corr0のマイナスとcorr1のプラスはノイズなので捨てる。
    let raw: Vec<f32> = scores
        .iter()
        .map(|&(corr0, corr1)| raw_score(corr0, corr1))
        .collect();

    // 窓サイズ。原典は fps をまず整数に丸めてから使う（`framesPerSec`）ので、
    // ここでも合わせる。
    let frames_per_sec = fps.round();
    let half_avg_frames = (frames_per_sec * AVG_DUR_SEC / 2.0 + 0.5) as usize;
    let half_median_frames = (frames_per_sec * MEDIAN_DUR_SEC / 2.0 + 0.5) as usize;
    let avg_frames = half_avg_frames * 2 + 1;
    let median_frames = half_median_frames * 2 + 1;
    // 前後の窓のうち大きい方の半分ぶんパディングすれば、すべての窓が配列の
    // 外に出ない（原典 `winFrames = max(aveFrames, medianFrames)`）。
    let win_frames = avg_frames.max(median_frames);
    let front_pad = win_frames / 2;
    let back_pad = win_frames - front_pad;

    // 手順2: 両端を端の値で埋める。
    let mut padded = vec![0f32; front_pad + n + back_pad];
    padded[..front_pad].fill(raw[0]);
    padded[front_pad..front_pad + n].copy_from_slice(&raw);
    padded[front_pad + n..].fill(raw[n - 1]);

    // 手順3, 5: 各フレームの判定と、境界精緻化用の0.5秒メディアン。
    let (mut result, score) = judge_frames(
        &padded,
        front_pad,
        n,
        half_avg_frames,
        avg_frames,
        half_median_frames,
        median_frames,
    );

    // 手順4: 不明区間の穴埋め。
    fill_unknown_runs(&mut result);

    let logo_frames = result.iter().filter(|&&j| j == Judgement::HasLogo).count();

    // 手順6, 7: 区間ごとの境界精緻化と出力。
    let text = build_text(&result, &score);

    LogoIntervals {
        text,
        logo_frames,
        total_frames: n,
    }
}

/// 手順3, 5: パディング済み生スコアから、各フレームの判定（MinMaxと移動平均の
/// 合議、不明の穴埋め前）と境界精緻化用メディアンを計算する。
///
/// `padded` は長さ `front_pad + n + back_pad`。フレーム `i`（`0..n`）は
/// `padded[front_pad + i]` に対応する。呼び出し側（[`write_result`]）が
/// `front_pad` / `back_pad` を「どの窓も配列の外に出ない」ように決めている
/// ことを前提にする。
fn judge_frames(
    padded: &[f32],
    front_pad: usize,
    n: usize,
    half_avg_frames: usize,
    avg_frames: usize,
    half_median_frames: usize,
    median_frames: usize,
) -> (Vec<Judgement>, Vec<f32>) {
    let mut result = Vec::with_capacity(n);
    let mut score = Vec::with_capacity(n);
    for i in 0..n {
        let idx = front_pad + i;

        // MinMax: 直前0.5秒（現在フレームを含まない）の最大値と、直後0.5秒
        // （同様に現在フレームを含まない）の最大値の小さい方。
        let before_max = padded[idx - half_avg_frames..idx]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        let after_max = padded[idx + 1..idx + 1 + half_avg_frames]
            .iter()
            .copied()
            .fold(f32::NEG_INFINITY, f32::max);
        let minmax_result = Judgement::from_threshold(before_max.min(after_max), THRESH_L);

        // 1秒移動平均（現在フレームを含む、前後半分ずつの窓）。
        let avg: f32 = padded[idx - half_avg_frames..=idx + half_avg_frames]
            .iter()
            .sum::<f32>()
            / avg_frames as f32;
        let avg_result = Judgement::from_threshold(avg, THRESH);

        // 2つが食い違ったら不明。
        result.push(if minmax_result != avg_result {
            Judgement::Unknown
        } else {
            minmax_result
        });

        // 境界精緻化用の0.5秒メディアン（現在フレームを含む、前後半分ずつの窓）。
        let mut buf: Vec<f32> =
            padded[idx - half_median_frames..=idx + half_median_frames].to_vec();
        buf.sort_by(f32::total_cmp);
        score.push(buf[half_median_frames]);
        debug_assert_eq!(buf.len(), median_frames);
    }
    (result, score)
}

/// 手順4: 不明の連続区間を、前後の判定が同じならその値で埋める（違えば不明の
/// まま）。区間が配列の先頭・末尾に接している場合、存在しない側の判定は
/// 「ロゴなし」とみなす（原典が `frameResult.begin()` / `end()` の場合に `0`
/// を使うのに合わせる）。
fn fill_unknown_runs(result: &mut [Judgement]) {
    let n = result.len();
    let mut i = 0;
    while i < n {
        if result[i] != Judgement::Unknown {
            i += 1;
            continue;
        }
        let start = i;
        let mut end = i;
        while end < n && result[end] == Judgement::Unknown {
            end += 1;
        }
        let prev = if start == 0 {
            Judgement::NoLogo
        } else {
            result[start - 1]
        };
        let next = if end == n {
            Judgement::NoLogo
        } else {
            result[end]
        };
        if prev == next {
            for r in &mut result[start..end] {
                *r = prev;
            }
        }
        i = end;
    }
}

/// `[lo, hi)` を前から探索し、最初に `pred` を満たす添字を返す。見つからなければ
/// `hi` を返す（`std::find_if(first, last, pred)` が見つからないと `last` を
/// 返すのに対応する、宣言的な「見つからなかった」の表現）。
fn find_forward(lo: usize, hi: usize, mut pred: impl FnMut(usize) -> bool) -> usize {
    (lo..hi).find(|&k| pred(k)).unwrap_or(hi)
}

/// `[lo, hi)` を後ろから探索し、最初に `pred` を満たす添字 `p` に対して `p + 1`
/// を返す。見つからなければ `lo` を返す。
///
/// 原典の `std::find_if(std::make_reverse_iterator(hi), std::make_reverse_iterator(lo),
/// pred).base()` に対応する（逆イテレータの `base()` は「見つかった位置の1つ
/// 先」を指すため `p + 1` になる。見つからずに逆イテレータが尽きた場合は
/// `lo` に戻る）。
fn find_backward_base(hi: usize, lo: usize, mut pred: impl FnMut(usize) -> bool) -> usize {
    (lo..hi).rev().find(|&k| pred(k)).map_or(lo, |k| k + 1)
}

/// 手順6, 7: 判定列とメディアンから、ロゴ区間ごとに境界を精緻化して
/// logoframe 形式のテキストを組み立てる。
fn build_text(result: &[Judgement], score: &[f32]) -> String {
    let n = result.len();
    let mut text = String::new();
    let mut it = 0usize;
    while it < n {
        // ロゴありの開始候補と、その後の最初のロゴなし（終了候補）。
        let s_end_raw = find_forward(it, n, |k| result[k] == Judgement::HasLogo);
        let e_end_raw = find_forward(s_end_raw, n, |k| result[k] == Judgement::NoLogo);

        // 移動平均は時間差があるので、メディアンがTHRESHを跨ぐ位置まで
        // 前後に探索して開始位置を精緻化する。
        let mut s_end = s_end_raw;
        if s_end != n {
            s_end = if score[s_end] >= THRESH {
                // すでに始まっているので戻ってみる。
                find_backward_base(s_end, it, |k| score[k] < THRESH)
            } else {
                // まだ始まっていないので進んでみる。
                find_forward(s_end, n, |k| score[k] >= THRESH)
            };
        }

        // 同様に終了位置を精緻化する。
        let mut e_end = e_end_raw;
        if e_end != n {
            e_end = if score[e_end] <= -THRESH {
                // すでに終わっているので戻ってみる。
                find_backward_base(e_end, s_end, |k| score[k] > -THRESH)
            } else {
                // まだ終わっていないので進んでみる。
                find_forward(e_end_raw, n, |k| score[k] <= -THRESH)
            };
        }

        // 可能性の範囲（左端）と最良位置。
        let s_start = find_backward_base(s_end, it, |k| score[k] <= -THRESH);
        let e_start = find_backward_base(e_end, s_end, |k| score[k] >= THRESH);
        let s_best = find_forward(s_start, s_end, |k| score[k] > 0.0);
        let e_best = find_backward_base(e_end, e_start, |k| score[k] > 0.0);

        // 区間がある場合だけ出力する。開始位置の前方精緻化（直後の
        // `find_forward(s_end, n, ...)`）が `e_end` を追い越すと `s_end >
        // e_end` になり得る（原典ならここで逆イテレータ範囲が反転して UB）。
        // `!=` ではなく`<` にして、この malformed なケース（範囲が反転した
        // 行）だけを出力しない（S より前の E を join_logo_scp に渡さない）。
        if s_end < e_end {
            let s_starti = s_start as i64;
            let s_besti = s_best as i64;
            let s_endi = s_end as i64;
            // E側は「終了の1つ先」を指す添字なので、フレーム番号にするには
            // 1引く（原典 `eStarti`/`eBesti`/`eEndi` の `- 1`）。
            let e_starti = e_start as i64 - 1;
            let e_besti = e_best as i64 - 1;
            let e_endi = e_end as i64 - 1;
            text.push_str(&format!("{s_besti:>6} S 0 ALL {s_starti:>6} {s_endi:>6}\n"));
            text.push_str(&format!("{e_besti:>6} E 0 ALL {e_starti:>6} {e_endi:>6}\n"));
        }

        it = e_end_raw;
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // raw_score / instant_label（ロゴ検出の階層化方式が使う共有ロジック、
    // issue #154）
    // ---------------------------------------------------------------

    #[test]
    fn raw_score_drops_negative_corr0_and_positive_corr1() {
        assert_eq!(raw_score(-1.0, 0.5), 0.0);
        assert_eq!(raw_score(1.0, 0.5), 1.0);
        assert_eq!(raw_score(1.0, -0.5), 0.5);
    }

    #[test]
    fn instant_label_matches_write_result_threshold() {
        assert_eq!(instant_label(1.0), Some(true));
        assert_eq!(instant_label(-1.0), Some(false));
        assert_eq!(instant_label(0.0), None);
        // THRESH = 0.2 の境界。
        assert_eq!(instant_label(0.19), None);
        assert_eq!(instant_label(0.21), Some(true));
    }

    /// テスト用の生スコアを `(corr0, corr1)` に変換する。テスト側は
    /// `raw[n] = max(0,corr0)+min(0,corr1)` の詳細を気にせず生スコアだけを
    /// 直接指定したいので、`corr1` にそのまま入れて `corr0 = 0` にする
    /// （`raw = 0 + min(0, corr1) = corr1` が `corr1 <= 0` の場合に成立するのは
    /// 都合が悪いので、正の値は `corr0` 側に入れる）。
    fn scores_from_raw(raw: &[f32]) -> Vec<(f32, f32)> {
        raw.iter()
            .map(|&v| if v >= 0.0 { (v, 0.0) } else { (0.0, v) })
            .collect()
    }

    /// 各行の "S" / "E" の出現数を数える（`S` 行と `E` 行が必ずセットで出る
    /// ことの確認に使う）。
    fn count_markers(text: &str, marker: &str) -> usize {
        text.lines()
            .filter(|line| line.split_whitespace().nth(1) == Some(marker))
            .count()
    }

    #[test]
    fn honpen_cm_honpen_gives_two_intervals() {
        // 本編1000フレーム(ロゴあり) -> CM450フレーム(ロゴなし) -> 本編1000フレーム(ロゴあり)。
        let mut raw = vec![1.0f32; 1000];
        raw.extend(vec![-1.0f32; 450]);
        raw.extend(vec![1.0f32; 1000]);
        let scores = scores_from_raw(&raw);

        let result = write_result(&scores, 30.0);

        // フレーム番号までそのまま固定する。issue の罠「1始まりにすると
        // 全区間が1フレームずれ、しかもエラーは出ない」への直接の回帰テスト
        // になる（0始まり・表示順で `0-999` / `1450-2449` にぴったり一致する
        // はず）。
        assert_eq!(
            result.text,
            concat!(
                "     0 S 0 ALL      0      0\n",
                "   999 E 0 ALL    999    999\n",
                "  1450 S 0 ALL   1450   1450\n",
                "  2449 E 0 ALL   2449   2449\n",
            )
        );
        assert_eq!(result.total_frames, 2450);
        assert_eq!(result.logo_frames, 1994);
    }

    #[test]
    fn brief_dip_inside_honpen_does_not_split_the_interval() {
        // 本編中に動きでロゴがかき消されたことを模した、数フレームだけの落ち込み。
        // 前後0.5秒(fps=30で15フレーム)より短い落ち込みなので、MinMaxの救済で
        // 区間が分断されないはず。
        let mut raw = vec![1.0f32; 1000];
        for v in raw.iter_mut().skip(500).take(6) {
            *v = -1.0;
        }
        let scores = scores_from_raw(&raw);

        let result = write_result(&scores, 30.0);

        assert_eq!(
            count_markers(&result.text, "S"),
            1,
            "落ち込みで区間が分断されず1本のままのはず: {}",
            result.text
        );
        assert_eq!(count_markers(&result.text, "E"), 1);
    }

    #[test]
    fn all_no_logo_gives_empty_text() {
        let raw = vec![-1.0f32; 100];
        let scores = scores_from_raw(&raw);

        let result = write_result(&scores, 30.0);

        assert_eq!(result.text, "");
        assert_eq!(result.logo_frames, 0);
        assert_eq!(result.total_frames, 100);
    }

    /// 出力1行を期待文字列と丸ごと比較し、列の順序と桁が logoframe 形式どおり
    /// であることを確認する。
    ///
    /// fps=2.0 で `half_avg_frames = half_median_frames = 1`（窓が前後1フレーム
    /// ずつ）になるように選び、10フレームの生スコア
    /// `[-1,-1,-1, 1,1,1,1, -1,-1,-1]` を手で計算した結果と比較する
    /// （手計算: フレーム3,6は MinMax と移動平均が食い違って不明、
    /// フレーム4,5がロゴあり。境界精緻化はメディアンが単調に変化する箇所なので
    /// 揺れなく s_start=s_best=s_end=3、e_start=e_best=e_end=7 になる）。
    #[test]
    fn one_line_matches_logoframe_format_exactly() {
        let raw = [-1.0, -1.0, -1.0, 1.0, 1.0, 1.0, 1.0, -1.0, -1.0, -1.0];
        let scores = scores_from_raw(&raw);

        let result = write_result(&scores, 2.0);

        assert_eq!(
            result.text,
            "     3 S 0 ALL      3      3\n     6 E 0 ALL      6      6\n"
        );
        assert_eq!(result.logo_frames, 2);
        assert_eq!(result.total_frames, 10);
    }

    #[test]
    fn empty_scores_give_empty_result() {
        let result = write_result(&[], 30.0);
        assert_eq!(
            result,
            LogoIntervals {
                text: String::new(),
                logo_frames: 0,
                total_frames: 0,
            }
        );
    }

    /// テスト用の決定的な擬似乱数生成器（xorshift64*）。標準の乱数クレートを
    /// 追加せずに再現可能なノイズ入力を作るためだけに使う。
    struct Xorshift64(u64);

    impl Xorshift64 {
        fn next_f32(&mut self) -> f32 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            // [0, 1) の範囲に落とす。
            (self.0 >> 11) as f32 / (1u64 << 53) as f32
        }
    }

    /// 出力1行 (`"<best> S 0 ALL <左> <右>"` / `"<best> E 0 ALL <左> <右>"`) を
    /// `(best, 左, 右)` にパースする。
    fn parse_line(line: &str) -> (i64, i64, i64) {
        let fields: Vec<&str> = line.split_whitespace().collect();
        // fields = [best, "S"/"E", "0", "ALL", 左, 右]
        let best = fields[0].parse().unwrap();
        let left = fields[4].parse().unwrap();
        let right = fields[5].parse().unwrap();
        (best, left, right)
    }

    /// 実バグの回帰テスト: 開始位置の前方精緻化（`build_text` 内、
    /// `find_forward(s_end, n, score >= THRESH)`）が `e_end` を追い越すと
    /// `s_end > e_end` になることがある。修正前は `if s_end != e_end` を
    /// 通ってしまい、
    /// ```text
    ///     79 S 0 ALL     79     79
    ///     78 E 0 ALL     78     69      <- 範囲左(78) > 範囲右(69)
    /// ```
    /// のような、E行の範囲が反転した壊れた出力を実際に吐いていた（fps=5・
    /// ノイズの多いランダム入力で確認済み）。`if s_end < e_end` に直したので、
    /// このケースは出力から除外される。fps=5・ノイズの多いランダム入力
    /// 5000ケースについて、出力される全行が「範囲の左 <= 右」を満たし、
    /// 各区間で S行がE行より前に来ることを確認する。
    #[test]
    fn build_text_never_emits_reversed_ranges() {
        let mut rng = Xorshift64(0x2545_F491_4F6C_DD1D);
        let n = 200;

        for case in 0..5000u32 {
            let raw: Vec<f32> = (0..n).map(|_| rng.next_f32() * 4.0 - 2.0).collect();
            let scores = scores_from_raw(&raw);

            let result = write_result(&scores, 5.0);

            let lines: Vec<&str> = result.text.lines().collect();
            assert_eq!(
                lines.len() % 2,
                0,
                "case {case}: S行とE行が対になっていない: {}",
                result.text
            );
            for pair in lines.chunks(2) {
                let (s_line, e_line) = (pair[0], pair[1]);
                assert!(
                    s_line.split_whitespace().nth(1) == Some("S")
                        && e_line.split_whitespace().nth(1) == Some("E"),
                    "case {case}: S行・E行の順序がおかしい: {s_line} / {e_line}"
                );
                let (_, s_left, s_right) = parse_line(s_line);
                let (_, e_left, e_right) = parse_line(e_line);
                assert!(s_left <= s_right, "case {case}: S行の範囲が反転: {s_line}");
                assert!(
                    e_left <= e_right,
                    "case {case}: E行の範囲が反転(実バグの再発): {e_line}"
                );
                assert!(
                    s_right <= e_right,
                    "case {case}: S行がE行より後: {s_line} / {e_line}"
                );
            }
        }
    }
}
