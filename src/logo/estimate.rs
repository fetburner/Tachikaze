//! 入力自身からロゴ矩形の候補列を推定し、本編/CM の在/不在信号で採点して選ぶ
//! （E18-2・E18-3）。
//!
//! `analyze --logo` は `.lgd` を必要とし、`.lgd` を作る `make-logo` はロゴ矩形を
//! `--rect x,y,w,h` で人手指定させている。位置を人間が測らないと何も始まらない。
//! [`estimate_candidates`] は「定常的な段差がある場所」の候補列を作り（E18-2、
//! 手順1〜7）、続けてその候補を本編/CM ラベルとの相関（AUC）で採点して採用列を
//! 決める（E18-3、下記「候補の採点」節）。採用列から実際に `make-logo` を試す
//! 直列の採用ループは別 issue #135 の担当（この関数は順序つきの採用列を返すだけ）。
//!
//! ## 原理: 半透明合成の隣接差分
//!
//! ロゴが乗った画素は `I_t = (1-a) B_t + a L` なので、隣接画素の符号つき差分は
//!
//! ```text
//! d_t(x,y) = I_t(x+1,y) - I_t(x,y) = (1-a)·dB_t + a·dL
//! ```
//!
//! となり、**毎フレーム同じ向き・同じ大きさの下駄 `a·dL` が乗る**。背景由来の項
//! `dB_t` は場面が変わるので期待値 0。標本を時系列で [`NUM_BLOCKS`] ブロックに
//! 等分すると、`a·dL` は全ブロックのブロック平均に同符号で現れるのに対し、
//! 静止ショットや特定の場面による偏りは一部のブロックにしか乗らない。したがって
//! 画素ごとの
//!
//! - **効果量** = ブロック平均 `mean_b(d)` のブロック横断の中央値（単位は階調）
//! - **符号一致率** = 中央値と同符号のブロックの割合
//!
//! が「ここに定常的な段差がある」の判定量になる。
//!
//! ## なぜ t 統計量ではなくブロック中央値か（罠）
//!
//! 初版の設計は `t = |mean|/(sd/√N)` だったが、**t は標本数の平方根で育つため
//! 閾値が標本数に依存する。** 実測: キーフレーム 451 枚で較正した `t >= 5.0` は、
//! 全フレーム 14386 枚では画面の 78.4〜88.4% が有意になり、ロゴがあるのに
//! 「ロゴ無し」と誤判定した（全フレームでは静止ショット内の隣接フレームが同じ
//! `dB` を繰り返し数えるため「背景項の期待値 0」も経験平均上で崩れる）。ブロック
//! 中央値は同じ入力で有意画素 0.09%・正しい矩形を返した。効果量は絶対単位（階調）
//! なので閾値が標本数に依存せず、キーフレーム約451枚でも全フレーム約14386枚でも
//! 同じ閾値ラダーが成立する。**この理由により、単純な t 検定に戻してはならない。**
//!
//! ## なぜ固定閾値ラダーで、適応閾値（種 × 0.25 等）を使わないか（罠）
//!
//! 実測: フジテレビ（L字放送）で L字帯の断片（158 階調）が種になり、適応閾値が
//! 半透明ロゴ（7 階調）を大きく超えてロゴが消えた。t 版では適応閾値が必要だった
//! （t の名目値が入力ごとに大きく振れるため）が、階調は絶対単位なので
//! [`THRESHOLD_LADDER`] のような固定のラダーが成立する。
//!
//! ## なぜラダーを複数段にするか（罠）
//!
//! 実測: テレビ朝日（4:3 再放送）では、最下段（1.5 階調）は時計・天気パネル・
//! ピラーボックス縁が繋がった大構造として全部落ち、成分なしになる。時計は
//! 12 階調以上の段でだけ天気パネルから分離して候補になり、CM 約 4.25 分の追加
//! 除去に繋がった。**最下段だけにしてはならない。**
//!
//! ## その他の罠
//!
//! - **「本編ブロックの平均 − CM 全体平均」のような、少数標本の平均を全ブロック
//!   共通の基準として引く変種を作ってはいけない。** 追検証で試して棄却した。
//!   CM 標本 77 枚程度の平均は busy な画素で ±3 階調超の雑音を持ち、それが
//!   全ブロック共通の下駄になって偽の符号一致を量産する（有意画素 42%）。
//!   本編/CM の情報は #133 が候補単位で使う。
//! - **符号一致率は効果量に採用した方向（水平/垂直）のものを使う。** 方向をまたいで
//!   「良い方の一致率」を取ると、効果量と一致率が別の方向を見て条件が緩む。
//! - **順位づけを画素数にしてはいけない。** 実測: D4DJ では画面下部の偽成分の方が
//!   画素数が多かった（4533〜8624 に対しロゴは 2376）が、統計量はロゴが5倍離れて
//!   大きかった。そのため段ごとの上位候補は最大効果量の降順で選ぶ。
//! - **ロゴは隙間で分裂する。** 実測: TOKYO MX 1 が 7px / 11px の隙間で3成分に
//!   割れた。併合しないと矩形がロゴの一部しか覆わない。
//! - **大構造の除去（膨張して塊単位で捨てる）と併合（膨張せずラベリングして
//!   上限内でのみ）は両方行う。** 実測: 膨張なしの上限判定だけでは台風テロップの
//!   断片や天気パネルの1枠が上限をすり抜けて1位になった。
//!
//! ## 標本数を事前に知らなくてよい理由
//!
//! [`estimate_candidates`] はフレーム供給関数を**2回**呼ぶ。1回目は標本総数を
//! 数えるだけ（コールバックは何もしない）、2回目でその総数を使ってブロック割当て
//! （`総標本数 × フレーム番号 / 16`）を行う。キーフレームだけを読む供給関数
//! （`frames::stream_keyframe_luma_frames`）は ffmpeg を起動し直すだけで、
//! キーフレーム限定デコードは軽いためこの2回呼びは許容する。**2回目が返す実際の
//! フレーム数は1回目の総数と一致することを明示的に検査する**（一致しないと
//! ブロック割当てが静かに歪む。CLAUDE.md 罠3の一般形と同じ「検査が無いと例外を
//! 飛ばさずに間違った結果が出る」パターンのため、無視できないエラーにしている）。
//!
//! ## 候補の採点（AUC、E18-3）
//!
//! 上記の手順1〜7は「定常的な段差がある場所」を強い順に並べるだけで、**定常
//! オーバーレイは局ロゴだけではない**（実測: 時計・天気パネル・L字帯・台風テロップ
//! が候補に混ざった。ロゴは4.5〜7.1階調と桁違いに弱いため強さでは選べない）。
//! ロゴの定義は「本編で出ていてCMで消える」なので、候補ごとに「フレームごとの
//! 在/不在スコア」を作り、**本編/CMラベルとの分離度（AUC、Mann–Whitney U /
//! n_本編 n_CM）**で採点する。
//!
//! - **在/不在スコア。** 候補ごとに、`raw_bbox` + 余白 [`RECT_MARGIN`] 内の
//!   **全標本平均の d 場**（水平・垂直、[`BlockAccumulator`] が溜めたブロック平均の
//!   重み付き平均から合成できるので追加の走査は不要）をテンプレートとし、各標本の
//!   同領域の d 場との正規化相関（cos 類似度）をその標本のスコアとする
//!   （[`build_template`]/[`compute_score`]）。フレームの再走査が要るのはこの
//!   スコア計算の1パスだけで、**全候補ぶんを1パスでまとめて計算する**
//!   （[`score_candidates`]。候補ごとに走査し直さない）。
//! - **採用列の決定**（[`select_by_auc`]）: CM標本が [`MIN_CM_SAMPLES_FOR_AUC`]
//!   枚以上なら `AUC >= `[`AUC_SELECT_THRESHOLD`]` の候補をAUC降順で返す（1つも
//!   無ければ空＝ロゴ無し）。CM標本がそれ未満なら、AUCを使わず**候補列の先頭
//!   （最大効果量が最大の候補）1つだけ**を返す（silence検出がCMを見つけられない
//!   入力こそロゴ検出の価値が最大なので、ここで棄権しない）。
//!
//! ### 分類器（`classify_sample`）の契約
//!
//! [`estimate_candidates`] は「流れてきた標本の通し番号（0始まり、`stream_frames`
//! が呼ぶ順）」から本編/CMを返すクロージャを受け取る。**この関数は標本の通し番号と
//! `.dtvi` のフレーム番号の対応を一切持たない**（GOP=120固定という CLAUDE.md
//! 「前提」を使って計算していない）。理由は2つ: (1) `stream_frames` に渡す供給関数
//! （典型的には [`crate::logo::frames::stream_keyframe_luma_frames`]）は
//! キーフレームだけを流すため、通し番号は「何番目のキーフレームか」であって
//! フレーム番号ではない。(2) その対応づけには実際のキーフレーム位置（mp4の
//! サンプル表の同期サンプル、または `.dtvi`）が要り、それは呼び出し側だけが持つ
//! 情報である。**対応がずれると本編のフレームがCM群に混ざったまま採点され、
//! 静かに間違った候補が選ばれる**（例外は飛ばない）。呼び出し側は実際の
//! キーフレーム位置から通し番号への対応を作ってから `classify_sample` を渡すこと。
//!
//! ### なぜ Welch t（本編/CM 2群の差分マップ）を使わないか（罠）
//!
//! 初版の主基準は画素単位の Welch t マップだったが、追検証で2つの欠陥が出た。
//! (1) **CM標本が少ないと退化する**: silence検出がCMを見つけられない入力では
//! CM標本が1枚まで減り、t マップが偽の位置に極端な値（実測 t=411）を出した。
//! 「CM標本0枚なら全フレーム版へフォールバック」という条件ではこの退化を防げない
//! （1枚でも退化する）。(2) 時計のような「本編でもCMでも出ているもの」を落とす／
//! 活かす判断は候補単位のAUCでしか行えない。画素単位の差分には「分のケタの平均差を
//! 拾う」偽陽性も観測された（実測 `291,94,89x43` に t=51）。**この理由により
//! Welch差分を復活させてはならない。**
//!
//! ### なぜAUC不採用のとき無条件採用へフォールバックしないか（罠）
//!
//! 「せっかく推定したから」と AUC が全滅したときに無条件採用へ落ちてはならない。
//! これは旧版の罠「差分が『ロゴ無し』と言ったときに全フレーム版へ落ちてはいけない」
//! と同じ思想で、AUCが候補単位で下した「本編/CMと相関しない」という判定を上書きして
//! しまう。フォールバックしてよいのは [`MIN_CM_SAMPLES_FOR_AUC`] 節で述べる
//! 「CM標本が少なすぎてAUC自体が作れない」場合だけである。
//!
//! ### なぜ CM標本20枚未満のAUCを信用しないか（[`MIN_CM_SAMPLES_FOR_AUC`] の根拠）
//!
//! 実測: CM標本1枚の入力では、断片（AUC 0.983）が全体のロゴ（0.946）を上回った。
//! 負例1枚のAUCは「その1枚をたまたま外したか」の二値でしかなく統計的な意味を
//! 持たない。20枚という下限は4局の実測から較正した値であり、これ未満では
//! AUCによる順位づけ自体を信用せず候補列の先頭を無条件で返す。
//!
//! ### AUCの上位は僅差で並ぶことがある（罠ではなく既知の性質）
//!
//! 実測: L字放送の局ではロゴ0.990に対しL字帯・台風テロップの断片が0.947〜0.985
//! だった（L字も本編にだけ出るので相関する）。順位の逆転はあり得るが、AUCが高い
//! 候補は定義上本編/CMと相関しており、誤採用しても既存の検出フレーム割合の絶対
//! 閾値と `auto` の gate が最終防御になる（時計が有用な入力での実証と同じ理屈）。

use std::collections::{BTreeMap, HashSet};

