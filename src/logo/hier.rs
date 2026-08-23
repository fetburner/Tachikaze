//! ロゴ検出（`detect_logo`）の階層化方式（キーフレーム走査＋状態が変わる GOP
//! だけの部分デコード）のうち、ffmpeg も `.dtvi` の型も扱わない、純粋な
//! ロジック部分だけをここに集める（issue #154「解くべき問題」）。
//!
//! `src/analyze.rs::detect_logo_scores_hier` から呼ばれる。ffmpeg の起動と
//! `.dtvi` 由来の値の取り出しはすべて呼び出し側の責務で、このモジュールは
//! テストしやすいよう ffmpeg プロセスを起動せずに検証できる形にしている。
//!
//! ## 全体の流れ（呼び出し側 `detect_logo_scores_hier` が実行する）
//!
//! 1. `analyze.rs::dtvi_keyframe_frame_numbers` で `.dtvi` からキーフレームの
//!    表示順フレーム番号を集める
//! 2. `frames::stream_keyframe_cropped_luma_frames` で全キーフレームを走査し、
//!    ロゴ矩形を ffmpeg 側（`-vf crop`）で切り出した輝度を `LogoMask::evaluate`
//!    に渡し、各キーフレームの `(corr0, corr1)` を得る。読めた枚数が手順1の
//!    キーフレーム数と一致することを検査する
//! 3. `crate::logo::interval::raw_score` / `instant_label` で各キーフレームを
//!    窓なしの瞬時判定で粗くロゴ在/不在にラベルづけする
//! 4. [`build_gops`] でキーフレーム境界から GOP 一覧を作る
//! 5. [`select_refine_targets`] で**初期**の精緻化対象GOP（`kf_index` の集合）を
//!    選ぶ（ラベル変化GOP±1、先頭・末尾）
//! 6. [`group_consecutive`] で対象を連続範囲ごとにまとめ（ffmpeg 起動回数を
//!    減らす）、範囲ごとに `frames::decode_frame_range_luma_frames`（末尾GOPを
//!    含む範囲だけ `frames::decode_from_seek_until_eof_luma_frames`）で部分
//!    デコードし、先頭フレームの corr が手順2の同じキーフレームの corr と
//!    完全一致することを検査（着地オラクル、CLAUDE.md 罠3の一般形）した上で
//!    [`RefinedRange`] を作る
//! 7. [`assemble_full_scores`] で全長の `(f32, f32)` 列を組み立て、無改造の
//!    `logo::interval::write_result` に通す
//! 8. **不動点化**（レビュー指摘、blocker1）: [`touched_frame_ranges_from_logoframe_text`]
//!    で手順7の出力（S/E行の範囲欄）が触れたフレーム範囲を取り出し、
//!    [`gops_overlapping_ranges`] でそれらのフレームを含む GOP を求める。
//!    **さらに、現在精緻化済みのGOPの隣接GOPも無条件に精緻化対象へ加える**
//!    （範囲欄だけでは発見できない領域がある。詳細は下記「不動点化」の節）。
//!    手順6-7をやり直し、出力が
//!    `crate::analyze::REQUIRED_STABLE_ROUNDS` 回連続で変化しなくなるまで
//!    繰り返す（1回の一致だけでは「偽の安定」を収束と誤判定しうる）

/// GOP（キーフレーム境界で区切った区間）。`[start, end)` は表示順フレーム番号の
/// 半開区間。`kf_index` はこの GOP の先頭キーフレームの、キーフレーム番号列
/// （`analyze.rs::dtvi_keyframe_frame_numbers` の戻り値）内でのインデックス。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Gop {
    pub kf_index: usize,
    pub start: u64,
    pub end: u64,
}

/// キーフレーム番号列と全フレーム数から GOP 一覧を作る。
///
/// `kf_frame_numbers` は昇順・空でないことを前提にする（呼び出し側が `.dtvi`
/// から作る）。最後の GOP は `total_frames` までを終端とする。
pub(crate) fn build_gops(kf_frame_numbers: &[u64], total_frames: u64) -> Vec<Gop> {
    let k = kf_frame_numbers.len();
    (0..k)
        .map(|i| {
            let start = kf_frame_numbers[i];
            let end = kf_frame_numbers.get(i + 1).copied().unwrap_or(total_frames);
            Gop {
                kf_index: i,
                start,
                end,
            }
        })
        .collect()
}

