//! ffmpeg を子プロセスとして起動し、ロゴ矩形の輝度平面をフレーム順に読む。
//!
//! ロゴ検出（別 issue）には mp4 の輝度プレーンをフレーム順に流す経路が要る。
//! ここでは `ffmpeg -vf crop=<w>:<h>:<x>:<y> -pix_fmt gray -f rawvideo -` を
//! 起動し、標準出力をストリームで読む（libav をリンクせず、既存の外部プロセス
//! 方式に揃える）。ロゴ矩形だけに crop するので1フレームは `w*h` バイトしかなく、
//! 全フレームをメモリに溜めない設計にする（[`stream_luma_frames`] はフレームごとに
//! コールバックを呼ぶ）。
//!
//! ## 座標系がこの issue の本質
//!
//! ロゴ区間は後段で `join_logo_scp -inlogo` に渡され、chapter_exe が出した
//! 無音シーンチェンジ（`scp.txt`）と**同じフレーム座標系**で解釈される。その
//! 座標系は `.dtvi`（`timeline_profile dtv-display-order-v1`、0始まりの表示順、
//! `crate::dtvi` の doc comment参照）。ffmpeg の rawvideo 出力は表示順だが、
//! **フレーム数が `.dtvi` の `frame_count` と一致することを検査しなければ
//! ならない**。ずれても join_logo_scp はエラーを出さず、ずれた Trim を平然と
//! 出す（CLAUDE.md 罠3: 表示順とデコード順の混同は例外を飛ばさない）。そのため
//! [`stream_luma_frames`] はこの検査を省略可能なオプションにしていない。
//!
//! ## フレームの重複・欠落を防ぐオプション（`-fps_mode passthrough`）
//!
//! ffmpeg は既定で、出力側のタイムスタンプをフレームレートに合わせて丸める際に
//! フレームを複製・欠落させることがある。**実測**（ffmpeg 8.1.2 / macOS arm64、
//! `tests/fixtures/sample.mp4`、599フレーム、64x64 crop）: `-fps_mode passthrough`
//! を付けないと出力が 2,461,696 バイト（= 601 フレーム分）になり、付けると
//! 2,453,504 バイト（= 599 フレーム分、入力と一致）になる。**付けないと実際に
//! フレームが増える。** `-fps_mode passthrough`（`-vsync 0` と同義、ffmpeg の
//! フレーム同期方式のうち唯一「入力フレームをそのまま来た順に出力し、複製も
//! 欠落もしない」もの）を指定する。`-fps_mode` は `-vsync`（非推奨）の後継で、
//! 本プロジェクトが前提とする ffmpeg（docs/toolchain-macos.md、homebrew 版）は
//! この環境で実測した ffmpeg 8.1.2 を含め対応しているため、非推奨の `-vsync 0`
//! ではなく `-fps_mode passthrough` を使う。付けなかった場合の 601 という数は
//! `.dtvi` の 599 と食い違うため、下記のフレーム数一致検査にも引っかかる
//! （オプションと検査は二重の防御になっている）。
//!
//! **`-fps_mode passthrough` を付けてもフレーム数の検査だけでは捕まらないバグが
//! ありうる**（CLAUDE.md 罠3の一般形）: 万一このオプションが効かない・外部要因で
//! 複製と欠落が同時に1回ずつ起きた場合、フレーム総数は変わらないまま中身が
//! 1フレームずれる。本 issue の検査（総数一致）はこのケースを捕まえられないと
//! 明記した上で、複製/欠落を発生源で止める（`-fps_mode passthrough`）方を対策の
//! 主とする。
//!
//! ## 矩形の範囲外検査
//!
//! `crop` フィルタの座標が映像の外に出ると ffmpeg はエラーになるが、その文字列は
//! フィルタの内部実装依存で分かりにくい。[`LogoRect::validate`] で
//! `x+w<=映像幅 && y+h<=映像高さ` を自分で検査し、矩形と映像サイズを明示した
//! メッセージを ffmpeg を起動する前に出す。
//!
//! ## デッドロック回避
//!
//! 標準出力を呼び出し側が逐次読む間、子プロセスの標準エラーを溜めたまま読まないと
//! デッドロックしうる（`crate::external` の `spawn_streaming` の doc comment
//! 参照）。この対策は `spawn_streaming` 側に実装済みで、本モジュールはそれを
//! そのまま使う。
//!
//! ## [`stream_keyframe_luma_frames`] を別関数にした理由（E18-1）
//!
//! ロゴ矩形を入力自身から推定する処理（別 issue）には、[`stream_luma_frames`] が
//! そのままでは使えない。理由は2点:
//!
//! 1. **クロップしてしまう**。矩形推定はこれから矩形を決める処理なので、クロップ前の
//!    全画面が要る
//! 2. **フレーム数の一致検査を必ず行う**（`expected_frame_count` が省略不可）。
//!    キーフレームだけ読むと GOP=120（CLAUDE.md「前提」）ごとにしか出ないため、
//!    `.dtvi` の `frame_count` とは当然一致しない
//!
//! 1点目は crop フィルタを付けないだけで済むが、2点目は「一致検査を持たない」
//! ことが要件そのものなので、既存関数から検査を外に出すことはできない
//! （`stream_luma_frames` の一致検査は CLAUDE.md 罠3への防御であり、検出経路
//! `analyze --logo` → `logo::score` が引き続き頼っている。弱めない）。そのため
//! 新しい関数 [`stream_keyframe_luma_frames`] を追加する。この関数はフレーム番号と
//! `.dtvi` の対応を一切使わず、統計量（矩形推定の材料）を集めるだけなので、
//! 検査が無くても安全。**[`stream_keyframe_luma_frames`] 自体は検出経路から
//! 呼ばないこと。**（全画面デコード＋呼び出し側での手動クロップという構成が、
//! 下記「ロゴ検出の階層化方式」の罠の対象になるため。フレーム数の一致検査が
//! 無いことはこの制約の理由ではあるが、E18-9 以降は理由ではなく罠そのもの
//! （crop 経路の違いによる画素値のずれ）が本質になった）。
//!
//! ## ロゴ検出の階層化方式が使う関数（E18-9）
//!
//! `detect_logo`（`src/analyze.rs`）は辞書ヒット時でも毎回全編をフルデコード
//! していたが、ロゴ区間の境界は CM 切り替わり付近にしかなく、区間の内部は
//! キーフレームの判定だけで決まる。そこで [`stream_keyframe_cropped_luma_frames`]
//! でキーフレームだけを粗く走査し、判定が変わる GOP だけ
//! [`decode_frame_range_luma_frames`] で部分デコードする（階層化方式、
//! ロジックは `src/logo/hier.rs`）。**この2関数は検出経路（`detect_logo` 内の
//! `detect_logo_scores_hier`）から使う。** [`stream_keyframe_luma_frames`] とは
//! 違い、`.dtvi` との対応検査を呼び出し側が自前で行う（下記「実録画で見つかった
//! 罠」の型のずれとは別に、CLAUDE.md 罠3への通常の防御として必須）。
//!
//! ### 実録画で見つかった罠1: `-vf crop` を経由するかどうかで画素値がずれる
//!
//! 実測（30分1080p実録画）: ffmpeg の `-pix_fmt gray` 変換は、直前に `-vf
//! crop=w:h:x:y` を経由するかどうか・経由する場合のオフセットによって、同じ
//! 元画素に対し異なる出力バイト値を返す（libswscale の経路依存の丸め）。
//! [`stream_keyframe_luma_frames`]（全画面デコード）の出力を呼び出し側で手動
//! クロップした輝度と、[`decode_frame_range_luma_frames`]（`-vf crop` 経由）が
//! 同じフレームに対して返す輝度は、シークが正しく意図したフレームに着地して
//! いてもビット単位で一致しない（最大 ±2/255 程度、単純な矩形でも大半のバイトが
//! ずれる）。そのため階層化方式の2段はどちらも `-vf crop` を経由する
//! [`stream_keyframe_cropped_luma_frames`] / [`decode_frame_range_luma_frames`]
//! を使い、[`stream_keyframe_luma_frames`]（全画面・手動クロップ）は使わない。
//! この罠は合成フィクスチャでは再現しない（単色ロゴでは丸め差が判定に出にくく、
//! 実録画で初めて発覚した）。
//!
//! `stream_keyframe_luma_frames` は矩形推定・辞書候補採点（E18-2/E18-4）用に
//! 「全画面を1回読んで候補ごとに切り出す」設計なので、この罠は影響しない
//! （候補間の相対比較にしか使わず、他の経路とビット単位で比較しないため）。
//!
//! ### 実録画で見つかった罠2: `-ss` は既定でフレーム精度シークを行う
//!
//! 当初 [`decode_frame_range_luma_frames`] の `seek_seconds` は「対象キーフレーム
//! の表示区間の中央（`+0.5` フレーム分）」を指定する設計だった。mp4 の入力シーク
//! は指定時刻**以下**の直近同期サンプルに丸められる、という前提に基づく（境界
//! そのものだと浮動小数点誤差で1フレーム前に着地しうるための安全策）。
//!
//! 実測（乙女ゲー30分1080p実録画）でこの前提は誤りだと判明した。手元の ffmpeg
//! （9.0.1）は `-ss`（`-i` より前、入力シーク）でも**既定でフレーム精度シークを
//! 行い**、デコーダ側で「pts < 指定時刻」のフレームを捨てて出力する。対象
//! キーフレームの pts より**後ろ**の時刻を指定すると、そのキーフレーム自身が
//! 捨てられ、**次のフレームから出力が始まる**。実測でフレーム360/4200/54000の
//! いずれでも「`+0.5`フレーム版の出力」と「対象フレームの1つ後（`+1`）」が
//! ビット単位で完全一致した（つまりズレは1フレーム分の系統的なオフセットで、
//! ランダムな丸め誤差ではない）。
//!
//! ### 実録画で見つかった罠3: pts は0始まりとは限らない
//!
//! 罠2の修正直後は `seek_seconds` を「対象フレーム番号 / fps」の式で近似して
//! いた（対象キーフレームの pts の**手前**を指定する設計自体は正しい）。
//! 実録画（`start_time` ヘッダが0の素材）ではこの近似で問題無かったが、
//! レビューで追加した E2E フィクスチャ（`tests/fixtures/sample.mp4`、
//! `start_time` ヘッダが2002）で破綻した: 実際の pts は
//! `frame_number * 1001 + 2002` で、式による近似はこのオフセットを無視して
//! 2フレーム早い時刻を指定してしまい、末尾GOPの部分デコードで実際に読めた
//! 枚数が期待値と2フレームずれた（issue #154 レビュー指摘）。
//!
//! 修正と詳細は [`crate::logo::hier::seek_seconds_for_pts`] の doc comment
//! 参照（式による近似ではなく `.dtvi` のフレーム表に記録された実測 pts を
//! そのまま使うよう修正した）。
//!
//! 修正後は着地オラクル（`src/analyze.rs::verify_landing_oracle`）が
//! **完全一致（`==`）**で検査でき、実録画3本＋`sample.mp4`/`sample_logo.mp4`
//! いずれでも実際に完全一致することを確認済み（罠1のような crop 経路依存の
//! 丸めは、両段とも `-vf crop` を経由させている限りここでは発生しない）。
//! 罠2・罠3はいずれも合成フィクスチャでは当初再現しなかった（罠2はGOPが短い
//! 合成フィクスチャでは1フレームのオフセットが表面化しにくく実録画で初めて
//! 発覚、罠3は`start_time`ヘッダが0の実録画では近似がたまたま正しい値になり、
//! `start_time`が非0のE2Eフィクスチャで初めて発覚した）。