use crate::logo::frames::{LogoRect, VideoSize};
use crate::logo::scan::round_rect_to_even;

/// 標本を時系列で等分するブロック数（手順1）。ブロック横断の中央値・符号一致率の
/// 母数になる。
const NUM_BLOCKS: usize = 16;

/// 1ブロックに必要な最小標本数（手順3）。これ未満のブロックは中央値の計算対象から
/// 外す（標本が少なすぎるブロックの平均は信頼できない）。
const MIN_SAMPLES_PER_BLOCK: u64 = 5;

/// 有効ブロック（[`MIN_SAMPLES_PER_BLOCK`] 以上の標本を持つブロック）の最小個数
/// （手順3）。これ未満なら「短すぎる入力」として空の候補列を返す。中央値の頑健性は
/// ある程度の個数の標本（ここではブロック）が無いと成立しない。
const MIN_VALID_BLOCKS: usize = 8;

/// 画面外周から除外するマージン（画素、手順2）。放送素材の端には必ず定常段差が
/// 出る。実測: 1920x1080 で `x=1914..1917` に高 t の縦筋が出た。
const BORDER_MARGIN: u32 = 4;

/// 輝度の時間標準偏差がこの値未満の画素は「凍結領域」の疑いとして除外対象にする
/// （手順2、[`FROZEN_MEAN_THRESHOLD`] と同時に満たす場合のみ除外）。
const FROZEN_STD_THRESHOLD: f64 = 1.0;

/// 輝度の時間平均がこの値未満の画素は「黒帯」の疑いとして除外対象にする
/// （手順2、[`FROZEN_STD_THRESHOLD`] と同時に満たす場合のみ除外）。
const FROZEN_MEAN_THRESHOLD: f64 = 24.0;

/// ロゴ矩形の幅の上限（映像幅に対する比率、手順4）。実測（4局）でロゴは
/// `126x36` / `183x24` / `127x48` / `55x60` だったのに対し、偽陽性の外形は
/// L字帯 `1912x1068`・台風テロップ `136x369`・常設帯 `421x63`〜`432x104`・
/// ピラーボックス `1912x1068`。この比率で全ロゴが残り全偽陽性が落ちた。
const MAX_WIDTH_RATIO: f64 = 0.20;

/// ロゴ矩形の高さの上限（映像高さに対する比率、手順4）。根拠は [`MAX_WIDTH_RATIO`]
/// と同じ実測。
const MAX_HEIGHT_RATIO: f64 = 0.15;

/// ロゴ矩形の辺の下限（画素、手順4）。1画素幅の端ノイズを落とすため。追検証では
/// `85x1` のような線状の偽成分もこの下限で落ちる。
const MIN_SIDE: u32 = 8;

/// 成分の**有意画素数**の下限（手順4）。bbox の面積（幅×高さ）に適用すると
/// `MIN_SIDE`（8x8=64）を満たせば自動的に20以上になり恒真の判定になってしまう
/// （実際に起きた回帰: 1080pの段48で有意画素数2〜3個の点状成分が bbox は
/// 小さくても上位3枠を占め、段24では枠が埋まって本物のロゴが押し出される寸前
/// だった）。bbox が広くても実際に閾値を超えた画素が少ない「点状の偽成分」を
/// 落とすため、成分に属する画素数（[`Component::pixels`] の長さ、併合後は
/// 併合元の合計）に適用する。
const MIN_AREA: usize = 20;

/// 閾値ラダー（階調、手順5）。実測で決めた固定値。**適応閾値（種の値 × 定数）に
/// してはならない**（モジュール doc comment「なぜ固定閾値ラダーか」参照）。
const THRESHOLD_LADDER: [f32; 7] = [1.5, 3.0, 6.0, 12.0, 24.0, 48.0, 96.0];

/// 符号一致率の下限（手順5）。
const SIGN_AGREEMENT_MIN: f32 = 0.8;

/// 段ごとにプールする上位候補数（手順5）。「最大効果量の降順」で選ぶ
/// （画素数で選んではならない、モジュール doc comment「その他の罠」参照）。
const TOP_K_PER_RUNG: usize = 3;

/// 大構造除去のための膨張半径（画素、手順6-1）。
const DILATE_RADIUS: usize = 8;

/// 併合の対象にする bbox 間の最大ギャップ（画素、縦横とも、手順6-3）。
const MERGE_GAP: u32 = 16;

/// 推定矩形を作る際に bbox の外側へ足す余白（画素、手順7）。
const RECT_MARGIN: u32 = 8;

/// 重複除去のために座標・寸法を丸める単位（画素、手順7）。
const DEDUP_ROUND: u32 = 8;

/// CM標本数がこの値以上のときだけAUCによる採点を信用する（手順4）。モジュール
/// doc comment「なぜCM標本20枚未満のAUCを信用しないか」参照（4局の実測からの
/// 較正値）。
const MIN_CM_SAMPLES_FOR_AUC: u64 = 20;

/// AUCがこの値以上の候補だけを採用列に入れる（手順4）。
const AUC_SELECT_THRESHOLD: f64 = 0.9;

/// フレームごとのコールバック（輝度平面を受け取り、成否を返す）。
/// [`estimate_candidates`] の `stream_frames` 引数が受け取る型を名付けたもの
/// （clippy `type_complexity` 対策。裸で書くと `&mut dyn FnMut(&[u8]) ->
/// anyhow::Result<()>` がシグネチャに毎回並び、複雑さの警告になる）。
type FrameCallback<'a> = dyn FnMut(&[u8]) -> anyhow::Result<()> + 'a;

/// 標本（[`estimate_candidates`] の `stream_frames` が流す1フレーム）の本編/CM
/// ラベル。`classify_sample` 引数が返す型（モジュール doc comment「分類器
/// （`classify_sample`）の契約」参照）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleLabel {
    /// 本編（CM以外）の標本。
    Program,
    /// CMの標本。
    Cm,
}

/// ロゴ矩形の候補。[`estimate_candidates`] が返す採用列では、AUCで採点できた場合は
/// AUC降順、できなかった場合（CM標本が少ない）は候補列の先頭1つだけになる
/// （モジュール doc comment「候補の採点」参照）。
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    /// bbox の外側に余白 [`RECT_MARGIN`] 画素を足し、映像範囲でクランプしてから
    /// `round_rect_to_even` で2の倍数に丸めた矩形。`--rect` にそのまま渡せる形。
    pub estimated_rect: LogoRect,
    /// 後処理（大構造除去・併合・上限下限）を終えた成分の生の bbox（丸め・余白なし）。
    pub raw_bbox: LogoRect,
    /// 成分内の画素が持つ効果量（絶対値、階調）の最大値。
    pub max_effect: f32,
    /// 成分に属する有意画素数。
    pub significant_pixels: usize,
    /// この候補を生成した閾値ラダーの段（階調）。
    pub rung_threshold: f32,
}