/// 部分デコードの `-ss` に渡す秒数を求める。
///
/// # 実録画で見つかった罠1: `-ss` は既定で「フレーム精度シーク」を行う
///
/// 当初は「フレーム区間の中央（`+0.5` フレーム分）を指定して境界の内側に確実に
/// 入れる」という設計だった（同期サンプルへの着地が指定時刻**以下**の直近に
/// 丸められる、という誤った前提に基づく）。実測（30分1080p実録画）で判明した
/// 実際の挙動: 手元の ffmpeg（9.0.1）は `-ss`（`-i` より前、入力シーク）でも
/// **既定でフレーム精度シークを行い**、デコーダ側で「pts < 指定時刻」のフレームを
/// 捨てて出力する。対象キーフレームの pts より **後ろ**の時刻（`+0.5` フレーム分）
/// を指定すると、そのキーフレーム自身が「pts < 指定時刻」に該当して捨てられ、
/// **次のフレームから出力が始まってしまう**（終始1フレーム遅れて着地する。
/// 実測: 乙女ゲー30分1080pのフレーム360/4200/54000のいずれでも、`+0.5`フレーム版は
/// 次フレームとビット単位で完全一致した）。
///
/// 正しい規則は逆で、対象キーフレームの pts **以下**の時刻を指定する。
/// 浮動小数点の丸めで pts をわずかに超えてしまうと同じ問題が再発するため、
/// [`SEEK_EPSILON_SECONDS`] 分だけ手前にずらす。
///
/// # 実録画で見つかった罠2: pts は 0 始まりとは限らない
///
/// 上記の修正直後は `frame_number / fps` で対象キーフレームの pts を近似して
/// いた（`fps` は `.dtvi` ヘッダの `frame_rate_num`/`frame_rate_den`）。実録画
/// （乙女ゲー等、`start_time` ヘッダが 0）ではこの近似で着地・全長一致まで
/// 確認できたが、**レビューで追加した E2E（`tests/fixtures/sample.mp4`、
/// `start_time` ヘッダが 2002）で近似が破綻した**: 実際の pts は
/// `frame_number * 1001 + 2002`（`.dtvi` の `start_time` ヘッダ分だけ先頭に
/// オフセットがある。エンコーダの初期遅延・B フレームの並べ替えに起因すると
/// 見られる）で、`frame_number / fps` はこのオフセットを無視するため、
/// 2フレーム分（約67ms）早い時刻を指定してしまい、末尾GOPの部分デコードで
/// 実際に読めた枚数が期待値と2フレームずれた（issue #154 レビュー指摘、
/// blocker3の「媒体側の真値」検査で発覚。着地オラクル自体はこの矩形の画素が
/// 全編で不変なフィクスチャだったため検出できなかった）。
///
/// 修正: `frame_number` から式で近似せず、**その表示順フレームの `.dtvi` 上の
/// 実測 pts をそのまま使う**（`pts_time_base_units` として受け取る。呼び出し側
/// `detect_logo_scores_hier` が `dtvi.frames[frame_number].pts` を渡す。
/// `dtvi.frames` は `frame_number` の昇順0始まり連番で並ぶことが保証されている
/// ため添字に直接使える）。`time_base_seconds`（`.dtvi` ヘッダの
/// `time_base_num`/`time_base_den` から作る、1単位あたりの秒数）を掛けて秒に
/// 変換する。修正後は [`crate::analyze::verify_landing_oracle`]（着地オラクル）
/// が完全一致（`==`）で検査でき、実際に完全一致することを実録画3本＋
/// `sample.mp4`/`sample_logo.mp4` で確認済み。
pub(crate) fn seek_seconds_for_pts(pts_time_base_units: i64, time_base_seconds: f64) -> f64 {
    (pts_time_base_units as f64 * time_base_seconds - SEEK_EPSILON_SECONDS).max(0.0)
}

/// [`seek_seconds_for_pts`] が対象キーフレームの pts から手前にずらす量（秒）。
///
/// GOP は最短でも1フレーム（実測30分1080pの末尾GOPで14フレーム）あり、フレーム
/// 間隔は約33.4ミリ秒（30000/1001fps）。1ミリ秒はこれよりずっと小さく、前の
/// フレームの pts 領域へ食い込まない。かつ浮動小数点の丸め誤差（f64 で
/// 有効数字15〜17桁、対象の pts は最大で1時間超でも秒のオーダーなので誤差は
/// 1e-10秒未満）よりずっと大きく、丸めで pts を超えてしまう事故を防げる。
/// 実測（乙女ゲー30分1080p、フレーム360/4200/54000、および
/// `sample.mp4`/`sample_logo.mp4` の末尾GOP）でこの値により全て対象フレームへの
/// 着地・全長一致を確認済み。
const SEEK_EPSILON_SECONDS: f64 = 0.001;

/// 隣接するキーフレームのラベルが「変わった」とみなすか。
///
/// `None`（窓なし瞬時判定の不明）が絡む組は、判断材料が無いため常に「変わった」
/// 扱いにする（安全側に倒す設計判断。両方 `None` の組も含む）。
fn labels_conflict(a: Option<bool>, b: Option<bool>) -> bool {
    match (a, b) {
        (Some(x), Some(y)) => x != y,
        _ => true,
    }
}