use std::io::{ErrorKind, Read};
use std::path::Path;

use anyhow::{bail, Context};

use crate::errctx::PathContext as _;
use crate::external;

/// ロゴ矩形。ffmpeg の `crop=w:h:x:y` にそのまま渡す座標系
/// （映像の左上を原点とする表示ピクセル座標）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogoRect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl LogoRect {
    /// 矩形が `video_width` x `video_height` の映像に収まっているか検査する。
    ///
    /// ffmpeg の crop フィルタのエラー文字列に頼らず、矩形と映像サイズを明示した
    /// メッセージを返す（モジュール doc comment「矩形の範囲外検査」参照）。
    fn validate(self, video_width: u32, video_height: u32) -> anyhow::Result<()> {
        if self.w == 0 || self.h == 0 {
            bail!(
                "ロゴ矩形の幅・高さは正の値である必要があります: w={}, h={}",
                self.w,
                self.h
            );
        }
        let right = self.x.checked_add(self.w);
        let bottom = self.y.checked_add(self.h);
        let fits = matches!(right, Some(r) if r <= video_width)
            && matches!(bottom, Some(b) if b <= video_height);
        if !fits {
            bail!(
                "ロゴ矩形が映像範囲の外に出ています: 矩形=(x={}, y={}, w={}, h={}), \
                 映像=({}x{})。x+w<=映像幅 かつ y+h<=映像高さ である必要があります。",
                self.x,
                self.y,
                self.w,
                self.h,
                video_width,
                video_height,
            );
        }
        Ok(())
    }

    /// 1フレーム分のバイト数（`gray` は1ピクセル1バイト）。
    fn frame_bytes(self) -> usize {
        self.w as usize * self.h as usize
    }
}