/// 映像サイズと「キーフレームを流す供給関数」と「標本の本編/CM分類器」から
/// ロゴ矩形の採用列を推定する。
///
/// `stream_frames` は「フレームごとのコールバックを受け取り、実際に流した
/// フレーム数を返す関数」を渡す。呼び出し側の典型的な使い方（実際の ffmpeg 経由）:
///
/// ```text
/// estimate_candidates(video_size, |on_frame| {
///     frames::stream_keyframe_luma_frames(&ffmpeg, &input, &cwd, video_size, on_frame)
/// }, classify_sample)
/// ```
///
/// `classify_sample` は「`stream_frames` が呼ぶ順に0始まりで振った標本の通し番号」
/// を受け取り [`SampleLabel`] を返すクロージャ。**標本の通し番号から時刻や
/// `.dtvi` のフレーム番号への対応づけは呼び出し側の責務**（この関数はその対応を
/// 一切持たない。理由と、対応をGOP=120の仮定で計算してはならない理由はモジュール
/// doc comment「分類器（`classify_sample`）の契約」参照）。
///
/// この関数は `stream_frames` を最大3回呼ぶ: 1・2回目は候補列の生成
/// （理由はモジュール doc comment「標本数を事前に知らなくてよい理由」参照）、
/// 3回目はAUC採点のスコア計算（CM標本が[`MIN_CM_SAMPLES_FOR_AUC`]枚以上あるときのみ、
/// モジュール doc comment「候補の採点」参照）。`video_size` の幅・高さが 0 の場合や
/// 標本が1枚も無い場合、候補列が生成されなかった場合は3回目を呼ばずに空の採用列を
/// 返す。
pub fn estimate_candidates(
    video_size: VideoSize,
    mut stream_frames: impl FnMut(&mut FrameCallback<'_>) -> anyhow::Result<u64>,
    classify_sample: impl Fn(u64) -> SampleLabel,
) -> anyhow::Result<Vec<Candidate>> {
    let Some(raw) = estimate_raw(video_size, &mut stream_frames)? else {
        return Ok(Vec::new());
    };
    if raw.candidates.is_empty() {
        return Ok(raw.candidates);
    }

    select_by_auc(
        raw.w,
        raw.h,
        &raw.acc,
        raw.candidates,
        raw.total,
        &mut stream_frames,
        &classify_sample,
    )
}

/// [`estimate_raw`] の結果（手順1〜7で作った候補列と、その元になった集計）。
/// AUC採点（手順4〜5、[`select_by_auc`]）に必要な `acc`/`w`/`h`/`total` を
/// 候補列と一緒に持ち越す（[`BlockAccumulator`] を2回目の `stream_frames` 呼び出し
/// だけで作るのと同じ理由で、AUC採点のために集計をやり直さない）。
struct RawEstimate {
    w: usize,
    h: usize,
    total: u64,
    acc: BlockAccumulator,
    candidates: Vec<Candidate>,
}

/// [`estimate_candidates`] の手順1〜7（候補列の生成、E18-2）。`stream_frames` を
/// 2回呼ぶ（理由はモジュール doc comment「標本数を事前に知らなくてよい理由」参照）。
/// `video_size` の幅・高さが0の場合や標本が1枚も無い場合は `None`。
///
/// AUC採点（E18-3）を経由しないこの関数の出力単体を検証するテストは
/// `#[cfg(test)] mod tests` の `estimate_raw_candidates` から呼ぶ（構造的な候補
/// 生成とAUC採点は独立に検証する）。
fn estimate_raw(
    video_size: VideoSize,
    stream_frames: &mut impl FnMut(&mut FrameCallback<'_>) -> anyhow::Result<u64>,
) -> anyhow::Result<Option<RawEstimate>> {
    let w = video_size.width as usize;
    let h = video_size.height as usize;
    if w == 0 || h == 0 {
        return Ok(None);
    }

    let total = stream_frames(&mut |_frame: &[u8]| Ok(()))?;
    if total == 0 {
        return Ok(None);
    }

    let mut acc = BlockAccumulator::new(w, h, total);
    let actual = stream_frames(&mut |frame: &[u8]| {
        acc.add_frame(frame);
        Ok(())
    })?;
    anyhow::ensure!(
        actual == total,
        "フレーム供給関数が1回目({total}枚)と2回目({actual}枚)で異なる枚数を返しました。\
         ブロック割当て（総標本数 × フレーム番号 / 16）は2回とも同じ枚数が流れることを\
         前提にしており、食い違うと統計量が静かに歪みます。"
    );
    acc.finish();

    let candidates = build_candidates(w, h, video_size, &acc);
    Ok(Some(RawEstimate {
        w,
        h,
        total,
        acc,
        candidates,
    }))
}

/// [`estimate_candidates`] の手順4〜5（採点・採用列の決定・ログ）。候補列が
/// 空でないことは呼び出し側が保証する。
fn select_by_auc(
    w: usize,
    h: usize,
    acc: &BlockAccumulator,
    candidates: Vec<Candidate>,
    total: u64,
    stream_frames: &mut impl FnMut(&mut FrameCallback<'_>) -> anyhow::Result<u64>,
    classify_sample: &impl Fn(u64) -> SampleLabel,
) -> anyhow::Result<Vec<Candidate>> {
    // CM標本数だけをまず数える(フレームの再走査なしで判定できる。手順4の分岐は
    // この数だけで決まるため、下限未満ならAUC採点用の3回目のストリーミングを
    // 呼ばずに済む。「フレームの再走査が要るのはスコア計算の1パスだけ」という
    // 性能上の前提を守るため、不要な走査は増やさない)。
    let cm_count = (0..total)
        .filter(|&i| classify_sample(i) == SampleLabel::Cm)
        .count() as u64;
    let program_count = total - cm_count;
    eprintln!(
        "[logo-estimate] 標本の本編/CM分類: 本編{program_count}枚 CM{cm_count}枚 \
         (合計{total}枚)"
    );

    if cm_count < MIN_CM_SAMPLES_FOR_AUC {
        // モジュール doc comment「なぜCM標本20枚未満のAUCを信用しないか」参照。
        let top = candidates[0].clone();
        eprintln!(
            "[logo-estimate] CM標本が{cm_count}枚(下限{MIN_CM_SAMPLES_FOR_AUC}枚)未満のため、\
             AUCを計算せず候補列の先頭(最大効果量={:.1})を採用します: \
             raw_bbox=(x={}, y={}, w={}, h={})",
            top.max_effect, top.raw_bbox.x, top.raw_bbox.y, top.raw_bbox.w, top.raw_bbox.h,
        );
        return Ok(vec![top]);
    }

    let templates: Vec<Option<CandidateTemplate>> = candidates
        .iter()
        .map(|c| build_template(w, h, acc, bbox_from_rect(c.raw_bbox)))
        .collect();

    let (program_scores, cm_scores) =
        score_candidates(w, &templates, stream_frames, classify_sample, total)?;

    let mut scored: Vec<(Candidate, f64)> = candidates
        .into_iter()
        .zip(program_scores.into_iter().zip(cm_scores))
        .map(|(candidate, (program, cm))| {
            let auc = auc_from_scores(&program, &cm);
            eprintln!(
                "[logo-estimate] 候補 raw_bbox=(x={}, y={}, w={}, h={}) 最大効果量={:.1}: \
                 AUC={auc:.3} (本編標本{}枚 CM標本{}枚)",
                candidate.raw_bbox.x,
                candidate.raw_bbox.y,
                candidate.raw_bbox.w,
                candidate.raw_bbox.h,
                candidate.max_effect,
                program.len(),
                cm.len(),
            );
            (candidate, auc)
        })
        .collect();

    // AUC >= 閾値の候補だけをAUC降順で残す（同値タイは最大効果量降順、さらに
    // bbox座標で決定的に破る。`build_candidates` の並び順と同じ流儀）。
    scored.retain(|(_, auc)| *auc >= AUC_SELECT_THRESHOLD);
    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| {
                b.0.max_effect
                    .partial_cmp(&a.0.max_effect)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .then_with(|| a.0.raw_bbox.x.cmp(&b.0.raw_bbox.x))
            .then_with(|| a.0.raw_bbox.y.cmp(&b.0.raw_bbox.y))
            .then_with(|| a.0.raw_bbox.w.cmp(&b.0.raw_bbox.w))
            .then_with(|| a.0.raw_bbox.h.cmp(&b.0.raw_bbox.h))
    });

    eprintln!(
        "[logo-estimate] 採用列(AUC>={AUC_SELECT_THRESHOLD}): {}個",
        scored.len()
    );
    for (rank, (c, auc)) in scored.iter().enumerate() {
        eprintln!(
            "[logo-estimate] 採用{}: raw_bbox=(x={}, y={}, w={}, h={}) AUC={auc:.3}",
            rank + 1,
            c.raw_bbox.x,
            c.raw_bbox.y,
            c.raw_bbox.w,
            c.raw_bbox.h,
        );
    }

    Ok(scored.into_iter().map(|(c, _)| c).collect())
}

/// [`LogoRect`] を内部の [`Bbox`]（両端含む座標）へ変換する。
fn bbox_from_rect(rect: LogoRect) -> Bbox {
    Bbox {
        x_min: rect.x,
        y_min: rect.y,
        x_max: rect.x + rect.w - 1,
        y_max: rect.y + rect.h - 1,
    }
}

/// 候補1つのスコアリング領域（画素座標、両端を含む）。
#[derive(Debug, Clone, Copy)]
struct ScoreRegion {
    x_min: usize,
    y_min: usize,
    x_max: usize,
    y_max: usize,
}

/// `bbox` + 余白 [`RECT_MARGIN`] を、[`BlockAccumulator`] がブロック平均を実際に
/// 集計した範囲（外周 [`BORDER_MARGIN`] を除いた内側、`build_effect_maps` と同じ
/// 範囲）にクランプする。集計範囲外は `block_mean_h`/`block_mean_v` が既定値の
/// 0 のままで、テンプレートに含めると偽の下駄になるため範囲外に出さない。
///
/// 範囲が空になる（`bbox` が集計範囲の外にある等）場合は `None` を返す。
fn candidate_score_region(bbox: Bbox, w: usize, h: usize) -> Option<ScoreRegion> {
    let border = BORDER_MARGIN as usize;
    if w <= 2 * border + 1 || h <= 2 * border + 1 {
        return None;
    }
    let lo_x = border;
    let hi_x = w - 1 - border;
    let lo_y = border;
    let hi_y = h - 1 - border;
    let margin = RECT_MARGIN as usize;

    let x_min = (bbox.x_min as usize).saturating_sub(margin).max(lo_x);
    let y_min = (bbox.y_min as usize).saturating_sub(margin).max(lo_y);
    let x_max = (bbox.x_max as usize + margin).min(hi_x);
    let y_max = (bbox.y_max as usize + margin).min(hi_y);
    if x_min > x_max || y_min > y_max {
        return None;
    }
    Some(ScoreRegion {
        x_min,
        y_min,
        x_max,
        y_max,
    })
}

/// 候補1つの在/不在スコア計算に使うテンプレート（モジュール doc comment
/// 「候補の採点」参照）。
struct CandidateTemplate {
    region: ScoreRegion,
    /// `region`内を行優先（`(y - region.y_min) * region幅 + (x - region.x_min)`）
    /// で並べた、全標本平均の水平差分場。
    template_h: Vec<f32>,
    /// `template_h` と同じ並びの垂直差分場。
    template_v: Vec<f32>,
    /// `(template_h, template_v)` を連結したベクトルのノルム。0 ならテンプレートが
    /// 全ゼロ（この領域に段差が無い）ことを意味し、[`compute_score`] はcos類似度を
    /// 定義できないとみなして0を返す。
    norm: f64,
}

/// `bbox` の [`candidate_score_region`] 内で、[`BlockAccumulator`] が溜めた
/// ブロック平均を標本数で重み付き平均し、テンプレートを合成する
/// （モジュール doc comment「候補の採点」の「在/不在スコア」参照。追加の
/// フレーム走査は不要）。範囲が作れない場合は `None`。
fn build_template(
    w: usize,
    h: usize,
    acc: &BlockAccumulator,
    bbox: Bbox,
) -> Option<CandidateTemplate> {
    let region = candidate_score_region(bbox, w, h)?;
    let total_count: u64 = acc.block_sample_count.iter().sum();
    if total_count == 0 {
        return None;
    }

    let wh = w * h;
    let region_w = region.x_max - region.x_min + 1;
    let region_h = region.y_max - region.y_min + 1;
    let mut template_h = vec![0.0f32; region_w * region_h];
    let mut template_v = vec![0.0f32; region_w * region_h];

    for ry in 0..region_h {
        let y = region.y_min + ry;
        for rx in 0..region_w {
            let x = region.x_min + rx;
            let idx = y * w + x;
            let mut sum_h = 0.0f64;
            let mut sum_v = 0.0f64;
            for b in 0..NUM_BLOCKS {
                let count = acc.block_sample_count[b];
                if count == 0 {
                    continue;
                }
                sum_h += acc.block_mean_h[b * wh + idx] as f64 * count as f64;
                sum_v += acc.block_mean_v[b * wh + idx] as f64 * count as f64;
            }
            let ti = ry * region_w + rx;
            template_h[ti] = (sum_h / total_count as f64) as f32;
            template_v[ti] = (sum_v / total_count as f64) as f32;
        }
    }

    let norm_sq: f64 = template_h
        .iter()
        .zip(template_v.iter())
        .map(|(&th, &tv)| (th as f64).powi(2) + (tv as f64).powi(2))
        .sum();

    Some(CandidateTemplate {
        region,
        template_h,
        template_v,
        norm: norm_sq.sqrt(),
    })
}

/// 1標本(1フレーム)の在/不在スコア。`template` の領域内で標本の d 場との
/// cos類似度を計算する。`template.norm` が0（段差なし）か、この標本のその領域の
/// d場がすべて0（分散なし）ならcos類似度を定義できないため0を返す
/// （モジュール doc comment「候補の採点」参照）。
fn compute_score(w: usize, template: &CandidateTemplate, frame: &[u8]) -> f32 {
    let region = &template.region;
    let region_w = region.x_max - region.x_min + 1;
    let region_h = region.y_max - region.y_min + 1;

    let mut dot = 0.0f64;
    let mut sample_norm_sq = 0.0f64;
    for ry in 0..region_h {
        let y = region.y_min + ry;
        let row_base = y * w;
        for rx in 0..region_w {
            let x = region.x_min + rx;
            let idx = row_base + x;
            let v = frame[idx] as f64;
            let dh = frame[idx + 1] as f64 - v;
            let dv = frame[idx + w] as f64 - v;
            let ti = ry * region_w + rx;
            dot += template.template_h[ti] as f64 * dh + template.template_v[ti] as f64 * dv;
            sample_norm_sq += dh * dh + dv * dv;
        }
    }

    if template.norm > 0.0 && sample_norm_sq > 0.0 {
        (dot / (template.norm * sample_norm_sq.sqrt())) as f32
    } else {
        0.0
    }
}

/// [`estimate_candidates`] のAUC採点用3回目の `stream_frames` 呼び出し。
/// **全候補ぶんを1パスでまとめて計算する**（モジュール doc comment「候補の採点」の
/// 「在/不在スコア」参照。候補ごとに走査し直さない）。`templates[i]` が `None` の
/// 候補（[`candidate_score_region`] が範囲を作れなかった、実運用ではまず起きない
/// 縁ケース）はスコアを計算せず、その候補の本編/CM双方の標本列が空のまま返る
/// （[`auc_from_scores`] が空集合をNaNとして扱い、結果として採用列から自動的に
/// 除外される）。
///
/// 候補ごとの標本スコア列（[`score_candidates`] の戻り値の要素。clippy
/// `type_complexity` 対策で [`FrameCallback`] と同じ理由により名付ける）。
type PerCandidateScores = Vec<Vec<f32>>;

/// 戻り値は `(候補ごとの本編標本スコア列, 候補ごとのCM標本スコア列)`。
fn score_candidates(
    w: usize,
    templates: &[Option<CandidateTemplate>],
    stream_frames: &mut impl FnMut(&mut FrameCallback<'_>) -> anyhow::Result<u64>,
    classify_sample: &impl Fn(u64) -> SampleLabel,
    total: u64,
) -> anyhow::Result<(PerCandidateScores, PerCandidateScores)> {
    let mut program_scores: Vec<Vec<f32>> = vec![Vec::new(); templates.len()];
    let mut cm_scores: Vec<Vec<f32>> = vec![Vec::new(); templates.len()];
    let mut serial: u64 = 0;

    let actual = stream_frames(&mut |frame: &[u8]| {
        let label = classify_sample(serial);
        for (idx, template_opt) in templates.iter().enumerate() {
            if let Some(template) = template_opt {
                let score = compute_score(w, template, frame);
                match label {
                    SampleLabel::Program => program_scores[idx].push(score),
                    SampleLabel::Cm => cm_scores[idx].push(score),
                }
            }
        }
        serial += 1;
        Ok(())
    })?;
    anyhow::ensure!(
        actual == total,
        "フレーム供給関数がAUC採点用の3回目の呼び出しで{actual}枚を返しましたが、\
         1・2回目は{total}枚でした。ブロック割当てと同じ理由で、3回とも同じ枚数が\
         流れることを前提にしています。"
    );

    Ok((program_scores, cm_scores))
}

/// Mann–Whitney U から AUC（本編群からランダムに選んだ標本のスコアがCM群から
/// ランダムに選んだ標本のスコアより大きい確率、同値は0.5として数える）を計算する。
/// 同値タイは平均順位で処理する（`program_scores`/`cm_scores`のいずれかが空の場合は
/// 定義できないため `f64::NAN` を返す。呼び出し側は `NAN >= 閾値` が常に `false`に
/// なることを利用して自動的に除外する）。
fn auc_from_scores(program_scores: &[f32], cm_scores: &[f32]) -> f64 {
    let n1 = program_scores.len();
    let n2 = cm_scores.len();
    if n1 == 0 || n2 == 0 {
        return f64::NAN;
    }

    // group=0: 本編, group=1: CM。
    let mut combined: Vec<(f32, u8)> = Vec::with_capacity(n1 + n2);
    combined.extend(program_scores.iter().map(|&s| (s, 0u8)));
    combined.extend(cm_scores.iter().map(|&s| (s, 1u8)));
    combined.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let n = combined.len();
    let mut ranks = vec![0.0f64; n];
    let mut i = 0;
    while i < n {
        let mut j = i;
        while j + 1 < n && combined[j + 1].0 == combined[i].0 {
            j += 1;
        }
        // 0始まりの範囲 i..=j に、1始まりの平均順位を割る(タイの標準的な処理)。
        let avg_rank = ((i + 1) + (j + 1)) as f64 / 2.0;
        for r in ranks.iter_mut().take(j + 1).skip(i) {
            *r = avg_rank;
        }
        i = j + 1;
    }

    let r1: f64 = combined
        .iter()
        .zip(ranks.iter())
        .filter(|((_, group), _)| *group == 0)
        .map(|(_, &rank)| rank)
        .sum();
    let u1 = r1 - (n1 as f64) * ((n1 as f64) + 1.0) / 2.0;
    u1 / (n1 as f64 * n2 as f64)
}

/// フレーム番号 `i`（0始まり、`i < total`）が属するブロック番号
/// （`総標本数 × ブロック番号 / 16` の割当て、手順1）。
fn block_of(i: u64, total: u64) -> usize {
    if total == 0 {
        return 0;
    }
    let b = (i as u128 * NUM_BLOCKS as u128) / total as u128;
    // `stream_frames` の1回目と2回目が異なるフレーム数を返した場合に配列外
    // アクセスへ落ちないための防御的なクランプ（本来 i < total なら b < 16 のはず）。
    (b as usize).min(NUM_BLOCKS - 1)
}

/// フレームを1枚ずつ受け取り、画素ごとの水平・垂直差分のブロック別平均と、
/// 輝度そのものの1次・2次モーメントを溜めるアキュムレータ（手順1）。
///
/// ブロック平均の保存は f32 で16面×2方向で `w*h` に比例するメモリを使う
/// （1920x1080 で約265MB）。ブロックは時系列で連続なので、走査中は現在ブロックの
/// 和だけを `f64` で持ち、ブロックが締まるたびに平均へ畳んで捨てる
/// （[`BlockAccumulator::close_block`]）。
struct BlockAccumulator {
    w: usize,
    h: usize,
    total: u64,
    frames_seen: u64,
    current_block: usize,
    current_count: u64,
    current_sum_h: Vec<f64>,
    current_sum_v: Vec<f64>,
    /// `NUM_BLOCKS * w*h` 要素。ブロック `b` の画素 `idx` は `b*w*h + idx`。
    block_mean_h: Vec<f32>,
    block_mean_v: Vec<f32>,
    block_sample_count: [u64; NUM_BLOCKS],
    luma_sum: Vec<f64>,
    luma_sum2: Vec<f64>,
    finished: bool,
}

impl BlockAccumulator {
    fn new(w: usize, h: usize, total: u64) -> Self {
        let wh = w * h;
        BlockAccumulator {
            w,
            h,
            total,
            frames_seen: 0,
            current_block: 0,
            current_count: 0,
            current_sum_h: vec![0.0; wh],
            current_sum_v: vec![0.0; wh],
            block_mean_h: vec![0.0; NUM_BLOCKS * wh],
            block_mean_v: vec![0.0; NUM_BLOCKS * wh],
            block_sample_count: [0; NUM_BLOCKS],
            luma_sum: vec![0.0; wh],
            luma_sum2: vec![0.0; wh],
            finished: false,
        }
    }

    /// `frame` は `w*h` バイトの輝度平面（`frames::stream_keyframe_luma_frames`
    /// が渡す形式そのまま）。
    fn add_frame(&mut self, frame: &[u8]) {
        debug_assert_eq!(
            frame.len(),
            self.w * self.h,
            "frame のバイト数が w*h と不一致"
        );
        if self.total == 0 {
            return;
        }

        let b = block_of(self.frames_seen, self.total);
        if b != self.current_block {
            self.close_block();
            self.current_block = b;
        }

        let (w, h, border) = (self.w, self.h, BORDER_MARGIN as usize);
        // 外周 `border` 画素を除いた内側だけを走査する。手順2の外周除外は「候補から
        // 外す」だけでなく、ここでは差分の添字が範囲内に収まることも保証する
        // （`x+1<w`、`y+1<h` は `border>=1` かつ内側の範囲から常に成り立つ）。
        if w > 2 * border && h > 2 * border {
            for y in border..h - border {
                for x in border..w - border {
                    let idx = y * w + x;
                    let v = frame[idx] as f64;
                    self.luma_sum[idx] += v;
                    self.luma_sum2[idx] += v * v;
                    self.current_sum_h[idx] += frame[idx + 1] as f64 - v;
                    self.current_sum_v[idx] += frame[idx + w] as f64 - v;
                }
            }
        }

        self.current_count += 1;
        self.frames_seen += 1;
    }

    /// 現在のブロックの和を平均へ畳んで `block_mean_h`/`block_mean_v` に格納し、
    /// 標本数を `block_sample_count` に記録してから、次のブロックのために和を
    /// リセットする。
    fn close_block(&mut self) {
        self.block_sample_count[self.current_block] = self.current_count;
        if self.current_count > 0 {
            let n = self.current_count as f64;
            let wh = self.w * self.h;
            let base = self.current_block * wh;
            for idx in 0..wh {
                self.block_mean_h[base + idx] = (self.current_sum_h[idx] / n) as f32;
                self.block_mean_v[base + idx] = (self.current_sum_v[idx] / n) as f32;
            }
        }
        self.current_sum_h.iter_mut().for_each(|v| *v = 0.0);
        self.current_sum_v.iter_mut().for_each(|v| *v = 0.0);
        self.current_count = 0;
    }

    /// 最後に開いていたブロックを締める。`estimate_candidates` はフレームを流し
    /// 終えた直後に1回だけ呼ぶ（2回目以降は何もしない。誤って2回呼んでも
    /// `block_sample_count` を壊さないための安全策）。
    fn finish(&mut self) {
        if !self.finished {
            self.close_block();
            self.finished = true;
        }
    }
}

/// 効果量マップ・符号一致率マップ・有効マスク（手順2・3）。
struct EffectMaps {
    /// 選ばれた方向（水平/垂直のうち絶対値が大きい方）の中央値（符号付き、階調）。
    effect: Vec<f32>,
    /// 選ばれた方向の符号一致率。
    sign_rate: Vec<f32>,
    /// 外周除外・凍結領域除外を通ったか（`true` なら候補になり得る）。
    valid: Vec<bool>,
}

fn sign_of(v: f32) -> i32 {
    if v > 0.0 {
        1
    } else if v < 0.0 {
        -1
    } else {
        0
    }
}

/// `values` を昇順に並べて中央値を返す（偶数個なら中央2個の平均）。
fn median(values: &mut [f32]) -> f32 {
    if values.is_empty() {
        return 0.0;
    }
    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let n = values.len();
    if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    }
}

/// `median()` が返した符号と同じ符号を持つ値の割合。中央値が 0（符号なし）の場合は
/// 「一致」を主張できないため 0 を返す。
fn sign_agreement(values: &[f32], median_value: f32) -> f32 {
    let target = sign_of(median_value);
    if target == 0 || values.is_empty() {
        return 0.0;
    }
    let agree = values.iter().filter(|&&v| sign_of(v) == target).count();
    agree as f32 / values.len() as f32
}

fn build_effect_maps(
    w: usize,
    h: usize,
    valid_blocks: &[usize],
    acc: &BlockAccumulator,
) -> EffectMaps {
    let wh = w * h;
    let mut effect = vec![0.0f32; wh];
    let mut sign_rate = vec![0.0f32; wh];
    let mut valid = vec![false; wh];

    let border = BORDER_MARGIN as usize;
    if w <= 2 * border || h <= 2 * border {
        return EffectMaps {
            effect,
            sign_rate,
            valid,
        };
    }

    let n = acc.frames_seen as f64;
    if n <= 0.0 {
        return EffectMaps {
            effect,
            sign_rate,
            valid,
        };
    }

    let mut h_vals = vec![0.0f32; valid_blocks.len()];
    let mut v_vals = vec![0.0f32; valid_blocks.len()];

    for y in border..h - border {
        for x in border..w - border {
            let idx = y * w + x;

            // 凍結領域・黒帯の除外(手順2)。
            let mean = acc.luma_sum[idx] / n;
            let var = (acc.luma_sum2[idx] / n - mean * mean).max(0.0);
            let std = var.sqrt();
            if std < FROZEN_STD_THRESHOLD && mean < FROZEN_MEAN_THRESHOLD {
                continue;
            }

            for (slot, &b) in valid_blocks.iter().enumerate() {
                h_vals[slot] = acc.block_mean_h[b * wh + idx];
                v_vals[slot] = acc.block_mean_v[b * wh + idx];
            }
            let med_h = median(&mut h_vals);
            let med_v = median(&mut v_vals);
            let agree_h = sign_agreement(&h_vals, med_h);
            let agree_v = sign_agreement(&v_vals, med_v);

            let (chosen_effect, chosen_rate) = if med_h.abs() >= med_v.abs() {
                (med_h, agree_h)
            } else {
                (med_v, agree_v)
            };

            effect[idx] = chosen_effect;
            sign_rate[idx] = chosen_rate;
            valid[idx] = true;
        }
    }

    EffectMaps {
        effect,
        sign_rate,
        valid,
    }
}

/// 矩形の bbox（画素座標、両端を含む）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Bbox {
    x_min: u32,
    y_min: u32,
    x_max: u32,
    y_max: u32,
}

impl Bbox {
    fn width(&self) -> u32 {
        self.x_max - self.x_min + 1
    }

    fn height(&self) -> u32 {
        self.y_max - self.y_min + 1
    }

    fn union(&self, other: &Bbox) -> Bbox {
        Bbox {
            x_min: self.x_min.min(other.x_min),
            y_min: self.y_min.min(other.y_min),
            x_max: self.x_max.max(other.x_max),
            y_max: self.y_max.max(other.y_max),
        }
    }
}

/// 連結成分（8近傍でラベリングした結果1つ分）。
struct Component {
    bbox: Bbox,
    pixels: Vec<usize>,
}

fn bbox_of(w: usize, pixels: &[usize]) -> Bbox {
    debug_assert!(!pixels.is_empty(), "空の成分から bbox は計算できない");
    let mut x_min = u32::MAX;
    let mut y_min = u32::MAX;
    let mut x_max = 0u32;
    let mut y_max = 0u32;
    for &idx in pixels {
        let x = (idx % w) as u32;
        let y = (idx / w) as u32;
        x_min = x_min.min(x);
        x_max = x_max.max(x);
        y_min = y_min.min(y);
        y_max = y_max.max(y);
    }
    Bbox {
        x_min,
        y_min,
        x_max,
        y_max,
    }
}

/// 1次元の真偽値配列に対する箱型膨張（半径 `radius`）。真の画素から `radius`
/// 以内にある画素をすべて真にする。左右に伸びる真のウィンドウの個数をカウンタで
/// 追跡し、O(n) で計算する（愚直な O(n*radius) を避ける）。
///
/// ウィンドウの下端・上端 (`lo`/`hi`) が `x` の進みに応じて別々の条件で動くため、
/// `out[x]` への書き込みをイテレータの `enumerate` に置き換えると却って読みにくく
/// なる。そのため clippy の `needless_range_loop` はここで抑制する。
#[allow(clippy::needless_range_loop)]
fn dilate_1d(input: &[bool], radius: usize) -> Vec<bool> {
    let n = input.len();
    let mut out = vec![false; n];
    if n == 0 {
        return out;
    }

    let mut hi = radius.min(n - 1);
    let mut count = input[0..=hi].iter().filter(|&&b| b).count();
    let mut lo = 0usize;

    for x in 0..n {
        out[x] = count > 0;
        if x + 1 < n {
            let new_lo = (x + 1).saturating_sub(radius);
            let new_hi = (x + 1 + radius).min(n - 1);
            while lo < new_lo {
                if input[lo] {
                    count -= 1;
                }
                lo += 1;
            }
            while hi < new_hi {
                hi += 1;
                if input[hi] {
                    count += 1;
                }
            }
        }
    }
    out
}

/// 2次元の箱型膨張（水平・垂直の分離可能な1次元膨張を順に適用、手順6-1）。
fn dilate(w: usize, h: usize, mask: &[bool], radius: usize) -> Vec<bool> {
    let mut tmp = vec![false; w * h];
    for y in 0..h {
        let row = &mask[y * w..(y + 1) * w];
        let dilated_row = dilate_1d(row, radius);
        tmp[y * w..(y + 1) * w].copy_from_slice(&dilated_row);
    }

    let mut out = vec![false; w * h];
    let mut col = vec![false; h];
    for x in 0..w {
        for y in 0..h {
            col[y] = tmp[y * w + x];
        }
        let dilated_col = dilate_1d(&col, radius);
        for y in 0..h {
            out[y * w + x] = dilated_col[y];
        }
    }
    out
}

/// 8近傍の連結成分ラベルを返す（背景は `-1`）。
fn label_components(w: usize, h: usize, mask: &[bool]) -> Vec<i32> {
    let wh = w * h;
    let mut labels = vec![-1i32; wh];
    let mut next_label = 0i32;
    let mut stack = Vec::new();

    for start in 0..wh {
        if !mask[start] || labels[start] != -1 {
            continue;
        }
        labels[start] = next_label;
        stack.push(start);
        while let Some(idx) = stack.pop() {
            let x = (idx % w) as isize;
            let y = (idx / w) as isize;
            for dy in -1..=1isize {
                for dx in -1..=1isize {
                    if dx == 0 && dy == 0 {
                        continue;
                    }
                    let (nx, ny) = (x + dx, y + dy);
                    if nx < 0 || ny < 0 || nx >= w as isize || ny >= h as isize {
                        continue;
                    }
                    let nidx = ny as usize * w + nx as usize;
                    if mask[nidx] && labels[nidx] == -1 {
                        labels[nidx] = next_label;
                        stack.push(nidx);
                    }
                }
            }
        }
        next_label += 1;
    }
    labels
}

/// マスクを8近傍でラベリングし、成分ごとの bbox・画素一覧を返す（手順6-2）。
///
/// `groups` は `BTreeMap`（ラベル番号の昇順）を使う。`HashMap` は `RandomState`
/// でプロセスごとに種が変わり `into_values()` の列挙順が実行ごとに変わる。
/// ラベル自体は [`label_components`] のラスタ走査（決定的）で割られるため、
/// ラベル番号の昇順で列挙すれば結果は入力ごとに常に同じ順序になる。効果量が
/// 同値の成分が [`TOP_K_PER_RUNG`] より多い段では、この順序が「どの候補がプールに
/// 残るか」に直結する（「無劣化はバイト単位で再現する」という本リポジトリの流儀に
/// 反するため、非決定的な列挙は避ける）。
fn label_to_components(w: usize, h: usize, mask: &[bool]) -> Vec<Component> {
    let labels = label_components(w, h, mask);
    let mut groups: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for (idx, &lbl) in labels.iter().enumerate() {
        if lbl >= 0 {
            groups.entry(lbl).or_default().push(idx);
        }
    }
    groups
        .into_values()
        .map(|pixels| {
            let bbox = bbox_of(w, &pixels);
            Component { bbox, pixels }
        })
        .collect()
}

fn passes_upper_bound(bbox: Bbox, video_w: u32, video_h: u32) -> bool {
    let max_w = video_w as f64 * MAX_WIDTH_RATIO;
    let max_h = video_h as f64 * MAX_HEIGHT_RATIO;
    bbox.width() as f64 <= max_w && bbox.height() as f64 <= max_h
}

/// `significant_pixels` は bbox 内の画素数ではなく、成分に属する**有意画素の実数**
/// （[`MIN_AREA`] のdoc comment参照）。
fn passes_lower_bound(bbox: Bbox, significant_pixels: usize) -> bool {
    bbox.width() >= MIN_SIDE && bbox.height() >= MIN_SIDE && significant_pixels >= MIN_AREA
}

fn passes_size_bounds(bbox: Bbox, significant_pixels: usize, video_w: u32, video_h: u32) -> bool {
    passes_upper_bound(bbox, video_w, video_h) && passes_lower_bound(bbox, significant_pixels)
}

/// 2つの bbox の1次元方向のギャップ（重なっていれば 0）。
fn gap_1d(a_min: u32, a_max: u32, b_min: u32, b_max: u32) -> u32 {
    if b_min > a_max {
        b_min - a_max - 1
    } else if a_min > b_max {
        a_min - b_max - 1
    } else {
        0
    }
}

fn close_enough(a: Bbox, b: Bbox, max_gap: u32) -> bool {
    gap_1d(a.x_min, a.x_max, b.x_min, b.x_max) <= max_gap
        && gap_1d(a.y_min, a.y_max, b.y_min, b.y_max) <= max_gap
}

/// bbox が縦横とも [`MERGE_GAP`] 画素以内に接近した成分どうしを、併合後の bbox が
/// 上限を超えない場合にだけ貪欲に併合する（不動点まで繰り返す、手順6-3）。
/// 成分数が小さいことを前提に O(n^2) の総当たりを不動点まで繰り返す実装にしている
/// （大構造の除去を経た後は、実測で成分数が小さいことを確認済み）。
fn greedy_merge(mut components: Vec<Component>, video_w: u32, video_h: u32) -> Vec<Component> {
    loop {
        let mut merged_any = false;
        'outer: for i in 0..components.len() {
            for j in (i + 1)..components.len() {
                if close_enough(components[i].bbox, components[j].bbox, MERGE_GAP) {
                    let merged_bbox = components[i].bbox.union(&components[j].bbox);
                    // 注: `close_enough`（併合判定）は2成分の**bboxのギャップ**を見るが、
                    // 手順6-1の大構造除去は**実画素のギャップ**（膨張半径8を両側から、
                    // つまり実画素の隙間<=16で連結）で塊を決める。凸でない成分（例:
                    // 階段状に伸びた形）ではこの2つのギャップが乖離するため、bbox同士は
                    // `MERGE_GAP`(16)以内に近接していても、実画素は膨張しても繋がらず
                    // 別々の膨張塊のまま両方生き残ることがある（実例: 300x200・閾値1.5で、
                    // 階段状成分A(bbox 56x30, (40,40)-(95,69))とB(10x10, (112,40))は
                    // 膨張塊としては別のまま残るが、bboxのxギャップは16、union幅82は
                    // 上限60を超える）。そのような場合はここで初めて2成分のunion bboxが
                    // 判定され、上限を超えていれば以下の `passes_upper_bound` が偽になって
                    // 併合を止める。つまりこのガードは「bboxギャップと実画素ギャップが
                    // 乖離する非凸な成分」で実際に発火し得る防御的なコードである。
                    if passes_upper_bound(merged_bbox, video_w, video_h) {
                        let comp_j = components.remove(j);
                        components[i].bbox = merged_bbox;
                        components[i].pixels.extend(comp_j.pixels);
                        merged_any = true;
                        break 'outer;
                    }
                }
            }
        }
        if !merged_any {
            break;
        }
    }
    components
}