/// **初期の**精緻化対象の GOP インデックス（[`Gop::kf_index`]）の集合を選ぶ
/// （昇順・重複無し）。不動点化（モジュール doc comment「不動点化」節）が
/// この集合を出発点として必要な分だけ広げるため、ここで決める集合は「最初の
/// 部分デコードで確実に精緻化しておきたい GOP」というヒントであり、最終的に
/// 精緻化される GOP の集合はこれより広くなることがある。
///
/// 規則:
/// - 隣接キーフレームでラベルが変わる GOP（[`labels_conflict`]）
/// - その前後1 GOP（issue #154「罠」: フェードが隣接 GOP にまたがるため、
///   変化した GOP 単独では足りない。マージンを削ってはいけない）
/// - 先頭 GOP・末尾 GOP は無条件に選ぶ
///
/// `labels[i]` はキーフレーム番号列の `i` 番目にあるキーフレームの粗いラベル
/// （`Some(true)` = ロゴあり、`Some(false)` = ロゴなし、`None` = 不明）。
pub(crate) fn select_refine_targets(labels: &[Option<bool>]) -> Vec<usize> {
    let k = labels.len();
    if k == 0 {
        return Vec::new();
    }
    let mut targets = vec![false; k];
    targets[0] = true;
    targets[k - 1] = true;
    for g in 0..k - 1 {
        if labels_conflict(labels[g], labels[g + 1]) {
            let lo = g.saturating_sub(1);
            let hi = (g + 1).min(k - 1);
            for t in &mut targets[lo..=hi] {
                *t = true;
            }
        }
    }
    (0..k).filter(|&i| targets[i]).collect()
}

/// 選ばれた GOP インデックス集合を、連続する範囲（両端を含む）ごとにまとめる。
/// 精緻化対象の GOP ごとに ffmpeg を1回起動するのではなく、連続範囲ごとに1回
/// にまとめて起動回数を減らすため。
pub(crate) fn group_consecutive(mut indices: Vec<usize>) -> Vec<(usize, usize)> {
    indices.sort_unstable();
    indices.dedup();
    let mut ranges = Vec::new();
    let mut iter = indices.into_iter();
    if let Some(first) = iter.next() {
        let mut start = first;
        let mut end = first;
        for idx in iter {
            if idx == end + 1 {
                end = idx;
            } else {
                ranges.push((start, end));
                start = idx;
                end = idx;
            }
        }
        ranges.push((start, end));
    }
    ranges
}

/// 精緻化した1つの連続範囲（[`group_consecutive`] が返す1要素分）の実測結果。
///
/// `scores[0]` は `start_frame` にあるキーフレームの corr
/// （`decode_frame_range_luma_frames` が返す先頭フレーム。着地オラクルで
/// 第1段の結果と完全一致することを呼び出し側が確認済み）で、以降
/// `scores.len()` フレーム連続で `start_frame..start_frame + scores.len() as u64`
/// を覆う。GOP 境界をまたいでよい（[`group_consecutive`] で連続範囲にまとめた
/// 複数 GOP 分を1回のデコードで覆うため）。
pub(crate) struct RefinedRange {
    pub start_frame: u64,
    pub scores: Vec<(f32, f32)>,
}

/// キーフレーム走査の corr 列と精緻化した範囲から、全長 `Vec<(f32,f32)>` を
/// 組み立てる（`logo::interval::write_result` は全フレーム長の corr 列を前提と
/// するため、無改造で渡せる形にする）。
///
/// - まず「直前のキーフレームの corr をホールド」で全体を埋める。
///   `kf_frame_numbers[0] == 0`（先頭フレームは常にキーフレーム）を前提にする。
///   クローズド GOP でシーンチェンジ由来の IDR が無い本ツールの前提
///   （CLAUDE.md「前提」節）と整合する。
/// - その後、精緻化した範囲（`refined`）の実測 corr で上書きする。
///
/// # Panics
///
/// `kf_frame_numbers` が空、`kf_frame_numbers.len() != kf_scores.len()`、
/// `kf_frame_numbers[0] != 0`、または `refined` の範囲が `total_frames` を
/// 超える場合。いずれも呼び出し側（`detect_logo_scores_hier`）の前提が
/// 崩れているバグで、検出結果が静かにずれるより落とす方を選ぶ（CLAUDE.md
/// 罠3の一般形）。
pub(crate) fn assemble_full_scores(
    kf_frame_numbers: &[u64],
    kf_scores: &[(f32, f32)],
    total_frames: u64,
    refined: &[RefinedRange],
) -> Vec<(f32, f32)> {
    assert_eq!(
        kf_frame_numbers.len(),
        kf_scores.len(),
        "キーフレーム番号列とキーフレームcorr列の長さが食い違っています"
    );
    assert!(
        !kf_frame_numbers.is_empty(),
        "キーフレームが1つも無い入力は想定しない"
    );
    assert_eq!(
        kf_frame_numbers[0], 0,
        "先頭フレームは常にキーフレームである前提が崩れている: {}",
        kf_frame_numbers[0]
    );

    let total = total_frames as usize;
    let mut out = vec![(0.0f32, 0.0f32); total];

    // ホールド埋め: 各キーフレームの corr を、次のキーフレームの直前まで敷く。
    for (i, &kf_start) in kf_frame_numbers.iter().enumerate() {
        let end = kf_frame_numbers.get(i + 1).copied().unwrap_or(total_frames);
        let start = kf_start as usize;
        let end = (end as usize).min(total);
        for slot in out.iter_mut().take(end).skip(start) {
            *slot = kf_scores[i];
        }
    }

    // 精緻化した範囲を実測 corr で上書き。
    for range in refined {
        let start = range.start_frame as usize;
        for (offset, &score) in range.scores.iter().enumerate() {
            let idx = start + offset;
            assert!(
                idx < total,
                "精緻化範囲が全フレーム数を超えています: idx={idx}, total={total}"
            );
            out[idx] = score;
        }
    }

    out
}