/// `input` の映像サイズ。呼び出し側が `.dtvi` のヘッダ等から渡す（`stream_luma_frames`
/// はここでは probe しない）。[`LogoRect`] とまとめて1引数にすることで、
/// `stream_luma_frames` の引数数を clippy の `too_many_arguments` の閾値内に収める。
///
/// **実際に ffmpeg がデコードするサイズと一致していること。** `width`/`height` は
/// 1フレームのバイト数（`width*height`）を決めるのに使われ、その値が実際の出力と
/// ずれていると、合計バイト数さえ `frame_bytes` で割り切れれば端数バイトの検査も
/// （`stream_keyframe_luma_frames` の）0フレーム検査も素通りしたまま、フレームの
/// 内容が黙って境界からずれて読み込まれる（CLAUDE.md 罠3の一般形）。`0` は
/// 明示的に拒否される（`stream_keyframe_luma_frames` 参照。`frame_bytes==0` の
/// まま ffmpeg を起動すると `wait()` がハングするため）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoSize {
    pub width: u32,
    pub height: u32,
}

/// `ffmpeg` を起動し、`input` からロゴ矩形 `rect` の輝度平面を1フレームずつ
/// `on_frame` に渡す。
///
/// - `video_size`: `input` の映像サイズ。`rect` がこの範囲に収まっているかを
///   ffmpeg を起動する前に検査する。
/// - `expected_frame_count`: `.dtvi` の `frame_count`。読み終えた時点でこの値と
///   実際に読めたフレーム数が一致しなければエラーにする（省略不可、モジュール
///   doc comment参照）。
/// - `cwd`: `ffmpeg` の作業ディレクトリ（`external::spawn_streaming` にそのまま渡す）。
///   `input` は絶対パスに変換した上で渡すため、cwd の値自体はこの呼び出しの結果に
///   影響しない。
///
/// 戻り値は実際に読めたフレーム数（`expected_frame_count` と必ず一致する）。
///
/// `ffmpeg` が見つからない・異常終了した場合は `external::spawn_streaming` /
/// `StreamingChild::wait` の流儀（コマンドラインと stderr の末尾を含むエラー）
/// に従う。`on_frame` がエラーを返した場合はそのエラーがそのまま返る
/// （`ffmpeg` はまだ動いている可能性があるため内部で `kill()` するが、
/// `kill` によるシグナル終了エラーで `on_frame` 本来のエラーを隠さない）。
pub fn stream_luma_frames(
    ffmpeg: &Path,
    input: &Path,
    cwd: &Path,
    rect: LogoRect,
    video_size: VideoSize,
    expected_frame_count: u64,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    rect.validate(video_size.width, video_size.height)?;

    let absolute_input =
        std::fs::canonicalize(input).path_ctx("入力ファイルの絶対パス解決", input)?;
    let filter = format!("crop={}:{}:{}:{}", rect.w, rect.h, rect.x, rect.y);
    let input_arg = absolute_input.as_os_str();

    let args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-loglevel"),
        std::ffi::OsStr::new("error"),
        std::ffi::OsStr::new("-i"),
        input_arg,
        std::ffi::OsStr::new("-map"),
        std::ffi::OsStr::new("0:v:0"),
        std::ffi::OsStr::new("-an"),
        std::ffi::OsStr::new("-sn"),
        std::ffi::OsStr::new("-vf"),
        std::ffi::OsStr::new(&filter),
        std::ffi::OsStr::new("-pix_fmt"),
        std::ffi::OsStr::new("gray"),
        std::ffi::OsStr::new("-fps_mode"),
        std::ffi::OsStr::new("passthrough"),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("rawvideo"),
        std::ffi::OsStr::new("-"),
    ];

    let mut child = external::spawn_streaming(ffmpeg, &args, cwd)?;
    let read_result = read_frames(
        child.stdout(),
        rect.frame_bytes(),
        expected_frame_count,
        on_frame,
    );
    finish_stream(child, read_result)
}