/// 手順6の後処理を適用してから上限・下限を満たす成分だけを残す。
fn process_rung(
    w: usize,
    h: usize,
    effect: &[f32],
    sign_rate: &[f32],
    valid: &[bool],
    threshold: f32,
    video_size: VideoSize,
) -> Vec<Component> {
    let wh = w * h;
    let mut mask = vec![false; wh];
    for idx in 0..wh {
        if valid[idx] && effect[idx].abs() >= threshold && sign_rate[idx] >= SIGN_AGREEMENT_MIN {
            mask[idx] = true;
        }
    }

    // 6-1: 大構造の除去。膨張してから塊を取り、塊の bbox(膨張前の有意画素から
    // 計算)が上限を超えたら、その塊に属する有意画素をまとめて捨てる。
    let dilated = dilate(w, h, &mask, DILATE_RADIUS);
    let dilated_labels = label_components(w, h, &dilated);
    // `BTreeMap` を使う理由は `label_to_components` の doc comment参照
    // （この groups は削除判定のみに使い、結果はどの順で処理しても変わらないが、
    // 一貫性のため同じ流儀に揃える）。
    let mut groups: BTreeMap<i32, Vec<usize>> = BTreeMap::new();
    for idx in 0..wh {
        if mask[idx] {
            groups.entry(dilated_labels[idx]).or_default().push(idx);
        }
    }
    for pixels in groups.values() {
        let bbox = bbox_of(w, pixels);
        if !passes_upper_bound(bbox, video_size.width, video_size.height) {
            for &idx in pixels {
                mask[idx] = false;
            }
        }
    }

    // 6-2: 8近傍でラベリング(膨張していない元の有意画素で)。
    let components = label_to_components(w, h, &mask);

    // 6-3: 併合。
    let components = greedy_merge(components, video_size.width, video_size.height);

    // 6-4: 上限・下限を満たす成分だけを残す（下限の有意画素数は `c.pixels.len()`。
    // `MIN_AREA` のdoc comment参照）。
    components
        .into_iter()
        .filter(|c| passes_size_bounds(c.bbox, c.pixels.len(), video_size.width, video_size.height))
        .collect()
}