// --- 不動点化（issue #154 レビュー指摘、blocker1） ---
//
// `select_refine_targets`（ラベル変化GOP±1、先頭・末尾）は初期の精緻化対象を
// 決めるだけで、`logo::interval::build_text` の境界精緻化（0.5秒メディアンが
// 閾値を跨ぐ位置を前後に探索する処理）が実際にどこまで探索するかは、その時点の
// corr列（ホールド補間込み）に依存する。ホールド補間のプラトー上では実際の
// 連続デコードより探索が早く/遅く閾値を跨ぐことがあり、探索がまだホールド
// 補間のままのGOPまで踏み込むと、そのGOPの実測値次第で境界の「範囲」欄が
// 全編フルデコードと食い違いうる（実測: ぼっち・ざ・ろっく！で1行の範囲欄が
// 変化GOP±1マージンの外まで食い違った。docs/measurements.md「キーフレーム
// 走査と部分デコードによるロゴ検出の階層化」参照）。
//
// **当初の設計（出力の範囲欄が指すGOPだけを追加対象にする）は不十分だった**
// （レビュー指摘・実測で判明）。境界探索は精緻化済み区間の内側で（まだホールド
// 補間のままの隣接GOPへ踏み込む前に）閾値を跨いで停止することがあり、この場合
// 出力自身の範囲欄は常に精緻化済み区間の内側を指し、ホールド補間区間を
// 一度も指さない。そのため「範囲欄が指すGOPを対象に加える」方式だけでは、
// 本当は精緻化が必要なホールド補間区間を原理的に発見できない。
//
// そこで、範囲欄が指すGOP（`touched_frame_ranges_from_logoframe_text` /
// `gops_overlapping_ranges`、範囲がすでに精緻化済み区間の外を指していれば
// 素早く収束する経路）に加えて、**現在精緻化済みのGOPの隣接GOPも毎回無条件に
// 追加対象へ加える**（`analyze.rs::detect_logo_scores_hier` の不動点化
// ループ）。これにより、出力自身は変化していなくても精緻化範囲が着実に
// 外側へ広がり続け、いつかホールド補間区間に踏み込んで出力を変化させる
// （実測: ぼっち・ざ・ろっく！で出力が2ラウンド変化しない「偽の安定」を経て、
// さらに隣接GOPへ広げたところで出力が変わり、全編フルデコードと一致する値に
// 到達した）。
//
// 収束判定は1回の出力一致では行わず、[`crate::analyze::REQUIRED_STABLE_ROUNDS`]
// 回連続で出力が変化しないことを要求する（同定数の doc comment参照。
// 「偽の安定」を数ラウンド分の余裕で乗り越えるための経験的な閾値であり、
// 数学的な収束の証明ではない）。GOP数は有限で精緻化対象は単調増加のため、
// 遅くとも全GOPを精緻化した時点で必ず停止する。