/// `ffmpeg` を起動し、`input` の**キーフレームだけ**をデコードして、クロップして
/// いない全画面の輝度平面を1フレームずつ `on_frame` に渡す。
///
/// ロゴ矩形を入力自身から推定する処理（別 issue）専用の関数。[`stream_luma_frames`]
/// との違いと、その違いにもかかわらずフレーム数の一致検査を省いて安全な理由は
/// モジュール doc comment「[`stream_keyframe_luma_frames`] を別関数にした理由」
/// 参照。**この関数自体は `.dtvi` のフレーム番号との対応を検査しないので、
/// 検出経路からは呼ばないこと。** 検出経路（`detect_logo`）は、辞書ヒット時の
/// フルデコードなら [`stream_luma_frames`]、階層化方式（E18-9）の粗い走査なら
/// 全画面ではなく矩形をffmpeg側で切り出す [`stream_keyframe_cropped_luma_frames`]
/// を使う。後者は名前は似ているが別の関数で、`.dtvi` との対応検査を呼び出し側
/// （`detect_logo_scores_hier`）が自前で行う設計（モジュール doc comment「ロゴ
/// 検出の階層化方式が使う関数」参照）。この関数（`stream_keyframe_luma_frames`）
/// 自身が検出経路から呼ばれることはない。
///
/// GOP は 120 フレーム固定でシーンチェンジ由来の IDR がない（CLAUDE.md「前提」）ため、
/// キーフレームは約4秒等間隔＝時間的に偏りのない標本になり、矩形推定にはこれで足りる。
///
/// - `video_size`: `input` の映像サイズ。1フレームのバイト数（`width*height`、
///   `gray` は1ピクセル1バイト）を決めるために使う。
/// - `cwd`: `ffmpeg` の作業ディレクトリ（`external::spawn_streaming` にそのまま渡す）。
///   `input` は絶対パスに変換した上で渡すため、cwd の値自体はこの呼び出しの結果に
///   影響しない。
///
/// 戻り値は実際に読めたキーフレーム数。**0枚だった場合はエラーにする**
/// （`-skip_frame nokey` の綴りを間違える等で黙って0枚になると、後続の矩形推定が
/// 「ロゴ無し」と誤判定するだけで気づけないため）。
///
/// `ffmpeg` の引数は `stream_luma_frames` と同じ流儀に、`-skip_frame nokey`（`-i`
/// より前、デコーダ側の入力オプション。後ろに置くと出力側のオプションとして
/// 解釈され、全フレームがデコードされてしまう）を加え、`-vf crop=...` を外した
/// もの。
///
/// `ffmpeg` が見つからない・異常終了した場合や `on_frame` がエラーを返した場合の
/// 扱いは [`stream_luma_frames`] と同じ（コマンドラインと stderr の末尾を含む
/// エラー、`on_frame` 中断時は `kill()` してから `wait()` の結果を捨てる。
/// モジュール doc comment「デッドロック回避」参照）。
pub fn stream_keyframe_luma_frames(
    ffmpeg: &Path,
    input: &Path,
    cwd: &Path,
    video_size: VideoSize,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    let frame_bytes = video_size.width as usize * video_size.height as usize;
    if frame_bytes == 0 {
        // `LogoRect::validate` の「ffmpeg を起動する前に自分で検査してメッセージを
        // 明示する」流儀に合わせる（モジュール doc comment「矩形の範囲外検査」）。
        // `stream_luma_frames` は `rect.validate` が `rect.w/h>0` を要求し、それが
        // `video_size` にも収まることを検査するため `frame_bytes==0` になり得ない
        // が、この関数には crop も rect も無いので自分で検査する必要がある。
        //
        // ここを検査せず ffmpeg を起動すると: 読み取りループが1バイトも読まずに
        // `Ok(0)` で即 break → 0フレームでエラー（`ReadFramesError::Protocol`）
        // → `Protocol` 分岐は「reader が EOF に達した後なので `wait()` は安全」と
        // 判断して `kill()` せず `child.wait()` を呼ぶ → しかし ffmpeg は誰も
        // 読まない stdout パイプが埋まって書き込みブロック中で終了しておらず、
        // `wait()` が永久に返らない（実測: 45秒以上ハング）。
        bail!(
            "映像サイズが不正です: width={}, height={}。0を含む値のまま ffmpeg を\
             起動すると1フレームのバイト数が0になり、誰も読まない標準出力パイプが\
             埋まって `wait()` が返らずハングします。",
            video_size.width,
            video_size.height,
        );
    }

    let absolute_input =
        std::fs::canonicalize(input).path_ctx("入力ファイルの絶対パス解決", input)?;
    let input_arg = absolute_input.as_os_str();

    let args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-loglevel"),
        std::ffi::OsStr::new("error"),
        std::ffi::OsStr::new("-skip_frame"),
        std::ffi::OsStr::new("nokey"),
        std::ffi::OsStr::new("-i"),
        input_arg,
        std::ffi::OsStr::new("-map"),
        std::ffi::OsStr::new("0:v:0"),
        std::ffi::OsStr::new("-an"),
        std::ffi::OsStr::new("-sn"),
        std::ffi::OsStr::new("-pix_fmt"),
        std::ffi::OsStr::new("gray"),
        std::ffi::OsStr::new("-fps_mode"),
        std::ffi::OsStr::new("passthrough"),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("rawvideo"),
        std::ffi::OsStr::new("-"),
    ];

    let mut child = external::spawn_streaming(ffmpeg, &args, cwd)?;
    let read_result = read_keyframe_frames(child.stdout(), frame_bytes, on_frame);
    finish_stream(child, read_result)
}

/// ロゴ検出の階層化方式（issue #154）第1段（粗い走査）専用。
/// [`stream_keyframe_luma_frames`] と違い、**`-vf crop=...` を ffmpeg 側で行う**
/// （全画面デコード＋呼び出し側での手動クロップは使わない。モジュール
/// doc comment「実録画で見つかった罠」参照）。
///
/// - `rect`/`video_size`: [`stream_luma_frames`] と同じ（クロップ座標・矩形の
///   範囲外検査）。
/// - `-skip_frame nokey`: [`stream_keyframe_luma_frames`] と同じ、`-i` より前
///   （デコーダ側の入力オプション）。
///
/// 戻り値は実際に読めたキーフレーム数。内部で使う [`read_keyframe_frames`]
/// が0枚を検出したらエラーにするが、`.dtvi` のキーフレーム数との**一致**検査は
/// この関数自身は行わない（レビュー指摘: 以前のdoc commentは「0枚チェックも
/// 一致検査も行わない」と書いていたが誤り。0枚チェックは `read_keyframe_frames`
/// が行っている）。検出経路の呼び出し側 `detect_logo_scores_hier` が一致検査を
/// 必ず行うため、ここで重複させない。
pub fn stream_keyframe_cropped_luma_frames(
    ffmpeg: &Path,
    input: &Path,
    cwd: &Path,
    rect: LogoRect,
    video_size: VideoSize,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    rect.validate(video_size.width, video_size.height)?;

    let absolute_input =
        std::fs::canonicalize(input).path_ctx("入力ファイルの絶対パス解決", input)?;
    let filter = format!("crop={}:{}:{}:{}", rect.w, rect.h, rect.x, rect.y);
    let input_arg = absolute_input.as_os_str();

    let args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-loglevel"),
        std::ffi::OsStr::new("error"),
        std::ffi::OsStr::new("-skip_frame"),
        std::ffi::OsStr::new("nokey"),
        std::ffi::OsStr::new("-i"),
        input_arg,
        std::ffi::OsStr::new("-map"),
        std::ffi::OsStr::new("0:v:0"),
        std::ffi::OsStr::new("-an"),
        std::ffi::OsStr::new("-sn"),
        std::ffi::OsStr::new("-vf"),
        std::ffi::OsStr::new(&filter),
        std::ffi::OsStr::new("-pix_fmt"),
        std::ffi::OsStr::new("gray"),
        std::ffi::OsStr::new("-fps_mode"),
        std::ffi::OsStr::new("passthrough"),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("rawvideo"),
        std::ffi::OsStr::new("-"),
    ];

    let mut child = external::spawn_streaming(ffmpeg, &args, cwd)?;
    let read_result = read_keyframe_frames(child.stdout(), rect.frame_bytes(), on_frame);
    finish_stream(child, read_result)
}

