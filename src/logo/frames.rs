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
//! 検査が無くても安全。**検出経路からは呼ばないこと。**

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

    match read_result {
        // `read_frames` がフレーム数不一致・端数バイトで失敗するのは、いずれも
        // reader が EOF に達した後（ffmpeg は既に終了しているはず）。`wait()` は
        // ブロックしないので安全に呼べる。ffmpeg 自体も異常終了していた場合は、
        // フレーム数不一致等より根本原因に近いのでそちらを優先する（そうしないと
        // 「壊れた入力で ffmpeg が落ちた」が常に「座標系がずれている」という
        // 無関係な誤誘導メッセージに隠れる）。
        Err(ReadFramesError::Protocol(protocol_err)) => match child.wait() {
            Err(wait_err) => Err(wait_err),
            Ok(()) => Err(protocol_err),
        },
        // `on_frame` コールバックがエラーを返して読み取りを中断した場合は EOF 前
        // で、ffmpeg がまだ書き込み中の可能性がある。そのまま `wait()` すると
        // パイプが詰まってデッドロックしうるため、先に `kill()` してから `wait()`
        // は結果を捨てて reap だけする（`kill` によるシグナル終了エラーが
        // `on_frame` 本来のエラーを隠してしまうため、`wait()` の結果は使わない）。
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

/// `ffmpeg` を起動し、`input` の**キーフレームだけ**をデコードして、クロップして
/// いない全画面の輝度平面を1フレームずつ `on_frame` に渡す。
///
/// ロゴ矩形を入力自身から推定する処理（別 issue）専用の関数。[`stream_luma_frames`]
/// との違いと、その違いにもかかわらずフレーム数の一致検査を省いて安全な理由は
/// モジュール doc comment「[`stream_keyframe_luma_frames`] を別関数にした理由」
/// 参照。**この関数は `.dtvi` のフレーム番号との対応を検査しないので、
/// `analyze --logo` の検出経路（`logo::score`）からは呼ばないこと。** 検出経路は
/// 引き続き `stream_luma_frames` を使う。
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
    let absolute_input =
        std::fs::canonicalize(input).path_ctx("入力ファイルの絶対パス解決", input)?;
    let frame_bytes = video_size.width as usize * video_size.height as usize;
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

    match read_result {
        // `read_frames` と同じ理由（EOF後の失敗なら `wait()` は安全、そちらの
        // エラーを優先する）で `wait()` の呼び方を分ける。
        Err(ReadFramesError::Protocol(protocol_err)) => match child.wait() {
            Err(wait_err) => Err(wait_err),
            Ok(()) => Err(protocol_err),
        },
        // `on_frame` が中断した場合は EOF 前の可能性があるため、先に `kill()` して
        // から `wait()` の結果は捨てる（`stream_luma_frames` と同じ扱い）。
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

/// [`read_frames`] の失敗要因。`stream_luma_frames` が `wait()` の呼び方を
/// 分けるために区別する（詳細は呼び出し側の doc comment参照）。
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
/// `on_frame` を呼ぶ。ffmpeg プロセスの起動から分離しているのは、ffmpeg を
/// 起動せずにこの読み取りロジック単体をテストするため。
///
/// - `frame_bytes` の倍数でない終わり方（端数バイト）はエラーにする。
/// - 読み終えた時点で `expected_frame_count` と実際のフレーム数が一致しなければ
///   エラーにする（モジュール doc comment「座標系がこの issue の本質」参照。
///   一致しないまま後続に進むと CM の位置が黙ってずれる）。
fn read_frames<R: Read>(
    mut reader: R,
    frame_bytes: usize,
    expected_frame_count: u64,
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

/// `reader` から輝度フレームを `frame_bytes` バイトずつ読み、フレームごとに
/// `on_frame` を呼ぶ（[`stream_keyframe_luma_frames`] 用）。
///
/// [`read_frames`] との違いは2点:
/// - `expected_frame_count` を取らない・フレーム数の一致検査をしない
///   （矩形推定はフレーム番号と `.dtvi` の対応を使わないため、モジュール
///   doc comment「[`stream_keyframe_luma_frames`] を別関数にした理由」参照）
/// - 代わりに**0フレームをエラーにする**（`-skip_frame nokey` の指定ミス等で
///   黙って0枚になると、後続の矩形推定が「ロゴ無し」と誤判定するだけで
///   気づけないため）
///
/// `frame_bytes` の倍数でない終わり方（端数バイト）をエラーにする点は
/// `read_frames` と同じ。
fn read_keyframe_frames<R: Read>(
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
                 いません: フレーム{frame_count}個目の途中、実際は{n}バイトで終わっています。"
            )));
        }
        frame_count += 1;
        on_frame(&buf).map_err(ReadFramesError::Callback)?;
    }
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
}