/// `logo::interval::write_result` が返す logoframe テキストから、境界精緻化が
/// 実際に触れた（S/E行の「範囲」欄が示す）フレーム範囲の一覧を取り出す。
///
/// フォーマットは `logo::interval::build_text` が書く1行:
/// `<best> <S|E> <service_id> <label> <range_start> <range_end>`（空白区切り
/// 6列）のうち、末尾2列（`range_start`/`range_end`）を取り出す。範囲は両端を
/// 含む（`build_text` の `s_starti`/`s_endi`/`e_starti`/`e_endi` の定義に
/// 合わせる）。列数が違う行・数値でない列・範囲が反転している行（`start > end`）
/// は無視する。この関数は「安全側に広めに精緻化対象を選ぶ」ためのヒントを
/// 作るだけで、`text` のパース自体の正しさそのものを検証する責務は持たない
/// （不正な行を1つ無視しても、他の行から得られる精緻化対象は変わらず安全な
/// 方向に働く）。
pub(crate) fn touched_frame_ranges_from_logoframe_text(text: &str) -> Vec<(u64, u64)> {
    let mut ranges = Vec::new();
    for line in text.lines() {
        let cols: Vec<&str> = line.split_whitespace().collect();
        if cols.len() != 6 {
            continue;
        }
        let (Ok(start), Ok(end)) = (cols[4].parse::<i64>(), cols[5].parse::<i64>()) else {
            continue;
        };
        if start < 0 || end < 0 || start > end {
            continue;
        }
        ranges.push((start as u64, end as u64));
    }
    ranges
}

/// `ranges`（フレーム番号の両端を含む区間の一覧）のいずれかと重なる GOP の
/// インデックス集合を返す（昇順・重複無し）。GOP は `[gop.start, gop.end)` の
/// 半開区間、`ranges` の要素は両端を含む閉区間として扱う。
pub(crate) fn gops_overlapping_ranges(gops: &[Gop], ranges: &[(u64, u64)]) -> Vec<usize> {
    let mut hit = vec![false; gops.len()];
    for &(start, end) in ranges {
        for (i, gop) in gops.iter().enumerate() {
            if gop.start <= end && start < gop.end {
                hit[i] = true;
            }
        }
    }
    (0..gops.len()).filter(|&i| hit[i]).collect()
}