/// ロゴ検出の階層化方式（issue #154）第2段（精緻化）専用。`-ss <seek_seconds>`
/// の入力シークで対象範囲の先頭キーフレームまで飛び、そこから `frame_count`
/// フレームだけロゴ矩形をクロップして部分デコードし、`on_frame` に渡す。
///
/// - `rect`/`video_size`: [`stream_luma_frames`] と同じ（クロップ座標・矩形の
///   範囲外検査）。
/// - `seek_seconds`: 呼び出し側が対象キーフレームの**実測 pts**（`.dtvi` の
///   フレーム表の値、`frame_number / fps` のような式による近似ではない）から
///   `hier::seek_seconds_for_pts` で計算した値を渡す。mp4 の入力シークは
///   指定時刻**以下**の直近同期サンプルに着地するのではなく、ffmpeg の既定の
///   フレーム精度シークにより「pts < 指定時刻」のフレームが捨てられる
///   （モジュール doc comment「実録画で見つかった罠2」参照）ため、対象
///   キーフレームの pts の**わずかに手前**を指定する必要がある。
/// - `frame_count`: `.dtvi` のキーフレーム番号の差分（次の境界までの距離）から
///   呼び出し側が算出する。読めたフレーム数がこれと一致しなければエラーにする
///   （[`stream_luma_frames`] と同じ流儀、`-frames:v` で出力側にも制限を掛けるが
///   シーク先で入力が尽きた場合はそれより少なく終わりうるため検査は必要）。
///
/// 着地したフレームが本当に意図したキーフレームかどうかはここでは検証しない
/// （この関数は ffmpeg を起動するだけの薄いラッパー）。呼び出し側
/// （`detect_logo_scores_hier`）が、返った先頭フレームの corr と第1段
/// （キーフレーム走査）で得た同じキーフレームの corr が完全一致することを
/// 確認する（CLAUDE.md 罠3の一般形、着地オラクル）。`seek_seconds` の計算を
/// 誤ると（モジュール doc comment「実録画で見つかった罠2」「罠3」参照）着地が
/// ずれ、この検査で必ず捕まる。
#[allow(clippy::too_many_arguments)]
pub fn decode_frame_range_luma_frames(
    ffmpeg: &Path,
    input: &Path,
    cwd: &Path,
    rect: LogoRect,
    video_size: VideoSize,
    seek_seconds: f64,
    frame_count: u64,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    rect.validate(video_size.width, video_size.height)?;

    let absolute_input =
        std::fs::canonicalize(input).path_ctx("入力ファイルの絶対パス解決", input)?;
    let filter = format!("crop={}:{}:{}:{}", rect.w, rect.h, rect.x, rect.y);
    let input_arg = absolute_input.as_os_str();
    // `-ss` は `-i` より前（入力シーク）。値は呼び出し側が計算した秒数を
    // そのまま文字列化する（ffmpeg は小数秒の `-ss` を受け付ける）。
    let seek_arg = format!("{seek_seconds}");
    let count_arg = frame_count.to_string();

    let args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-loglevel"),
        std::ffi::OsStr::new("error"),
        std::ffi::OsStr::new("-ss"),
        std::ffi::OsStr::new(&seek_arg),
        std::ffi::OsStr::new("-i"),
        input_arg,
        std::ffi::OsStr::new("-map"),
        std::ffi::OsStr::new("0:v:0"),
        std::ffi::OsStr::new("-an"),
        std::ffi::OsStr::new("-sn"),
        std::ffi::OsStr::new("-vf"),
        std::ffi::OsStr::new(&filter),
        std::ffi::OsStr::new("-pix_fmt"),
        std::ffi::OsStr::new("gray"),
        std::ffi::OsStr::new("-fps_mode"),
        std::ffi::OsStr::new("passthrough"),
        std::ffi::OsStr::new("-frames:v"),
        std::ffi::OsStr::new(&count_arg),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("rawvideo"),
        std::ffi::OsStr::new("-"),
    ];

    let mut child = external::spawn_streaming(ffmpeg, &args, cwd)?;
    let read_result = read_frames(child.stdout(), rect.frame_bytes(), frame_count, on_frame);
    finish_stream(child, read_result)
}