/// [`process_rung`] が返した成分1つを、段の情報と一緒にプールへ入れるための
/// 中間表現。
struct PoolEntry {
    bbox: Bbox,
    max_effect: f32,
    significant_pixels: usize,
    threshold: f32,
}

fn round_to(v: u32, unit: u32) -> u32 {
    ((v as f64 / unit as f64).round() as u32) * unit
}

fn build_candidates(
    w: usize,
    h: usize,
    video_size: VideoSize,
    acc: &BlockAccumulator,
) -> Vec<Candidate> {
    let valid_blocks: Vec<usize> = (0..NUM_BLOCKS)
        .filter(|&b| acc.block_sample_count[b] >= MIN_SAMPLES_PER_BLOCK)
        .collect();
    if valid_blocks.len() < MIN_VALID_BLOCKS {
        eprintln!(
            "[logo-estimate] 有効ブロックが{}個(下限{MIN_VALID_BLOCKS})しかないため、\
             候補列を生成しません（入力が短すぎます）。",
            valid_blocks.len()
        );
        return Vec::new();
    }

    let maps = build_effect_maps(w, h, &valid_blocks, acc);

    let mut pool: Vec<PoolEntry> = Vec::new();
    for &threshold in THRESHOLD_LADDER.iter() {
        let components = process_rung(
            w,
            h,
            &maps.effect,
            &maps.sign_rate,
            &maps.valid,
            threshold,
            video_size,
        );
        eprintln!(
            "[logo-estimate] 閾値{threshold}階調: 成分{}個",
            components.len()
        );

        let mut ranked: Vec<PoolEntry> = components
            .into_iter()
            .map(|c| {
                let max_effect = c
                    .pixels
                    .iter()
                    .map(|&idx| maps.effect[idx].abs())
                    .fold(0.0f32, f32::max);
                PoolEntry {
                    bbox: c.bbox,
                    max_effect,
                    significant_pixels: c.pixels.len(),
                    threshold,
                }
            })
            .collect();
        // 最大効果量の降順。同値のタイは bbox の座標で決定的に破る（`label_to_components`
        // の列挙順は決定的だが、タイの並びが結果に影響しないことを明示するため
        // 追加の全順序を敷いておく）。
        ranked.sort_by(|a, b| {
            b.max_effect
                .partial_cmp(&a.max_effect)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| a.bbox.x_min.cmp(&b.bbox.x_min))
                .then_with(|| a.bbox.y_min.cmp(&b.bbox.y_min))
                .then_with(|| a.bbox.x_max.cmp(&b.bbox.x_max))
                .then_with(|| a.bbox.y_max.cmp(&b.bbox.y_max))
        });

        for entry in ranked.into_iter().take(TOP_K_PER_RUNG) {
            eprintln!(
                "[logo-estimate]   候補: 矩形=(x={}, y={}, w={}, h={}) 最大効果量={:.1} \
                 有意画素数={} 段={}",
                entry.bbox.x_min,
                entry.bbox.y_min,
                entry.bbox.width(),
                entry.bbox.height(),
                entry.max_effect,
                entry.significant_pixels,
                entry.threshold,
            );
            pool.push(entry);
        }
    }

    // 手順7: 段をまたいで bbox がほぼ同じ候補(座標・寸法を8画素単位に丸めて一致)を
    // 1つにまとめる(低い段のものを残す。bboxが広く取れるため。poolは閾値の昇順に
    // 積んでいるので、先に見つかったものを残せば自動的に低い段が優先される)。
    let mut seen: HashSet<(u32, u32, u32, u32)> = HashSet::new();
    let mut deduped: Vec<PoolEntry> = Vec::new();
    for entry in pool {
        let key = (
            round_to(entry.bbox.x_min, DEDUP_ROUND),
            round_to(entry.bbox.y_min, DEDUP_ROUND),
            round_to(entry.bbox.width(), DEDUP_ROUND),
            round_to(entry.bbox.height(), DEDUP_ROUND),
        );
        if seen.insert(key) {
            deduped.push(entry);
        }
    }

    let mut candidates: Vec<Candidate> = deduped
        .into_iter()
        .map(|entry| {
            let raw_bbox = LogoRect {
                x: entry.bbox.x_min,
                y: entry.bbox.y_min,
                w: entry.bbox.width(),
                h: entry.bbox.height(),
            };
            let expanded_x = entry.bbox.x_min.saturating_sub(RECT_MARGIN);
            let expanded_y = entry.bbox.y_min.saturating_sub(RECT_MARGIN);
            let expanded_right = (entry.bbox.x_max + 1 + RECT_MARGIN).min(video_size.width);
            let expanded_bottom = (entry.bbox.y_max + 1 + RECT_MARGIN).min(video_size.height);
            let expanded = LogoRect {
                x: expanded_x,
                y: expanded_y,
                w: expanded_right.saturating_sub(expanded_x),
                h: expanded_bottom.saturating_sub(expanded_y),
            };
            let (estimated_rect, _) = round_rect_to_even(expanded);
            Candidate {
                estimated_rect,
                raw_bbox,
                max_effect: entry.max_effect,
                significant_pixels: entry.significant_pixels,
                rung_threshold: entry.threshold,
            }
        })
        .collect();

    // 同値タイの決定的なタイブレークは上の `ranked.sort_by` と同じ理由。
    candidates.sort_by(|a, b| {
        b.max_effect
            .partial_cmp(&a.max_effect)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.raw_bbox.x.cmp(&b.raw_bbox.x))
            .then_with(|| a.raw_bbox.y.cmp(&b.raw_bbox.y))
            .then_with(|| a.raw_bbox.w.cmp(&b.raw_bbox.w))
            .then_with(|| a.raw_bbox.h.cmp(&b.raw_bbox.h))
    });

    // 手順8: 最終的な候補列を stderr に出す。段ごとのログ(上)は丸め・余白前の生bbox
    // しか分からないため、ここでは調査時にそのまま `--rect` へ渡せる形の
    // `estimated_rect` も併せて出す。
    for (rank, c) in candidates.iter().enumerate() {
        eprintln!(
            "[logo-estimate] 候補{}: estimated_rect=(x={}, y={}, w={}, h={}) \
             raw_bbox=(x={}, y={}, w={}, h={}) 最大効果量={:.1} 有意画素数={} 段={}",
            rank + 1,
            c.estimated_rect.x,
            c.estimated_rect.y,
            c.estimated_rect.w,
            c.estimated_rect.h,
            c.raw_bbox.x,
            c.raw_bbox.y,
            c.raw_bbox.w,
            c.raw_bbox.h,
            c.max_effect,
            c.significant_pixels,
            c.rung_threshold,
        );
    }

    candidates
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [`estimate_raw`] の結果から候補列だけを取り出す、テスト専用の薄いラッパー。
    /// AUC採点（E18-3、[`select_by_auc`]）を経由せず、#132 の構造的な候補生成
    /// （手順1〜7）だけを検証するために使う（構造的な候補生成とAUC採点は独立に
    /// テストする。モジュール doc comment「候補の採点」参照）。
    fn estimate_raw_candidates(
        video_size: VideoSize,
        mut stream_frames: impl FnMut(&mut FrameCallback<'_>) -> anyhow::Result<u64>,
    ) -> anyhow::Result<Vec<Candidate>> {
        Ok(estimate_raw(video_size, &mut stream_frames)?
            .map(|raw| raw.candidates)
            .unwrap_or_default())
    }

    /// 常に [`SampleLabel::Program`] を返す分類器。CM標本0枚になるため、
    /// [`select_by_auc`] の「CM標本が[`MIN_CM_SAMPLES_FOR_AUC`]枚未満」の分岐
    /// （候補列の先頭のみ採用）を確認するテストで使う。
    fn all_program(_serial: u64) -> SampleLabel {
        SampleLabel::Program
    }

    /// 標本の通し番号が5の倍数+4（0始まりで5,10,15...番目）ならCM、それ以外は本編と
    /// する分類器。AUCの完了条件テストで使う（`TOTAL`=160なら本編128枚・CM32枚に
    /// 分かれ、[`MIN_CM_SAMPLES_FOR_AUC`]=20を安全に上回る）。
    fn every_fifth_is_cm(i: u64) -> SampleLabel {
        if i % 5 == 4 {
            SampleLabel::Cm
        } else {
            SampleLabel::Program
        }
    }

    /// `x`・`y`・フレーム番号 `i` から決定的な擬似乱数を作る（-20.0〜20.0）。
    /// テレビ朝日の時計のような「常時表示だがCM側で検出が乱れる」を模すのに使う
    /// （テストのみ。乱数生成crateへの依存を避けるためビット混合で実装する）。
    fn pixel_noise(x: usize, y: usize, i: usize) -> f64 {
        let mut h = (x as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        h ^= (y as u64).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        h ^= (i as u64).wrapping_mul(0x94D0_49BB_1331_11EB);
        h ^= h >> 33;
        h = h.wrapping_mul(0xFF51_AFD7_ED55_8CCD);
        h ^= h >> 33;
        ((h % 41) as i64 - 20) as f64
    }

    /// 8bit にクランプして丸める。
    fn clamp_u8(v: f64) -> u8 {
        v.round().clamp(0.0, 255.0) as u8
    }

    /// [`make_frames`] の `regions` 引数1要素: 矩形(x,y,w,h) とその内側に乗せる
    /// 背景からの上乗せ量 `offset_at(フレーム番号)`。
    type Region<'a> = (usize, usize, usize, usize, &'a dyn Fn(usize) -> f64);

    /// 合成フレーム列を作る。`bg_at(i)` が i枚目の背景輝度(画面全体で一様)。
    /// 矩形が重なっていないことは呼び出し側の責任。
    fn make_frames(
        video_w: usize,
        video_h: usize,
        total: usize,
        bg_at: impl Fn(usize) -> f64,
        regions: &[Region<'_>],
    ) -> Vec<Vec<u8>> {
        (0..total)
            .map(|i| {
                let bg = bg_at(i);
                let mut frame = vec![clamp_u8(bg); video_w * video_h];
                for &(rx, ry, rw, rh, offset_at) in regions {
                    let off = offset_at(i);
                    for y in ry..ry + rh {
                        for x in rx..rx + rw {
                            frame[y * video_w + x] = clamp_u8(bg + off);
                        }
                    }
                }
                frame
            })
            .collect()
    }

    /// 時間変化する背景(4値を周期的に繰り返す)。凍結領域除外(輝度の時間標準偏差が
    /// 1.0未満)に引っかからないよう、十分な分散を持たせる。
    fn varying_bg(i: usize) -> f64 {
        [50.0, 100.0, 150.0, 200.0][i % 4]
    }

    /// `frames` を `estimate_candidates` の「供給関数」として何度でも再生できる
    /// アダプタ。
    fn frames_source(
        frames: &[Vec<u8>],
    ) -> impl FnMut(&mut FrameCallback<'_>) -> anyhow::Result<u64> + '_ {
        move |on_frame| {
            for f in frames {
                on_frame(f)?;
            }
            Ok(frames.len() as u64)
        }
    }

    /// 隣接差分から復元される bbox（手順の説明どおり、内側矩形 (x,y,w,h) に対して
    /// 左1画素・上1画素だけ外側にずれる。モジュール内のコメント「原理」参照）。
    fn expected_raw_bbox_for_inside_rect(x: u32, y: u32, w: u32, h: u32) -> LogoRect {
        LogoRect {
            x: x - 1,
            y: y - 1,
            w: w + 1,
            h: h + 1,
        }
    }

    fn expand_and_round(bbox: LogoRect, video_size: VideoSize) -> LogoRect {
        let expanded_x = bbox.x.saturating_sub(RECT_MARGIN);
        let expanded_y = bbox.y.saturating_sub(RECT_MARGIN);
        let expanded_right = (bbox.x + bbox.w + RECT_MARGIN).min(video_size.width);
        let expanded_bottom = (bbox.y + bbox.h + RECT_MARGIN).min(video_size.height);
        let expanded = LogoRect {
            x: expanded_x,
            y: expanded_y,
            w: expanded_right.saturating_sub(expanded_x),
            h: expanded_bottom.saturating_sub(expanded_y),
        };
        round_rect_to_even(expanded).0
    }

    const VIDEO_W: usize = 300;
    const VIDEO_H: usize = 200;
    const TOTAL: usize = 160; // 16ブロック x 10標本/ブロック。

    fn video_size() -> VideoSize {
        VideoSize {
            width: VIDEO_W as u32,
            height: VIDEO_H as u32,
        }
    }

    // ---------------------------------------------------------------
    // 完了条件1: 既知の矩形が候補列の先頭に復元される。
    // ---------------------------------------------------------------

    #[test]
    fn recovers_known_rect_as_top_candidate() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x, y, w, h, &offset)],
        );

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("supply関数はエラーを返さない");

        assert!(!candidates.is_empty(), "矩形が復元されるはず");
        let top = &candidates[0];

        let expected_raw =
            expected_raw_bbox_for_inside_rect(x as u32, y as u32, w as u32, h as u32);
        assert_eq!(top.raw_bbox, expected_raw, "raw_bbox={:?}", top.raw_bbox);

        let expected_rect = expand_and_round(expected_raw, video_size());
        assert_eq!(top.estimated_rect, expected_rect);

        // 先頭(最大効果量)であることの確認。
        for c in &candidates {
            assert!(top.max_effect >= c.max_effect);
        }
    }

    // ---------------------------------------------------------------
    // 完了条件2: 段差を一切乗せない場合は空。
    // ---------------------------------------------------------------

    /// 画面全体が動く斜め縞模様のフレームを作る。フレームごとに縞がずれるため、
    /// 「一様な背景(隣接差分が厳密に0)」よりずっと厳しい busy な入力になる
    /// （レビュー指摘: 一様背景だと閾値やラダー・符号一致率・大構造除去・上下限の
    /// どれを壊しても偶然通ってしまい、「busyな背景が偽陽性を生まない」という
    /// 見たい性質を検証できない。画素ごと独立な乱数雑音も使わない。標本数が
    /// 少ないと中央値の頑健性が足りず、有意画素2〜6個程度の偽候補が出ることを
    /// レビュアーが実測で確認済みのため）。
    fn moving_stripe_frame(video_w: usize, video_h: usize, frame_index: usize) -> Vec<u8> {
        (0..video_h)
            .flat_map(|y| {
                (0..video_w).map(move |x| {
                    let phase = (x + 3 * y + 7 * frame_index) % 64;
                    if phase < 32 {
                        40u8
                    } else {
                        200u8
                    }
                })
            })
            .collect()
    }

    #[test]
    fn no_step_yields_empty_candidates() {
        let frames: Vec<Vec<u8>> = (0..TOTAL)
            .map(|i| moving_stripe_frame(VIDEO_W, VIDEO_H, i))
            .collect();
        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");
        assert!(candidates.is_empty(), "candidates={candidates:?}");
    }

    // ---------------------------------------------------------------
    // 完了条件3: 上限を超える大きさの定常段差だけなら空。
    // ---------------------------------------------------------------

    #[test]
    fn oversized_step_yields_empty_candidates() {
        // 幅0.20*300=60が上限。100幅の帯にして確実に超える。
        let (x, y, w, h) = (20usize, 20usize, 100usize, 15usize);
        let offset = |_i: usize| 50.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x, y, w, h, &offset)],
        );

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");
        assert!(candidates.is_empty(), "candidates={candidates:?}");
    }

    // ---------------------------------------------------------------
    // MIN_AREA回帰: 下限判定は成分の有意画素数(bbox面積ではない)。
    // ---------------------------------------------------------------

    #[test]
    fn sparse_dotted_component_below_significant_pixel_minimum_is_rejected() {
        // 10x10角の4隅に1画素だけの段差(「点」)を置く。各点は隣接差分の観点で
        // 有意画素をちょうど3個生む(自分自身と、左・上の隣接画素。原理は
        // モジュール doc comment 「原理」節と同じ隣接差分)。4点で有意画素は
        // 合計12個だが、4点をまとめた生bboxは(39,39)-(49,49)付近のおよそ11x11
        // (面積121、各辺11>=8)になる。
        //
        // これは MIN_AREA を bbox面積で判定する旧実装なら合格してしまっていた
        // 形（各辺>=8を満たせば面積>=64>=20は自動的に満たされ恒真になっていた、
        // レビューで指摘された回帰）。新実装(成分の有意画素数=12<20で判定)では
        // 候補に出ないことをこのテストで固定する。
        //
        // (手動確認: `passes_lower_bound` を `bbox.width() as u64 *
        // bbox.height() as u64 >= 20` へ戻すと、このテストは失敗して
        // candidatesにこの点状成分が出るようになる。)
        let dots: [(usize, usize); 4] = [(40, 40), (49, 40), (40, 49), (49, 49)];
        let offset = |_i: usize| 30.0;
        let regions: Vec<Region<'_>> = dots
            .iter()
            .map(|&(x, y)| (x, y, 1usize, 1usize, &offset as &dyn Fn(usize) -> f64))
            .collect();
        let frames = make_frames(VIDEO_W, VIDEO_H, TOTAL, varying_bg, &regions);

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");
        assert!(
            candidates.is_empty(),
            "有意画素数が下限未満の点状成分が候補に出ている: {candidates:?}"
        );
    }

    // ---------------------------------------------------------------
    // 完了条件4: 16画素以内の2ブロックは併合される。
    // ---------------------------------------------------------------

    #[test]
    fn nearby_blocks_within_merge_gap_are_merged() {
        let (x1, y1, w1, h1) = (40usize, 40usize, 10usize, 10usize);
        // 1つ目の右端(生bboxはx=x1-1..x1+w1-1)から10画素の隙間(<16)を空けて2つ目。
        let (x2, y2, w2, h2) = (x1 + w1 + 10, 40usize, 10usize, 10usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x1, y1, w1, h1, &offset), (x2, y2, w2, h2, &offset)],
        );

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");

        assert_eq!(
            candidates.len(),
            1,
            "1つの矩形に併合されるはず: {candidates:?}"
        );
        let merged = &candidates[0];
        let bbox1 = expected_raw_bbox_for_inside_rect(x1 as u32, y1 as u32, w1 as u32, h1 as u32);
        let bbox2 = expected_raw_bbox_for_inside_rect(x2 as u32, y2 as u32, w2 as u32, h2 as u32);
        let b1 = Bbox {
            x_min: bbox1.x,
            y_min: bbox1.y,
            x_max: bbox1.x + bbox1.w - 1,
            y_max: bbox1.y + bbox1.h - 1,
        };
        let b2 = Bbox {
            x_min: bbox2.x,
            y_min: bbox2.y,
            x_max: bbox2.x + bbox2.w - 1,
            y_max: bbox2.y + bbox2.h - 1,
        };
        let union = b1.union(&b2);
        assert_eq!(merged.raw_bbox.x, union.x_min);
        assert_eq!(merged.raw_bbox.y, union.y_min);
        assert_eq!(merged.raw_bbox.w, union.width());
        assert_eq!(merged.raw_bbox.h, union.height());
    }

    // ---------------------------------------------------------------
    // 完了条件5: 上限を超えて離れた2ブロックは併合されず両方候補に入る。
    // ---------------------------------------------------------------

    #[test]
    fn far_apart_blocks_are_not_merged() {
        // gapを40にして、2成分の隙間(>MERGE_GAP=16)だけでなく、union(併合したと
        // 仮定した場合の bbox)自体も上限(幅0.20*300=60)を超える(実測: union幅61)
        // ようにする。issue の完了条件「上限を超えるほど離れた」を実際に検証する
        // ため（gapがMERGE_GAPを超えるだけでunionが上限内に収まる値だと、
        // 「離れているから併合されない」ことしか確認できず、「上限を超えるほど」の
        // 部分を検証できない）。
        let (x1, y1, w1, h1) = (40usize, 40usize, 10usize, 10usize);
        let gap = 40usize;
        let (x2, y2, w2, h2) = (x1 + w1 + gap, 40usize, 10usize, 10usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x1, y1, w1, h1, &offset), (x2, y2, w2, h2, &offset)],
        );

        let expected1 =
            expected_raw_bbox_for_inside_rect(x1 as u32, y1 as u32, w1 as u32, h1 as u32);
        let expected2 =
            expected_raw_bbox_for_inside_rect(x2 as u32, y2 as u32, w2 as u32, h2 as u32);
        let union_width = (expected2.x + expected2.w).max(expected1.x + expected1.w)
            - expected1.x.min(expected2.x);
        let upper_bound_width = (VIDEO_W as f64 * MAX_WIDTH_RATIO) as u32;
        assert!(
            union_width > upper_bound_width,
            "この試験の前提(unionが上限超え)が崩れている: union_width={union_width}, \
             upper_bound_width={upper_bound_width}"
        );

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");

        assert_eq!(candidates.len(), 2, "併合されず2つ残るはず: {candidates:?}");
        let bboxes: Vec<LogoRect> = candidates.iter().map(|c| c.raw_bbox).collect();
        assert!(bboxes.contains(&expected1), "bboxes={bboxes:?}");
        assert!(bboxes.contains(&expected2), "bboxes={bboxes:?}");
    }

    // ---------------------------------------------------------------
    // 完了条件6: 弱い段差+近接する上限超えの強い大構造は、低い段では全部落ち、
    // 高い段では強い成分だけが分離して候補に入る（テレビ朝日の時計を模した状況）。
    // ---------------------------------------------------------------

    #[test]
    fn weak_step_entangled_with_oversized_strong_step_separates_at_higher_rung() {
        // 弱い(4階調)大きめの帯(幅100、上限60を超える)。
        let (bx, by, bw, bh) = (20usize, 20usize, 100usize, 15usize);
        // 強い(60階調)小さな矩形。帯にちょうど隣接させる(生bboxが接するように)。
        let (sx, sy, sw, sh) = (bx + bw, 20usize, 10usize, 10usize);

        let weak_offset = |_i: usize| 4.0;
        let strong_offset = |_i: usize| 60.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[
                (bx, by, bw, bh, &weak_offset),
                (sx, sy, sw, sh, &strong_offset),
            ],
        );

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");

        // 強い小矩形だけが候補として残るはず(弱い大帯は単独でもどの段でも
        // 上限超え、かつ低い段では強い矩形と繋がって一緒に落ちる)。
        assert!(!candidates.is_empty(), "強い成分は候補に残るはず");
        let expected_strong =
            expected_raw_bbox_for_inside_rect(sx as u32, sy as u32, sw as u32, sh as u32);
        assert!(
            candidates.iter().any(|c| c.raw_bbox == expected_strong),
            "candidates={candidates:?}, expected_strong={expected_strong:?}"
        );

        // 弱い大帯単独に相当する(上限超えの)候補は残っていないはず。
        for c in &candidates {
            assert!(
                c.raw_bbox.w < 60 || c.raw_bbox.h < 30,
                "上限超えの成分が残っている: {:?}",
                c.raw_bbox
            );
        }
    }

    // ---------------------------------------------------------------
    // 完了条件7: 一部のブロックにだけ強い段差(場面の偏り)は符号一致率で落ちる。
    // ---------------------------------------------------------------

    #[test]
    fn sign_disagreement_across_blocks_is_rejected() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        // 16ブロックのうち9ブロックは+20、7ブロックは-20。
        // 符号一致率は多数派側でも9/16=0.5625 < 0.8 になり、閾値ラダーのどの段でも
        // 候補から落ちる。
        let positive_blocks: std::collections::HashSet<usize> = (0..9).collect();
        let offset = move |i: usize| {
            let b = block_of(i as u64, TOTAL as u64);
            if positive_blocks.contains(&b) {
                20.0
            } else {
                -20.0
            }
        };
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x, y, w, h, &offset)],
        );

        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");
        assert!(
            candidates.is_empty(),
            "符号一致率不足で落ちるはず: {candidates:?}"
        );
    }

    // ---------------------------------------------------------------
    // 完了条件8: 標本数を変えても(200枚と2000枚)同じ候補が出る(標本数不変性)。
    // ---------------------------------------------------------------

    #[test]
    fn threshold_is_invariant_to_sample_count() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let offset = |_i: usize| 20.0;

        let frames_200 = make_frames(VIDEO_W, VIDEO_H, 200, varying_bg, &[(x, y, w, h, &offset)]);
        let frames_2000 = make_frames(VIDEO_W, VIDEO_H, 2000, varying_bg, &[(x, y, w, h, &offset)]);

        let candidates_200 = estimate_raw_candidates(video_size(), frames_source(&frames_200))
            .expect("エラーにならない");
        let candidates_2000 = estimate_raw_candidates(video_size(), frames_source(&frames_2000))
            .expect("エラーにならない");

        assert!(!candidates_200.is_empty());
        assert_eq!(candidates_200.len(), candidates_2000.len());
        assert_eq!(candidates_200[0].raw_bbox, candidates_2000[0].raw_bbox);
        assert_eq!(
            candidates_200[0].estimated_rect,
            candidates_2000[0].estimated_rect
        );
        assert!((candidates_200[0].max_effect - candidates_2000[0].max_effect).abs() < 1e-4);
    }

    // ---------------------------------------------------------------
    // 補助関数の単体テスト。
    // ---------------------------------------------------------------

    #[test]
    fn dilate_1d_grows_true_runs_by_radius() {
        let input = vec![false, false, true, false, false, false, false];
        let out = dilate_1d(&input, 1);
        assert_eq!(out, vec![false, true, true, true, false, false, false]);
    }

    #[test]
    fn dilate_1d_with_radius_zero_is_identity() {
        let input = vec![false, true, false, true, false];
        let out = dilate_1d(&input, 0);
        assert_eq!(out, input);
    }

    #[test]
    fn dilate_1d_with_radius_covering_whole_input_is_all_true_if_any_true() {
        let input = vec![false, false, false, true, false, false, false];
        let out = dilate_1d(&input, input.len());
        assert_eq!(out, vec![true; input.len()]);
    }

    #[test]
    fn dilate_1d_all_false_stays_all_false_regardless_of_radius() {
        let input = vec![false; 10];
        assert_eq!(dilate_1d(&input, 0), input);
        assert_eq!(dilate_1d(&input, 3), input);
        assert_eq!(dilate_1d(&input, 100), input);
    }

    #[test]
    fn median_of_even_count_averages_middle_two() {
        let mut v = vec![1.0, 3.0, 2.0, 4.0];
        assert_eq!(median(&mut v), 2.5);
    }

    #[test]
    fn median_of_odd_count_returns_middle() {
        let mut v = vec![5.0, 1.0, 3.0];
        assert_eq!(median(&mut v), 3.0);
    }

    // ---------------------------------------------------------------
    // 早期return: 映像サイズ0・標本0枚・有効ブロック不足。
    // ---------------------------------------------------------------

    #[test]
    fn zero_width_returns_empty_without_calling_stream_frames() {
        let mut called = false;
        let candidates = estimate_raw_candidates(
            VideoSize {
                width: 0,
                height: 200,
            },
            |_on_frame| {
                called = true;
                Ok(0)
            },
        )
        .expect("エラーにならない");
        assert!(candidates.is_empty());
        assert!(!called, "映像サイズが0ならフレーム供給関数を呼ばないはず");
    }

    #[test]
    fn zero_height_returns_empty_without_calling_stream_frames() {
        let mut called = false;
        let candidates = estimate_raw_candidates(
            VideoSize {
                width: 300,
                height: 0,
            },
            |_on_frame| {
                called = true;
                Ok(0)
            },
        )
        .expect("エラーにならない");
        assert!(candidates.is_empty());
        assert!(!called, "映像サイズが0ならフレーム供給関数を呼ばないはず");
    }

    #[test]
    fn zero_total_frames_returns_empty() {
        let frames: Vec<Vec<u8>> = Vec::new();
        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");
        assert!(candidates.is_empty(), "candidates={candidates:?}");
    }

    #[test]
    fn too_few_valid_blocks_returns_empty() {
        // MIN_VALID_BLOCKS(8)未満しか標本が無い短い入力(1ブロック分の10枚だけ)。
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(VIDEO_W, VIDEO_H, 10, varying_bg, &[(x, y, w, h, &offset)]);
        let candidates = estimate_raw_candidates(video_size(), frames_source(&frames))
            .expect("エラーにならない");
        assert!(
            candidates.is_empty(),
            "有効ブロックが不足するので空のはず: {candidates:?}"
        );
    }

    #[test]
    fn inconsistent_frame_count_between_two_calls_is_an_error() {
        // 1回目と2回目で異なる枚数を返す供給関数(実装ミスや実行環境の変化を想定)。
        let mut call_count = 0;
        let err = estimate_raw_candidates(video_size(), |on_frame| {
            call_count += 1;
            let n = if call_count == 1 { 160 } else { 100 };
            for i in 0..n {
                on_frame(&vec![128u8; VIDEO_W * VIDEO_H])?;
                let _ = i;
            }
            Ok(n as u64)
        })
        .expect_err("1回目と2回目の枚数が食い違うのでエラーになるはず");
        assert!(err.to_string().contains("160"), "err={err}");
        assert!(err.to_string().contains("100"), "err={err}");
    }

    // ---------------------------------------------------------------
    // AUC完了条件1: 本編群にだけ段差がある候補は、AUCが高く採用列に入る。
    // ---------------------------------------------------------------

    #[test]
    fn step_present_only_in_program_group_has_high_auc_and_is_adopted() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let frames: Vec<Vec<u8>> = (0..TOTAL)
            .map(|i| {
                let bg = varying_bg(i);
                let mut frame = vec![clamp_u8(bg); VIDEO_W * VIDEO_H];
                if every_fifth_is_cm(i as u64) != SampleLabel::Cm {
                    for yy in y..y + h {
                        for xx in x..x + w {
                            frame[yy * VIDEO_W + xx] = clamp_u8(bg + 20.0);
                        }
                    }
                }
                frame
            })
            .collect();

        let candidates =
            estimate_candidates(video_size(), frames_source(&frames), every_fifth_is_cm)
                .expect("エラーにならない");

        let expected = expected_raw_bbox_for_inside_rect(x as u32, y as u32, w as u32, h as u32);
        assert_eq!(candidates.len(), 1, "candidates={candidates:?}");
        assert_eq!(candidates[0].raw_bbox, expected);
    }

    // ---------------------------------------------------------------
    // AUC完了条件2: 本編/CMと無相関な常時オーバーレイは、AUCが0.5付近になり
    // 採用列に入らない。
    // ---------------------------------------------------------------

    #[test]
    fn step_present_in_both_groups_has_auc_near_half_and_is_not_adopted() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x, y, w, h, &offset)],
        );

        let candidates =
            estimate_candidates(video_size(), frames_source(&frames), every_fifth_is_cm)
                .expect("エラーにならない");

        assert!(
            candidates.is_empty(),
            "本編/CMのどちらでも同じ強さで出るオーバーレイはAUC~0.5になり\
             採用されないはず: {candidates:?}"
        );
    }

    // ---------------------------------------------------------------
    // AUC完了条件3: 常時表示だがCM側でだけスコアが下がる候補
    // （テレビ朝日の時計を模したもの）は採用列に入る。
    // ---------------------------------------------------------------

    #[test]
    fn always_present_overlay_weaker_in_cm_is_adopted() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let frames: Vec<Vec<u8>> = (0..TOTAL)
            .map(|i| {
                let bg = varying_bg(i);
                let mut frame = vec![clamp_u8(bg); VIDEO_W * VIDEO_H];
                let is_cm = every_fifth_is_cm(i as u64) == SampleLabel::Cm;
                for yy in y..y + h {
                    for xx in x..x + w {
                        let off = if is_cm { pixel_noise(xx, yy, i) } else { 20.0 };
                        frame[yy * VIDEO_W + xx] = clamp_u8(bg + off);
                    }
                }
                frame
            })
            .collect();

        let candidates =
            estimate_candidates(video_size(), frames_source(&frames), every_fifth_is_cm)
                .expect("エラーにならない");

        let expected = expected_raw_bbox_for_inside_rect(x as u32, y as u32, w as u32, h as u32);
        assert!(
            candidates.iter().any(|c| c.raw_bbox == expected),
            "candidates={candidates:?}"
        );
    }

    // ---------------------------------------------------------------
    // AUC完了条件4: CM標本が0枚・下限未満のときはAUCを計算せず、
    // 候補列の先頭1つだけを返す。
    // ---------------------------------------------------------------

    #[test]
    fn zero_cm_samples_skips_auc_and_returns_top_candidate() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x, y, w, h, &offset)],
        );

        let candidates = estimate_candidates(video_size(), frames_source(&frames), all_program)
            .expect("エラーにならない");

        let expected = expected_raw_bbox_for_inside_rect(x as u32, y as u32, w as u32, h as u32);
        assert_eq!(candidates.len(), 1, "candidates={candidates:?}");
        assert_eq!(candidates[0].raw_bbox, expected);
    }

    #[test]
    fn few_cm_samples_below_minimum_skips_auc_and_returns_top_candidate() {
        let (x, y, w, h) = (40usize, 40usize, 20usize, 16usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x, y, w, h, &offset)],
        );

        // CM標本を10枚(下限20枚未満)だけ混ぜる。AUCは計算されず先頭候補が返るはず。
        let classify = |i: u64| {
            if i < 10 {
                SampleLabel::Cm
            } else {
                SampleLabel::Program
            }
        };
        let candidates = estimate_candidates(video_size(), frames_source(&frames), classify)
            .expect("エラーにならない");

        let expected = expected_raw_bbox_for_inside_rect(x as u32, y as u32, w as u32, h as u32);
        assert_eq!(candidates.len(), 1, "candidates={candidates:?}");
        assert_eq!(candidates[0].raw_bbox, expected);
    }

    // ---------------------------------------------------------------
    // AUC完了条件5: CM標本20枚以上で全候補のAUCが閾値未満のとき、空の採用列が
    // 返る（「せっかく推定したから」と無条件採用へフォールバックしない）。
    // ---------------------------------------------------------------

    #[test]
    fn no_candidate_clears_auc_threshold_yields_empty_not_unconditional_adoption() {
        // `far_apart_blocks_are_not_merged`(#132)と同じ配置で、併合されない
        // 2つの候補を作る。両方とも本編/CMどちらでも同じ強さで出る(AUC~0.5)ため、
        // 「全候補」が閾値未満になる状況を作れる。
        let (x1, y1, w1, h1) = (40usize, 40usize, 10usize, 10usize);
        let gap = 40usize;
        let (x2, y2, w2, h2) = (x1 + w1 + gap, 40usize, 10usize, 10usize);
        let offset = |_i: usize| 20.0;
        let frames = make_frames(
            VIDEO_W,
            VIDEO_H,
            TOTAL,
            varying_bg,
            &[(x1, y1, w1, h1, &offset), (x2, y2, w2, h2, &offset)],
        );

        let candidates =
            estimate_candidates(video_size(), frames_source(&frames), every_fifth_is_cm)
                .expect("エラーにならない");

        assert!(
            candidates.is_empty(),
            "AUCで全滅した場合は無条件採用へ落ちず空になるはず: {candidates:?}"
        );
    }

    // ---------------------------------------------------------------
    // auc_from_scores の単体テスト（cos類似度の合成を経由しない、Mann-Whitney U
    // の計算そのものの検証）。
    // ---------------------------------------------------------------

    #[test]
    fn auc_from_scores_is_one_for_perfect_separation() {
        let program = vec![1.0, 0.9, 0.8];
        let cm = vec![0.1, 0.2, 0.3];
        assert_eq!(auc_from_scores(&program, &cm), 1.0);
    }

    #[test]
    fn auc_from_scores_is_zero_for_perfect_reversal() {
        let program = vec![0.1, 0.2, 0.3];
        let cm = vec![1.0, 0.9, 0.8];
        assert_eq!(auc_from_scores(&program, &cm), 0.0);
    }

    #[test]
    fn auc_from_scores_is_half_for_identical_tied_scores() {
        let program = vec![0.5; 4];
        let cm = vec![0.5; 6];
        assert_eq!(auc_from_scores(&program, &cm), 0.5);
    }

    #[test]
    fn auc_from_scores_is_nan_when_a_group_is_empty() {
        assert!(auc_from_scores(&[], &[1.0]).is_nan());
        assert!(auc_from_scores(&[1.0], &[]).is_nan());
    }
}