/// 不動点化を放棄して全編フルデコードへフォールバックすべきかを判定する
/// （レビュー指摘後の追加対応: ロゴが薄い入力での性能悪化）。
///
/// `refine_target_count`（累計の精緻化対象GOP数、既に精緻化済みのGOPも含む）が
/// `total_gops` に対して `threshold` 以上の割合に達したら `true` を返す。
/// `total_gops` が `0` のときは判定不能として `false` を返す（呼び出し側の
/// GOP構築が既に失敗しているはずで、ここでは何も判断しない）。
///
/// プロセス（ffmpeg）を起動せずに検証できるよう、`detect_logo_scores_hier`
/// から分離した純粋関数にしている。閾値そのものの値・根拠は
/// `src/analyze.rs` の `HIER_FALLBACK_GOP_FRACTION_THRESHOLD` の doc comment
/// 参照。
pub(crate) fn should_fall_back_to_full_decode(
    refine_target_count: usize,
    total_gops: usize,
    threshold: f64,
) -> bool {
    if total_gops == 0 {
        return false;
    }
    (refine_target_count as f64 / total_gops as f64) >= threshold
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------------------------------------------------------
    // build_gops
    // ---------------------------------------------------------------

    #[test]
    fn build_gops_last_gop_ends_at_total_frames() {
        let gops = build_gops(&[0, 120, 240], 300);
        assert_eq!(
            gops,
            vec![
                Gop {
                    kf_index: 0,
                    start: 0,
                    end: 120
                },
                Gop {
                    kf_index: 1,
                    start: 120,
                    end: 240
                },
                Gop {
                    kf_index: 2,
                    start: 240,
                    end: 300
                },
            ]
        );
    }

    #[test]
    fn build_gops_single_keyframe_covers_whole_video() {
        let gops = build_gops(&[0], 50);
        assert_eq!(
            gops,
            vec![Gop {
                kf_index: 0,
                start: 0,
                end: 50
            }]
        );
    }

    // ---------------------------------------------------------------
    // seek_seconds_for_pts
    // ---------------------------------------------------------------

    #[test]
    fn seek_seconds_for_pts_lands_slightly_before_the_frames_own_pts() {
        // time_base=1/30000 で pts=100100（frame_number 100 相当、
        // オフセット無し）の手前(SEEK_EPSILON_SECONDS分)。
        let time_base_seconds = 1.0 / 30000.0;
        let pts = 100_100i64;
        let s = seek_seconds_for_pts(pts, time_base_seconds);
        let expected_pts_seconds = pts as f64 * time_base_seconds;
        assert!(
            (s - (expected_pts_seconds - SEEK_EPSILON_SECONDS)).abs() < 1e-12,
            "s={s}"
        );
        assert!(
            s < expected_pts_seconds,
            "対象フレーム自身の pts より手前である必要がある: s={s}"
        );
    }

    #[test]
    fn seek_seconds_for_pts_clamps_zero_to_non_negative() {
        // pts=0はエプシロン分マイナスになるが、`-ss` に負値を渡さないよう
        // 0にクランプする。
        let s = seek_seconds_for_pts(0, 1.0 / 30000.0);
        assert_eq!(s, 0.0);
    }

    #[test]
    fn seek_seconds_for_pts_accounts_for_nonzero_start_time_offset() {
        // レビュー指摘で判明した罠2: pts は0始まりとは限らない
        // （`.dtvi` ヘッダの `start_time` 分だけ先頭にオフセットがありうる）。
        // `frame_number / fps` のような式による近似ではなく、実測 pts を
        // そのまま渡せば、このオフセットを自動的に反映できることを確認する。
        // 実測（tests/fixtures/sample.mp4）: frame_number=480 の実際の pts は
        // 482482（= (480+2)*1001、start_time=2002 のオフセット込み）で、
        // frame_number*1001=480480 とは2001（約2フレーム分）ずれる。
        let time_base_seconds = 1.0 / 30000.0;
        let pts_with_offset = 482_482i64;
        let naive_pts_without_offset = 480_480i64;
        let s = seek_seconds_for_pts(pts_with_offset, time_base_seconds);
        let naive_s = seek_seconds_for_pts(naive_pts_without_offset, time_base_seconds);
        assert!(
            s > naive_s,
            "実測pts（オフセット込み）は素朴な近似（オフセット無視）より後の時刻になるはず: \
             s={s}, naive_s={naive_s}"
        );
    }

    // ---------------------------------------------------------------
    // select_refine_targets
    // ---------------------------------------------------------------

    #[test]
    fn select_refine_targets_always_includes_first_and_last_gop() {
        // ラベルが全部同じ(変化なし)でも、先頭・末尾は無条件に選ばれる。
        let labels = vec![Some(true); 6];
        assert_eq!(select_refine_targets(&labels), vec![0, 5]);
    }

    #[test]
    fn select_refine_targets_single_gop_is_just_that_gop() {
        assert_eq!(select_refine_targets(&[Some(true)]), vec![0]);
    }

    #[test]
    fn select_refine_targets_empty_input_is_empty() {
        assert_eq!(select_refine_targets(&[]), Vec::<usize>::new());
    }

    #[test]
    fn select_refine_targets_marks_transition_and_its_neighbors() {
        // 13 GOP: [A,A,A,A, B,B,B,B,B,B,B, A,A] (0始まり)
        // 変化点: g=3 (idx3->4, A->B), g=10 (idx10->11, B->A)。
        let a = Some(true);
        let b = Some(false);
        let labels = vec![a, a, a, a, b, b, b, b, b, b, b, a, a];
        let targets = select_refine_targets(&labels);
        // g=3の前後1: {2,3,4}。g=10の前後1: {9,10,11}。先頭0・末尾12。
        assert_eq!(targets, vec![0, 2, 3, 4, 9, 10, 11, 12]);
    }

    #[test]
    fn select_refine_targets_unknown_label_forces_refinement() {
        // 不明ラベルが絡む組は常に「変わった」扱いになる。
        let labels = vec![Some(true), Some(true), None, Some(true), Some(true)];
        let targets = select_refine_targets(&labels);
        // g=1 (idx1->2, true->None) と g=2 (idx2->3, None->true) の両方が変化点。
        // 前後1: g=1 -> {0,1,2}, g=2 -> {1,2,3}。先頭0・末尾4。
        assert_eq!(targets, vec![0, 1, 2, 3, 4]);
    }

    // ---------------------------------------------------------------
    // group_consecutive
    // ---------------------------------------------------------------

    #[test]
    fn group_consecutive_merges_adjacent_indices() {
        let ranges = group_consecutive(vec![0, 2, 3, 4, 9, 10, 11, 12]);
        assert_eq!(ranges, vec![(0, 0), (2, 4), (9, 12)]);
    }

    #[test]
    fn group_consecutive_of_empty_is_empty() {
        assert_eq!(group_consecutive(vec![]), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn group_consecutive_deduplicates_and_sorts() {
        let ranges = group_consecutive(vec![5, 3, 4, 3]);
        assert_eq!(ranges, vec![(3, 5)]);
    }

    // ---------------------------------------------------------------
    // assemble_full_scores（完了条件の単体テスト: 先頭/末尾GOP、連続する遷移、
    // 精緻化ゼロ件）
    // ---------------------------------------------------------------

    #[test]
    fn assemble_with_zero_refined_ranges_is_pure_hold_fill() {
        // 精緻化ゼロ件: 3 GOP、各キーフレームのcorrをそのままホールドするだけ。
        let kf_frame_numbers = vec![0, 3, 7];
        let kf_scores = vec![(1.0, 0.0), (0.5, -0.1), (-1.0, -1.0)];
        let out = assemble_full_scores(&kf_frame_numbers, &kf_scores, 10, &[]);
        assert_eq!(
            out,
            vec![
                (1.0, 0.0),
                (1.0, 0.0),
                (1.0, 0.0),
                (0.5, -0.1),
                (0.5, -0.1),
                (0.5, -0.1),
                (0.5, -0.1),
                (-1.0, -1.0),
                (-1.0, -1.0),
                (-1.0, -1.0),
            ]
        );
    }

    #[test]
    fn assemble_refines_leading_gop_from_frame_zero() {
        // 先頭GOP: フレーム0から精緻化した実測値で上書きされる。
        let kf_frame_numbers = vec![0, 4, 8];
        let kf_scores = vec![(9.0, 9.0), (0.0, 0.0), (0.0, 0.0)];
        let refined = vec![RefinedRange {
            start_frame: 0,
            scores: vec![(1.0, 0.0), (2.0, 0.0), (3.0, 0.0), (4.0, 0.0)],
        }];
        let out = assemble_full_scores(&kf_frame_numbers, &kf_scores, 12, &refined);
        assert_eq!(
            &out[0..4],
            &[(1.0, 0.0), (2.0, 0.0), (3.0, 0.0), (4.0, 0.0)]
        );
        // 精緻化していない残りはホールド埋めのまま。
        assert_eq!(&out[4..8], &[(0.0, 0.0); 4]);
        assert_eq!(&out[8..12], &[(0.0, 0.0); 4]);
    }

    #[test]
    fn assemble_refines_trailing_gop_up_to_total_frames() {
        // 末尾GOP: total_frames の直前まで精緻化した実測値が入る。
        let kf_frame_numbers = vec![0, 4, 8];
        let kf_scores = vec![(0.0, 0.0), (0.0, 0.0), (9.0, 9.0)];
        let refined = vec![RefinedRange {
            start_frame: 8,
            scores: vec![(-1.0, 0.0), (-2.0, 0.0)],
        }];
        let out = assemble_full_scores(&kf_frame_numbers, &kf_scores, 10, &refined);
        assert_eq!(&out[0..8], &[(0.0, 0.0); 8]);
        assert_eq!(&out[8..10], &[(-1.0, 0.0), (-2.0, 0.0)]);
    }

    #[test]
    fn assemble_handles_two_consecutive_refined_ranges_spanning_multiple_gops() {
        // 連続する遷移: group_consecutive がまとめた2本の連続範囲（GOP境界をまたぐ）
        // が、隙間のホールド区間を挟んで正しく上書きされることを確認する。
        let kf_frame_numbers = vec![0, 3, 6, 9, 12];
        let kf_scores = vec![
            (10.0, 0.0),
            (20.0, 0.0),
            (30.0, 0.0), // ホールドのまま残る中間GOP
            (40.0, 0.0),
            (50.0, 0.0),
        ];
        let refined = vec![
            // GOP0-1 (フレーム0..6) を1回のデコードで精緻化。
            RefinedRange {
                start_frame: 0,
                scores: vec![
                    (1.0, 0.0),
                    (1.0, 0.0),
                    (1.0, 0.0),
                    (2.0, 0.0),
                    (2.0, 0.0),
                    (2.0, 0.0),
                ],
            },
            // GOP3-4 (フレーム9..15) を1回のデコードで精緻化。
            RefinedRange {
                start_frame: 9,
                scores: vec![
                    (4.0, 0.0),
                    (4.0, 0.0),
                    (4.0, 0.0),
                    (5.0, 0.0),
                    (5.0, 0.0),
                    (5.0, 0.0),
                ],
            },
        ];
        let out = assemble_full_scores(&kf_frame_numbers, &kf_scores, 15, &refined);
        assert_eq!(
            &out[0..6],
            &[
                (1.0, 0.0),
                (1.0, 0.0),
                (1.0, 0.0),
                (2.0, 0.0),
                (2.0, 0.0),
                (2.0, 0.0)
            ]
        );
        // 中間GOP(フレーム6..9)は精緻化されず、ホールドのまま。
        assert_eq!(&out[6..9], &[(30.0, 0.0); 3]);
        assert_eq!(
            &out[9..15],
            &[
                (4.0, 0.0),
                (4.0, 0.0),
                (4.0, 0.0),
                (5.0, 0.0),
                (5.0, 0.0),
                (5.0, 0.0)
            ]
        );
    }

    #[test]
    #[should_panic(expected = "先頭フレームは常にキーフレームである前提")]
    fn assemble_panics_if_first_keyframe_is_not_frame_zero() {
        assemble_full_scores(&[1, 5], &[(0.0, 0.0), (0.0, 0.0)], 10, &[]);
    }

    #[test]
    #[should_panic(expected = "全フレーム数を超えています")]
    fn assemble_panics_if_refined_range_overruns_total_frames() {
        let refined = vec![RefinedRange {
            start_frame: 8,
            scores: vec![(0.0, 0.0), (0.0, 0.0), (0.0, 0.0)],
        }];
        assemble_full_scores(&[0], &[(0.0, 0.0)], 10, &refined);
    }

    // ---------------------------------------------------------------
    // touched_frame_ranges_from_logoframe_text（issue #154 レビュー指摘、
    // blocker1「不動点化」）
    // ---------------------------------------------------------------

    #[test]
    fn touched_frame_ranges_parses_s_and_e_lines() {
        // build_text が書く形式そのまま: `<best> S 0 ALL <start> <end>`。
        let text = "   100 S 0 ALL    90   110\n   250 E 0 ALL   240   260\n";
        let ranges = touched_frame_ranges_from_logoframe_text(text);
        assert_eq!(ranges, vec![(90, 110), (240, 260)]);
    }

    #[test]
    fn touched_frame_ranges_is_empty_for_empty_text() {
        assert_eq!(touched_frame_ranges_from_logoframe_text(""), Vec::new());
    }

    #[test]
    fn touched_frame_ranges_ignores_malformed_lines() {
        // 列数が違う行、数値でない列、範囲が反転している行（start > end）は
        // 無視し、他の正常な行は取り出す。
        let text = "not enough columns\n\
                     100 S 0 ALL abc 110\n\
                     100 S 0 ALL 110 90\n\
                     100 S 0 ALL 90 110\n";
        assert_eq!(
            touched_frame_ranges_from_logoframe_text(text),
            vec![(90, 110)]
        );
    }

    #[test]
    fn touched_frame_ranges_ignores_negative_columns() {
        // build_text はマイナス値（`e_starti` 等が -1 になるケース）を出すことが
        // ある。負の範囲は「触れたフレーム」として意味を持たないため無視する。
        let text = "5 E 0 ALL -1 -1\n";
        assert_eq!(touched_frame_ranges_from_logoframe_text(text), Vec::new());
    }

    // ---------------------------------------------------------------
    // gops_overlapping_ranges（issue #154 レビュー指摘、blocker1「不動点化」）
    // ---------------------------------------------------------------

    #[test]
    fn gops_overlapping_ranges_finds_single_overlapping_gop() {
        let gops = build_gops(&[0, 120, 240], 300);
        // フレーム150は2番目のGOP [120,240) の内側。
        assert_eq!(gops_overlapping_ranges(&gops, &[(150, 150)]), vec![1]);
    }

    #[test]
    fn gops_overlapping_ranges_finds_multiple_gops_spanned_by_one_range() {
        let gops = build_gops(&[0, 120, 240], 300);
        // フレーム範囲[100,250]は3つのGOP全部に重なる。
        assert_eq!(gops_overlapping_ranges(&gops, &[(100, 250)]), vec![0, 1, 2]);
    }

    #[test]
    fn gops_overlapping_ranges_unions_multiple_input_ranges() {
        let gops = build_gops(&[0, 120, 240], 300);
        assert_eq!(
            gops_overlapping_ranges(&gops, &[(10, 10), (250, 250)]),
            vec![0, 2]
        );
    }

    #[test]
    fn gops_overlapping_ranges_is_empty_for_no_ranges() {
        let gops = build_gops(&[0, 120, 240], 300);
        assert_eq!(gops_overlapping_ranges(&gops, &[]), Vec::<usize>::new());
    }

    #[test]
    fn gops_overlapping_ranges_boundary_frame_belongs_to_next_gop() {
        // GOP は半開区間 [start, end) なので、境界フレーム（次のGOPの先頭）は
        // 次のGOPにだけ属する。
        let gops = build_gops(&[0, 120, 240], 300);
        assert_eq!(gops_overlapping_ranges(&gops, &[(120, 120)]), vec![1]);
    }

    // ---------------------------------------------------------------
    // should_fall_back_to_full_decode
    // ---------------------------------------------------------------

    #[test]
    fn should_fall_back_to_full_decode_below_threshold_stays_hier() {
        // 10/100 = 10% < 40%。
        assert!(!should_fall_back_to_full_decode(10, 100, 0.4));
    }

    #[test]
    fn should_fall_back_to_full_decode_exactly_at_threshold_falls_back() {
        // 40/100 = 40% == 40%。閾値ちょうどは「以上」に含める（`>=`）。
        assert!(should_fall_back_to_full_decode(40, 100, 0.4));
    }

    #[test]
    fn should_fall_back_to_full_decode_above_threshold_falls_back() {
        // 90/100 = 90% > 40%。
        assert!(should_fall_back_to_full_decode(90, 100, 0.4));
    }

    #[test]
    fn should_fall_back_to_full_decode_zero_total_gops_is_false() {
        // GOPが1つも無い状態は呼び出し側の異常（別の検査で先に止まる想定）
        // だが、ここでは0除算せず判定不能として `false` を返す。
        assert!(!should_fall_back_to_full_decode(0, 0, 0.4));
    }

    #[test]
    fn should_fall_back_to_full_decode_zero_targets_stays_hier() {
        assert!(!should_fall_back_to_full_decode(0, 100, 0.4));
    }
}