/// 末尾GOPの部分デコード専用（issue #154 レビュー指摘）。`-frames:v` を付けず
/// EOFまで読み、実際に読めたフレーム数を返す。
///
/// [`decode_frame_range_luma_frames`] は呼び出し側が渡す `frame_count` との
/// 一致を検査するが、末尾GOPの `frame_count` は `.dtvi` ヘッダの `frame_count`
/// （信用してよいとは限らない値。issue #154「罠」・CLAUDE.md 罠3の一般形）から
/// 算出するため、そのヘッダが実際のメディアより小さい値を主張していても
/// 「指定した枚数だけ読めれば成功」という判定を素通りしてしまう。この関数は
/// 枚数を指定せず実際にメディアの終端まで読むため、戻り値を呼び出し側が
/// `.dtvi` から算出した期待値と比較すれば、**メディア側の実際の値**との
/// 食い違いを検出できる（`detect_logo_scores_hier` 参照）。
///
/// 内部の読み取りは [`read_keyframe_frames`] を再利用する（名前は
/// キーフレーム走査用だが、実装は「EOFまで読み、1バイトも読めなければ
/// エラーにする」だけで枚数の期待値を取らない。この関数の用途にも合致する）。
pub fn decode_from_seek_until_eof_luma_frames(
    ffmpeg: &Path,
    input: &Path,
    cwd: &Path,
    rect: LogoRect,
    video_size: VideoSize,
    seek_seconds: f64,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> anyhow::Result<u64> {
    rect.validate(video_size.width, video_size.height)?;

    let absolute_input =
        std::fs::canonicalize(input).path_ctx("入力ファイルの絶対パス解決", input)?;
    let filter = format!("crop={}:{}:{}:{}", rect.w, rect.h, rect.x, rect.y);
    let input_arg = absolute_input.as_os_str();
    let seek_arg = format!("{seek_seconds}");

    let args: Vec<&std::ffi::OsStr> = vec![
        std::ffi::OsStr::new("-hide_banner"),
        std::ffi::OsStr::new("-loglevel"),
        std::ffi::OsStr::new("error"),
        std::ffi::OsStr::new("-ss"),
        std::ffi::OsStr::new(&seek_arg),
        std::ffi::OsStr::new("-i"),
        input_arg,
        std::ffi::OsStr::new("-map"),
        std::ffi::OsStr::new("0:v:0"),
        std::ffi::OsStr::new("-an"),
        std::ffi::OsStr::new("-sn"),
        std::ffi::OsStr::new("-vf"),
        std::ffi::OsStr::new(&filter),
        std::ffi::OsStr::new("-pix_fmt"),
        std::ffi::OsStr::new("gray"),
        std::ffi::OsStr::new("-fps_mode"),
        std::ffi::OsStr::new("passthrough"),
        std::ffi::OsStr::new("-f"),
        std::ffi::OsStr::new("rawvideo"),
        std::ffi::OsStr::new("-"),
    ];

    let mut child = external::spawn_streaming(ffmpeg, &args, cwd)?;
    let read_result = read_keyframe_frames(child.stdout(), rect.frame_bytes(), on_frame);
    finish_stream(child, read_result)
}

/// `read_frames` / `read_keyframe_frames` の結果を受けて `child` の後始末をする
/// （[`stream_luma_frames`] と [`stream_keyframe_luma_frames`] の共通処理）。
///
/// - `Protocol` エラー（フレーム数不一致・端数バイト等）は、いずれも reader が
///   EOF に達した後（ffmpeg は既に終了しているはず）に起きるため `wait()` は
///   ブロックしないので安全に呼べる。ffmpeg 自体も異常終了していた場合は、
///   `Protocol` エラーより根本原因に近いのでそちらを優先する（そうしないと
///   「壊れた入力で ffmpeg が落ちた」が常に「フレーム数がずれている」という
///   無関係な誤誘導メッセージに隠れる）。
/// - `Callback` エラー（`on_frame` がエラーを返して読み取りを中断した場合）は
///   EOF 前で、ffmpeg がまだ書き込み中の可能性がある。そのまま `wait()` すると
///   パイプが詰まってデッドロックしうるため、先に `kill()` してから `wait()` は
///   結果を捨てて reap だけする（`kill` によるシグナル終了エラーが `on_frame`
///   本来のエラーを隠してしまうため、`wait()` の結果は使わない）。
fn finish_stream(
    mut child: external::StreamingChild,
    read_result: Result<u64, ReadFramesError>,
) -> anyhow::Result<u64> {
    match read_result {
        Err(ReadFramesError::Protocol(protocol_err)) => match child.wait() {
            Err(wait_err) => Err(wait_err),
            Ok(()) => Err(protocol_err),
        },
        Err(ReadFramesError::Callback(callback_err)) => {
            child.kill();
            let _ = child.wait();
            Err(callback_err)
        }
        Ok(frame_count) => {
            child.wait()?;
            Ok(frame_count)
        }
    }
}

/// [`read_frames`] / [`read_keyframe_frames`] の失敗要因。`finish_stream`
/// （`stream_luma_frames` と `stream_keyframe_luma_frames` の両方が使う）が
/// `wait()` の呼び方を分けるために区別する（詳細は `finish_stream` の
/// doc comment参照）。
#[derive(Debug)]
enum ReadFramesError {
    /// フレーム数不一致・端数バイト、または `fill_or_eof` 自体の I/O エラー。
    /// 前2つは reader が既に EOF に達している（ffmpeg は既に終了しているはずなので
    /// `wait()` は安全）。**I/O エラーの方は理論上 EOF 前にも起きうる**が、パイプの
    /// 読み取りでは（`Interrupted` 以外は）通常起こらないため、ここでは区別せず
    /// 同じ扱いにしている。
    Protocol(anyhow::Error),
    /// `on_frame` コールバックが返したエラー。EOF 前で中断したため、ffmpeg が
    /// まだ動いている可能性がある（`wait()` の前に `kill()` が必要）。
    Callback(anyhow::Error),
}

impl std::fmt::Display for ReadFramesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ReadFramesError::Protocol(e) | ReadFramesError::Callback(e) => write!(f, "{e}"),
        }
    }
}

/// `reader` から輝度フレームを `frame_bytes` バイトずつ読み、フレームごとに
/// `on_frame` を呼ぶ。[`read_frames`] と [`read_keyframe_frames`] の共通部分
/// （ffmpeg プロセスの起動から分離しているのは、ffmpeg を起動せずにこの読み取り
/// ロジック単体をテストするため）。
///
/// フレーム数が確定した後の検査（`.dtvi` との一致 / 0フレーム禁止）は用途によって
/// 異なるため、この関数には含めず呼び出し側がそれぞれ行う。
///
/// `frame_bytes` の倍数でない終わり方（端数バイト）は共通してエラーにする。
fn read_luma_frames<R: Read>(
    mut reader: R,
    frame_bytes: usize,
    mut on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> Result<u64, ReadFramesError> {
    let mut buf = vec![0u8; frame_bytes];
    let mut frame_count: u64 = 0;
    loop {
        let n = fill_or_eof(&mut reader, &mut buf).map_err(ReadFramesError::Protocol)?;
        if n == 0 {
            break;
        }
        if n != frame_bytes {
            return Err(ReadFramesError::Protocol(anyhow::anyhow!(
                "ffmpeg の出力が1フレーム分のバイト数({frame_bytes}バイト)の倍数になって\
                 いません: フレーム{frame_count}個目の途中、実際は{n}バイトで終わっています。\
                 ロゴ矩形の座標や crop フィルタの指定を確認してください。"
            )));
        }
        frame_count += 1;
        on_frame(&buf).map_err(ReadFramesError::Callback)?;
    }
    Ok(frame_count)
}

/// [`read_luma_frames`] で読み、読み終えた時点で `expected_frame_count` と実際の
/// フレーム数が一致しなければエラーにする（モジュール doc comment「座標系が
/// この issue の本質」参照。一致しないまま後続に進むと CM の位置が黙ってずれる）。
/// [`stream_luma_frames`] 用。
fn read_frames<R: Read>(
    reader: R,
    frame_bytes: usize,
    expected_frame_count: u64,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> Result<u64, ReadFramesError> {
    let frame_count = read_luma_frames(reader, frame_bytes, on_frame)?;
    if frame_count != expected_frame_count {
        return Err(ReadFramesError::Protocol(anyhow::anyhow!(
            "ffmpeg から読み取ったフレーム数({frame_count})が .dtvi の frame_count\
             ({expected_frame_count})と一致しません。この不一致を無視して後続の\
             ロゴ検出に進むと、ロゴ区間だけがフレーム数ぶんずれた Trim が出て、\
             CM の位置が黙ってずれます（join_logo_scp はこのずれをエラーにしません）。"
        )));
    }
    Ok(frame_count)
}

/// [`read_luma_frames`] で読み、**0フレームをエラーにする**（[`read_frames`] との
/// 違い、モジュール doc comment「[`stream_keyframe_luma_frames`] を別関数にした
/// 理由」参照）。`expected_frame_count` を取らない・一致検査もしない
/// （矩形推定はフレーム番号と `.dtvi` の対応を使わないため）。
/// [`stream_keyframe_luma_frames`] 用。
fn read_keyframe_frames<R: Read>(
    reader: R,
    frame_bytes: usize,
    on_frame: impl FnMut(&[u8]) -> anyhow::Result<()>,
) -> Result<u64, ReadFramesError> {
    let frame_count = read_luma_frames(reader, frame_bytes, on_frame)?;
    if frame_count == 0 {
        return Err(ReadFramesError::Protocol(anyhow::anyhow!(
            "ffmpeg からキーフレームを1枚も読み取れませんでした。`-skip_frame nokey` \
             の指定が効いていない可能性があります。黙って0枚のまま後続のロゴ矩形推定に\
             進むと、実際にはロゴがあっても「ロゴ無し」と誤判定されるだけで気づけません。"
        )));
    }
    Ok(frame_count)
}

/// `buf` を満たすまで読み、EOF に達したら実際に読めたバイト数を返す。
///
/// `Read::read_exact` は「バッファの先頭で EOF（=正常終了）」と「バッファの途中で
/// EOF（=端数バイト、異常）」を区別せず両方 `UnexpectedEof` にしてしまうため使えない
/// （`read_frames` はこの2つを区別する必要がある）。自前でループして読めたバイト数を
/// 数える。
fn fill_or_eof<R: Read>(reader: &mut R, buf: &mut [u8]) -> anyhow::Result<usize> {
    let mut total = 0;
    while total < buf.len() {
        match reader.read(&mut buf[total..]) {
            Ok(0) => break,
            Ok(n) => total += n,
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e).context("ffmpeg の標準出力の読み取りに失敗しました"),
        }
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    // ---------------------------------------------------------------
    // LogoRect::validate
    // ---------------------------------------------------------------

    #[test]
    fn rect_within_bounds_is_ok() {
        let rect = LogoRect {
            x: 10,
            y: 20,
            w: 30,
            h: 40,
        };
        assert!(rect.validate(640, 360).is_ok());
        // ちょうど境界に収まる場合も ok。
        let exact = LogoRect {
            x: 610,
            y: 320,
            w: 30,
            h: 40,
        };
        assert!(exact.validate(640, 360).is_ok());
    }

    #[test]
    fn rect_extending_past_width_is_an_error() {
        let rect = LogoRect {
            x: 620,
            y: 0,
            w: 30,
            h: 40,
        };
        let err = rect.validate(640, 360).unwrap_err();
        let message = err.to_string();
        assert!(message.contains("映像範囲の外"), "message={message}");
        assert!(message.contains("640"), "message={message}");
    }

    #[test]
    fn rect_extending_past_height_is_an_error() {
        let rect = LogoRect {
            x: 0,
            y: 340,
            w: 30,
            h: 40,
        };
        assert!(rect.validate(640, 360).is_err());
    }

    #[test]
    fn rect_with_overflowing_coordinates_is_an_error_not_a_panic() {
        let rect = LogoRect {
            x: u32::MAX,
            y: 0,
            w: 30,
            h: 40,
        };
        assert!(rect.validate(640, 360).is_err());
    }

    #[test]
    fn zero_sized_rect_is_an_error() {
        let rect = LogoRect {
            x: 0,
            y: 0,
            w: 0,
            h: 10,
        };
        let err = rect.validate(640, 360).unwrap_err();
        assert!(err.to_string().contains("幅・高さは正の値"));
    }

    // ---------------------------------------------------------------
    // read_frames / fill_or_eof（ffmpeg を起動しない純粋なロジックのテスト）
    // ---------------------------------------------------------------

    #[test]
    fn reads_expected_number_of_whole_frames() {
        // frame_bytes=4, 3フレームぶん = 12バイト。
        let data: Vec<u8> = (0..12).collect();
        let mut collected: Vec<Vec<u8>> = Vec::new();
        let n = read_frames(Cursor::new(data), 4, 3, |frame| {
            collected.push(frame.to_vec());
            Ok(())
        })
        .expect("フレーム数が一致するので成功するはず");

        assert_eq!(n, 3);
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], vec![0, 1, 2, 3]);
        assert_eq!(collected[1], vec![4, 5, 6, 7]);
        assert_eq!(collected[2], vec![8, 9, 10, 11]);
    }

    #[test]
    fn mismatched_expected_frame_count_is_an_error() {
        let data: Vec<u8> = (0..12).collect(); // 3フレームぶん
        let err = read_frames(Cursor::new(data), 4, 4, |_| Ok(()))
            .expect_err("期待値(4)と実際(3)が食い違うのでエラーになるはず");
        let message = err.to_string();
        assert!(message.contains("CM"), "message={message}");
        assert!(message.contains('3'), "message={message}");
        assert!(message.contains('4'), "message={message}");
    }

    #[test]
    fn actual_frame_count_exceeding_expected_is_an_error() {
        // frame_bytes=4 の4フレーム(16バイト)に対し、期待値を3と偽る。
        let data: Vec<u8> = (0..16).collect();
        let mut called = 0;
        let err = read_frames(Cursor::new(data), 4, 3, |_| {
            called += 1;
            Ok(())
        })
        .expect_err("期待値(3)と実際(4)が食い違うのでエラーになるはず");
        // 実際が期待を上回る方向でも、超過分を含めて on_frame は呼ばれてから
        // エラーになる（読み取り自体は最後まで進む。副作用が先に出る）。
        assert_eq!(called, 4, "4フレームぶんon_frameが呼ばれるはず");
        let message = err.to_string();
        assert!(message.contains('4'), "message={message}");
        assert!(message.contains('3'), "message={message}");
    }

    #[test]
    fn trailing_partial_bytes_is_an_error() {
        // frame_bytes=4 の倍数でない14バイト(3フレーム+2バイトの端数)。
        let data: Vec<u8> = (0..14).collect();
        let err = read_frames(Cursor::new(data), 4, 3, |_| Ok(()))
            .expect_err("端数バイトで終わるのでエラーになるはず");
        assert!(err.to_string().contains("倍数"), "message={}", err);
    }

    #[test]
    fn empty_input_with_zero_expected_frames_succeeds() {
        let n = read_frames(Cursor::new(Vec::<u8>::new()), 4, 0, |_| Ok(()))
            .expect("0フレーム期待に対して空入力は成功するはず");
        assert_eq!(n, 0);
    }

    #[test]
    fn on_frame_error_propagates_immediately() {
        let data: Vec<u8> = (0..12).collect();
        let mut calls = 0;
        let err = read_frames(Cursor::new(data), 4, 3, |_| {
            calls += 1;
            bail!("callback error")
        })
        .expect_err("on_frame のエラーが伝播するはず");
        assert_eq!(calls, 1, "1回目のフレームでエラーになったら以降は読まない");
        assert!(err.to_string().contains("callback error"));
    }

    // ---------------------------------------------------------------
    // read_keyframe_frames（stream_keyframe_luma_frames 用、ffmpeg を起動しない）
    // ---------------------------------------------------------------

    #[test]
    fn keyframe_reads_expected_number_of_whole_frames_without_expected_count() {
        // frame_bytes=4, 3フレームぶん = 12バイト。expected_frame_count に相当する
        // 引数を持たない（キーフレームだけ読むので `.dtvi` の frame_count とは
        // 一致しなくてよい）。
        let data: Vec<u8> = (0..12).collect();
        let mut collected: Vec<Vec<u8>> = Vec::new();
        let n = read_keyframe_frames(Cursor::new(data), 4, |frame| {
            collected.push(frame.to_vec());
            Ok(())
        })
        .expect("キーフレームが3枚読めるはず");

        assert_eq!(n, 3);
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], vec![0, 1, 2, 3]);
        assert_eq!(collected[1], vec![4, 5, 6, 7]);
        assert_eq!(collected[2], vec![8, 9, 10, 11]);
    }

    #[test]
    fn keyframe_trailing_partial_bytes_is_an_error() {
        // frame_bytes=4 の倍数でない14バイト(3フレーム+2バイトの端数)。
        let data: Vec<u8> = (0..14).collect();
        let err = read_keyframe_frames(Cursor::new(data), 4, |_| Ok(()))
            .expect_err("端数バイトで終わるのでエラーになるはず");
        assert!(err.to_string().contains("倍数"), "message={err}");
    }

    #[test]
    fn keyframe_zero_frames_is_an_error() {
        let err = read_keyframe_frames(Cursor::new(Vec::<u8>::new()), 4, |_| Ok(()))
            .expect_err("0フレームはエラーになるはず（黙って『ロゴ無し』誤判定を防ぐ）");
        assert!(
            err.to_string().contains("1枚も読み取れません"),
            "message={err}"
        );
    }

    #[test]
    fn keyframe_on_frame_error_propagates_immediately() {
        let data: Vec<u8> = (0..12).collect();
        let mut calls = 0;
        let err = read_keyframe_frames(Cursor::new(data), 4, |_| {
            calls += 1;
            bail!("callback error")
        })
        .expect_err("on_frame のエラーが伝播するはず");
        assert_eq!(calls, 1, "1回目のフレームでエラーになったら以降は読まない");
        assert!(err.to_string().contains("callback error"));
    }

    // ---------------------------------------------------------------
    // stream_keyframe_luma_frames: frame_bytes==0 の起動前拒否（レビューで
    // 見つかった回帰の防止。ffmpeg を実際には起動しないため、この検査が
    // 効いていない場合はこのテスト自体が長時間ハングして気付ける）。
    // ---------------------------------------------------------------

    #[test]
    fn keyframe_zero_width_is_rejected_before_spawning_ffmpeg() {
        // width=0 のまま ffmpeg を起動すると、1フレームのバイト数が0になり、
        // 誰も読まない標準出力パイプが埋まって `wait()` がハングする（実測で
        // 確認済みの回帰）。存在しない ffmpeg / 入力パスを渡しても、起動前の
        // 検査で弾かれるのでこのテストは高速に完走するはず。
        let err = stream_keyframe_luma_frames(
            Path::new("/nonexistent/ffmpeg-must-not-be-invoked"),
            Path::new("/nonexistent/input-must-not-be-touched.mp4"),
            Path::new("/tmp"),
            VideoSize {
                width: 0,
                height: 360,
            },
            |_| Ok(()),
        )
        .expect_err("width=0 は ffmpeg を起動する前に弾かれるはず");
        assert!(
            err.to_string().contains("映像サイズが不正"),
            "message={err}"
        );
    }

    #[test]
    fn keyframe_zero_height_is_rejected_before_spawning_ffmpeg() {
        let err = stream_keyframe_luma_frames(
            Path::new("/nonexistent/ffmpeg-must-not-be-invoked"),
            Path::new("/nonexistent/input-must-not-be-touched.mp4"),
            Path::new("/tmp"),
            VideoSize {
                width: 640,
                height: 0,
            },
            |_| Ok(()),
        )
        .expect_err("height=0 は ffmpeg を起動する前に弾かれるはず");
        assert!(
            err.to_string().contains("映像サイズが不正"),
            "message={err}"
        );
    }
}
