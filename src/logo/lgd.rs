//! Amatsukaze が作るロゴデータ `.lgd` のパーサ。
//!
//! ## フォーマット
//!
//! `.lgd` は **AviUtl 互換のベース部**の後ろに **Amatsukaze 独自の float 部**が
//! 続く二層構造のバイナリ。検出に使うのは float 部の画素ごとの係数 `aY`/`bY` だが、
//! ベース部を正しく読み飛ばさないと float 部の開始位置がずれる。
//!
//! すべてリトルエンディアン。例外は `logonum` 1 個だけ（ビッグエンディアン）。
//! 仕様は Amatsukaze `AMTLogo.hpp` の `WriteBaseLogo` / `WriteExtendedLogo` / `Load`
//! から確定した（ヘッダファイルは写さず、以下の表に落とし込んだもの）。
//!
//! ```text
//! [ベース部]
//!   0   char  str[28]      "<logo data file ver0.1>" + \0 埋め
//!   28  u32   logonum      ロゴ数。**ビッグエンディアン**（元コードが SWAP_ENDIAN している）
//!   32  LOGO_HEADER        char name[32], i16 x, y, h, w, fi, fo, st, ed  → 32 + 16 = 48 バイト
//!   80  LOGO_PIXEL[h*w]    i16 dp_y, y, dp_cb, cb, dp_cr, cr             → 1 画素 12 バイト
//! [Amatsukaze 独自部]
//!       LogoHeader         下記（540 バイト）
//!       f32 * (w*h + wUV*hUV*2) * 2   （aY, bY, aU, bU, aV, bV の順）
//! ```
//!
//! `LogoHeader` のレイアウト（C の構造体アライメントを含む。`name[255]` の直後に
//! **1 バイトのパディング**が入る）:
//!
//! | offset | 型 | 名前 |
//! |---|---|---|
//! | 0 | i32 | `magic`（`0x12345`） |
//! | 4 | i32 | `version`（1） |
//! | 8, 12 | i32 | `w`, `h` |
//! | 16, 20 | i32 | `logUVx`, `logUVy` |
//! | 24, 28 | i32 | `imgw`, `imgh` |
//! | 32, 36 | i32 | `imgx`, `imgy` |
//! | 40 | char[255] | `name` |
//! | 295 | — | パディング 1 バイト |
//! | 296 | i32 | `serviceId` |
//! | 300 | i32[60] | `reserved` |
//! | 540 | | 合計サイズ |
//!
//! float 部の並びは `aY, bY, aU, bU, aV, bV`。`wUV = w >> logUVx`、
//! `hUV = h >> logUVy`。`aY`/`bY` は `w*h` 要素、`aU`/`bU`/`aV`/`bV` は
//! `wUV*hUV` 要素。意味は `background = a * observed + b * maxv` という
//! アルファ合成の逆算式（`maxv` は 8bit なら 255）。検出で使うのは `aY`/`bY` だけだが、
//! `make-logo`（書き込み側、別 issue）が必要とするため全平面を読む。
//!
//! ## 罠
//!
//! - **ベース部のスキップ量はベース部の `LOGO_HEADER.w`/`h` から計算する**（`h*w*12`
//!   バイト）。Amatsukaze 独自部の `LogoHeader.w`/`h` ではない。通常は同じ値だが、
//!   食い違うファイルを読んだときに黙って別のオフセットを読み始めるのを避けるため、
//!   食い違っていたら [`LgdParseError::BaseExtendedSizeMismatch`] にする。
//! - `name[255]` の後のパディング 1 バイトを忘れると `serviceId` 以降が 1 バイトずれて
//!   読める。`magic` は先頭にあるため**このずれは `magic` の検査を素通りする**。
//!   [`LOGO_HEADER_LEN`] が 540 バイトであることをテストで assert している。
//! - `logonum` だけビッグエンディアン。他の整数はリトルエンディアン。
//! - `logUVx`/`logUVy` は「クロマの間引きの log2」。`>> log_uv` を書き間違えると
//!   float 部の要素数がずれて末尾を読み損なう（[`checked_shr`](u32::checked_shr) で
//!   シフト量が 0..32 の範囲外ならエラーにし、パニックにはしない）。

use std::fs;
use std::path::Path;

use crate::errctx::PathContext as _;

/// ベース部の `str[28]` のバイト数。
const BASE_STR_LEN: usize = 28;
/// ベース部の `logonum`（u32）のバイト数。
const BASE_LOGONUM_LEN: usize = 4;
/// ベース部の `LOGO_HEADER`（`name[32]` + `i16` x8）のバイト数。
const BASE_LOGO_HEADER_LEN: usize = 32 + 2 * 8;
/// ベース部の `LOGO_PIXEL` 1 画素あたりのバイト数（`i16` x6）。
const BASE_LOGO_PIXEL_SIZE: usize = 2 * 6;
/// ベース部の固定長部分（`str` + `logonum` + `LOGO_HEADER`）の合計バイト数。
/// この直後にベース部の可変長ピクセルデータが続く。
const BASE_FIXED_LEN: usize = BASE_STR_LEN + BASE_LOGONUM_LEN + BASE_LOGO_HEADER_LEN;

/// Amatsukaze 独自部 `LogoHeader` の合計バイト数（`name[255]` 直後の
/// パディング 1 バイトを含む）。
pub const LOGO_HEADER_LEN: usize = 540;
/// `LogoHeader.magic` に入っているべき値。
const MAGIC: u32 = 0x0001_2345;
/// `LogoHeader.name` のバイト数（固定長バッファ、末尾 NUL 終端）。
const NAME_LEN: usize = 255;

/// `.lgd` を読み込んだ結果。Amatsukaze 独自部（`LogoHeader` + float 部）の内容のみを
/// 保持する。ベース部（AviUtl 互換部）はスキップ量の検証にしか使わないため保持しない。
#[derive(Debug, Clone, PartialEq)]
pub struct LogoData {
    /// ロゴの幅（Y 平面）。
    pub w: i32,
    /// ロゴの高さ（Y 平面）。
    pub h: i32,
    /// クロマ間引きの log2（横方向）。`wUV = w >> log_uv_x`。
    pub log_uv_x: i32,
    /// クロマ間引きの log2（縦方向）。`hUV = h >> log_uv_y`。
    pub log_uv_y: i32,
    /// ロゴを検出した元画像の幅。
    pub imgw: i32,
    /// ロゴを検出した元画像の高さ。
    pub imgh: i32,
    /// ロゴを検出した元画像上での x 位置。
    pub imgx: i32,
    /// ロゴを検出した元画像上での y 位置。
    pub imgy: i32,
    /// ロゴの名前（`name[255]`、NUL 終端までを UTF-8 として解釈）。
    pub name: String,
    /// 対象サービス ID。
    pub service_id: i32,
    /// Y 平面の係数 `a`（`w*h` 要素、行優先）。
    pub a_y: Vec<f32>,
    /// Y 平面の係数 `b`（`w*h` 要素、行優先）。
    pub b_y: Vec<f32>,
    /// Cb 平面の係数 `a`（`wUV*hUV` 要素、行優先）。
    pub a_u: Vec<f32>,
    /// Cb 平面の係数 `b`（`wUV*hUV` 要素、行優先）。
    pub b_u: Vec<f32>,
    /// Cr 平面の係数 `a`（`wUV*hUV` 要素、行優先）。
    pub a_v: Vec<f32>,
    /// Cr 平面の係数 `b`（`wUV*hUV` 要素、行優先）。
    pub b_v: Vec<f32>,
}

/// `.lgd` のパースに失敗したことを表すエラー。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LgdParseError {
    /// ベース部の固定長部分（`str[28]` + `logonum` + `LOGO_HEADER`、80 バイト）に
    /// 届かないほどファイルが短い。
    TooShortForBaseHeader { actual: usize },
    /// ベース部の `LOGO_HEADER.w`/`h` から計算したピクセルデータの末尾まで
    /// ファイルが届かない。
    TooShortForBasePixels { needed: usize, actual: usize },
    /// Amatsukaze 独自部の `LogoHeader`（540 バイト）に届かないほどファイルが短い。
    TooShortForExtendedHeader { needed: usize, actual: usize },
    /// `LogoHeader.magic` が `0x12345` と一致しない。
    MagicMismatch { found: u32 },
    /// ベース部の `LOGO_HEADER.w`/`h` と Amatsukaze 独自部の `LogoHeader.w`/`h` が
    /// 食い違う。
    BaseExtendedSizeMismatch {
        base_w: i16,
        base_h: i16,
        ext_w: i32,
        ext_h: i32,
    },
    /// `LogoHeader.w` または `h` が 0 以下。
    NonPositiveSize { w: i32, h: i32 },
    /// `logUVx`/`logUVy` が `0..32` の範囲外で、`>>` によるクロマ平面サイズの
    /// 計算ができない。
    InvalidChromaShift { log_uv_x: i32, log_uv_y: i32 },
    /// float 部（`aY`/`bY`/`aU`/`bU`/`aV`/`bV`）の要素数がファイル終端までに届かない。
    TooShortForFloatPlanes { needed: usize, actual: usize },
}

impl std::fmt::Display for LgdParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LgdParseError::TooShortForBaseHeader { actual } => write!(
                f,
                ".lgd のベース部固定長ヘッダ（{BASE_FIXED_LEN} バイト）に届きません（実際 {actual} バイト）"
            ),
            LgdParseError::TooShortForBasePixels { needed, actual } => write!(
                f,
                ".lgd のベース部ピクセルデータの末尾（{needed} バイト目）に届きません（実際 {actual} バイト）"
            ),
            LgdParseError::TooShortForExtendedHeader { needed, actual } => write!(
                f,
                ".lgd の Amatsukaze 独自部 LogoHeader（{needed} バイト目まで、{LOGO_HEADER_LEN} バイト分）に届きません（実際 {actual} バイト）"
            ),
            LgdParseError::MagicMismatch { found } => write!(
                f,
                ".lgd の LogoHeader.magic が {MAGIC:#x} と一致しません（実際 {found:#x}）"
            ),
            LgdParseError::BaseExtendedSizeMismatch {
                base_w,
                base_h,
                ext_w,
                ext_h,
            } => write!(
                f,
                ".lgd のベース部 LOGO_HEADER の w/h（{base_w}x{base_h}）と \
                 Amatsukaze 独自部 LogoHeader の w/h（{ext_w}x{ext_h}）が食い違います"
            ),
            LgdParseError::NonPositiveSize { w, h } => write!(
                f,
                ".lgd の LogoHeader.w/h が 0 以下です（w={w}, h={h}）"
            ),
            LgdParseError::InvalidChromaShift {
                log_uv_x,
                log_uv_y,
            } => write!(
                f,
                ".lgd の LogoHeader.logUVx/logUVy が不正です（log_uv_x={log_uv_x}, log_uv_y={log_uv_y}, 0..32 の範囲が必要）"
            ),
            LgdParseError::TooShortForFloatPlanes { needed, actual } => write!(
                f,
                ".lgd の float 部（aY/bY/aU/bU/aV/bV）の末尾（{needed} バイト目）に届きません（実際 {actual} バイト）"
            ),
        }
    }
}

impl std::error::Error for LgdParseError {}

/// バイト列 `bytes` から `[offset, offset+4)` を `i32`（リトルエンディアン）として読む。
///
/// 呼び出し前に `bytes.len() >= offset + 4` を確認済みであることを前提にする
/// （このモジュール内では境界チェックをすべて済ませた後にしか呼ばない）。
fn read_i32_le(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

/// `read_i32_le` の `i16` 版。
fn read_i16_le(bytes: &[u8], offset: usize) -> i16 {
    i16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

/// `[offset, offset+count*4)` を `f32`（リトルエンディアン）`count` 個として読む。
fn read_f32_plane(bytes: &[u8], offset: usize, count: usize) -> Vec<f32> {
    // `count * 4` バイトちょうどのスライスなので余り（2番目の戻り値）は常に空。
    let (chunks, _remainder) = bytes[offset..offset + count * 4].as_chunks::<4>();
    chunks.iter().map(|c| f32::from_le_bytes(*c)).collect()
}

/// バイト列から `.lgd` の内容をパースする（ファイル I/O を含まない純粋な関数）。
///
/// ベース部を読み飛ばして Amatsukaze 独自部だけを [`LogoData`] として返す。
/// 壊れた入力（`magic` 不一致、ファイルが短い、`w`/`h` が 0 以下、ベース部と
/// 独自部で `w`/`h` が食い違う、float 部の要素数不足）はすべてエラーにする。
pub fn parse(bytes: &[u8]) -> Result<LogoData, LgdParseError> {
    if bytes.len() < BASE_FIXED_LEN {
        return Err(LgdParseError::TooShortForBaseHeader {
            actual: bytes.len(),
        });
    }

    // ベース部 LOGO_HEADER: str[28] + logonum(4, BE, 未使用) の後、
    // name[32] + i16 x8 (x, y, h, w, fi, fo, st, ed)。
    let logo_header_offset = BASE_STR_LEN + BASE_LOGONUM_LEN;
    let name_len = 32usize;
    let base_h = read_i16_le(bytes, logo_header_offset + name_len + 2 * 2);
    let base_w = read_i16_le(bytes, logo_header_offset + name_len + 2 * 3);

    // ベース部のピクセルデータのバイト数は、ベース部自身の LOGO_HEADER.w/h から
    // 計算する（Amatsukaze 独自部の LogoHeader.w/h は使わない。罠を参照）。
    let base_pixel_bytes: usize = i64::from(base_w)
        .checked_mul(i64::from(base_h))
        .and_then(|count| count.checked_mul(BASE_LOGO_PIXEL_SIZE as i64))
        .and_then(|n| usize::try_from(n).ok())
        .ok_or(LgdParseError::TooShortForBasePixels {
            needed: BASE_FIXED_LEN,
            actual: bytes.len(),
        })?;

    let ext_header_offset = BASE_FIXED_LEN + base_pixel_bytes;
    if bytes.len() < ext_header_offset {
        return Err(LgdParseError::TooShortForBasePixels {
            needed: ext_header_offset,
            actual: bytes.len(),
        });
    }

    let ext_header_end = ext_header_offset + LOGO_HEADER_LEN;
    if bytes.len() < ext_header_end {
        return Err(LgdParseError::TooShortForExtendedHeader {
            needed: ext_header_end,
            actual: bytes.len(),
        });
    }

    let magic = u32::from_le_bytes(
        bytes[ext_header_offset..ext_header_offset + 4]
            .try_into()
            .unwrap(),
    );
    if magic != MAGIC {
        return Err(LgdParseError::MagicMismatch { found: magic });
    }

    // +4: version (1) は現時点では検証・保持しない。
    let w = read_i32_le(bytes, ext_header_offset + 8);
    let h = read_i32_le(bytes, ext_header_offset + 12);
    let log_uv_x = read_i32_le(bytes, ext_header_offset + 16);
    let log_uv_y = read_i32_le(bytes, ext_header_offset + 20);
    let imgw = read_i32_le(bytes, ext_header_offset + 24);
    let imgh = read_i32_le(bytes, ext_header_offset + 28);
    let imgx = read_i32_le(bytes, ext_header_offset + 32);
    let imgy = read_i32_le(bytes, ext_header_offset + 36);

    let name_bytes = &bytes[ext_header_offset + 40..ext_header_offset + 40 + NAME_LEN];
    let name_nul = name_bytes
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(name_bytes.len());
    let name = String::from_utf8_lossy(&name_bytes[..name_nul]).into_owned();

    // +295: パディング1バイト（罠を参照。読み飛ばすだけで値は使わない）。
    let service_id = read_i32_le(bytes, ext_header_offset + 296);
    // +300: reserved[60] (240 バイト) は読み飛ばす。

    if w <= 0 || h <= 0 {
        return Err(LgdParseError::NonPositiveSize { w, h });
    }
    if i32::from(base_w) != w || i32::from(base_h) != h {
        return Err(LgdParseError::BaseExtendedSizeMismatch {
            base_w,
            base_h,
            ext_w: w,
            ext_h: h,
        });
    }

    // シフト量が 0..32 の範囲外なら checked_shr が None を返す。
    // 負値も u32 キャストで 32 以上になるためここで捕まる（パニックにしない）。
    let wuv = (w as u32)
        .checked_shr(log_uv_x as u32)
        .ok_or(LgdParseError::InvalidChromaShift { log_uv_x, log_uv_y })? as usize;
    let huv = (h as u32)
        .checked_shr(log_uv_y as u32)
        .ok_or(LgdParseError::InvalidChromaShift { log_uv_x, log_uv_y })? as usize;

    let y_count = w as usize * h as usize;
    let uv_count = wuv * huv;
    let floats_needed = 2 * y_count + 4 * uv_count;
    let float_start = ext_header_end;
    let float_end = float_start + floats_needed * 4;
    if bytes.len() < float_end {
        return Err(LgdParseError::TooShortForFloatPlanes {
            needed: float_end,
            actual: bytes.len(),
        });
    }

    let a_y = read_f32_plane(bytes, float_start, y_count);
    let b_y = read_f32_plane(bytes, float_start + y_count * 4, y_count);
    let uv_start = float_start + 2 * y_count * 4;
    let a_u = read_f32_plane(bytes, uv_start, uv_count);
    let b_u = read_f32_plane(bytes, uv_start + uv_count * 4, uv_count);
    let a_v = read_f32_plane(bytes, uv_start + 2 * uv_count * 4, uv_count);
    let b_v = read_f32_plane(bytes, uv_start + 3 * uv_count * 4, uv_count);

    Ok(LogoData {
        w,
        h,
        log_uv_x,
        log_uv_y,
        imgw,
        imgh,
        imgx,
        imgy,
        name,
        service_id,
        a_y,
        b_y,
        a_u,
        b_u,
        a_v,
        b_v,
    })
}

/// パスから `.lgd` を読み込む。I/O エラーは `path_ctx` でパス付きの文脈を付ける。
pub fn read<P: AsRef<Path>>(path: P) -> anyhow::Result<LogoData> {
    let path = path.as_ref();
    let bytes = fs::read(path).path_ctx(".lgd の読み込み", path)?;
    parse(&bytes).path_ctx(".lgd のパース", path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// テスト用に `.lgd` の全バイト列を組み立てるビルダ。
    ///
    /// ベース部はゼロ埋めしつつ、スキップ計算に効く `LOGO_HEADER.w`/`h` だけ
    /// 指定できるようにする。
    struct LgdBuilder {
        base_w: i16,
        base_h: i16,
        magic: u32,
        version: i32,
        w: i32,
        h: i32,
        log_uv_x: i32,
        log_uv_y: i32,
        imgw: i32,
        imgh: i32,
        imgx: i32,
        imgy: i32,
        name: &'static str,
        service_id: i32,
        a_y: Vec<f32>,
        b_y: Vec<f32>,
        a_u: Vec<f32>,
        b_u: Vec<f32>,
        a_v: Vec<f32>,
        b_v: Vec<f32>,
    }

    impl LgdBuilder {
        /// 2x2 (chroma 1x1、4:2:0 相当) の最小構成。
        fn new_2x2() -> Self {
            LgdBuilder {
                base_w: 2,
                base_h: 2,
                magic: MAGIC,
                version: 1,
                w: 2,
                h: 2,
                log_uv_x: 1,
                log_uv_y: 1,
                imgw: 1920,
                imgh: 1080,
                imgx: 10,
                imgy: 20,
                name: "test-logo",
                service_id: 1024,
                a_y: vec![1.0, 2.0, 3.0, 4.0],
                b_y: vec![5.0, 6.0, 7.0, 8.0],
                a_u: vec![9.0],
                b_u: vec![10.0],
                a_v: vec![11.0],
                b_v: vec![12.0],
            }
        }

        /// ベース部（`str[28]` + `logonum` + `LOGO_HEADER`）だけを組み立てる。
        /// ピクセルデータはゼロ埋め（内容は検証しないため）。
        fn build_base(&self) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.extend_from_slice(&[0u8; BASE_STR_LEN]);
            buf.extend_from_slice(&1u32.to_be_bytes()); // logonum (BE)
            buf.extend_from_slice(&[0u8; 32]); // name[32]
            buf.extend_from_slice(&0i16.to_le_bytes()); // x
            buf.extend_from_slice(&0i16.to_le_bytes()); // y
            buf.extend_from_slice(&self.base_h.to_le_bytes()); // h
            buf.extend_from_slice(&self.base_w.to_le_bytes()); // w
            buf.extend_from_slice(&0i16.to_le_bytes()); // fi
            buf.extend_from_slice(&0i16.to_le_bytes()); // fo
            buf.extend_from_slice(&0i16.to_le_bytes()); // st
            buf.extend_from_slice(&0i16.to_le_bytes()); // ed
            assert_eq!(buf.len(), BASE_FIXED_LEN);

            let pixel_count = (self.base_w.max(0) as usize) * (self.base_h.max(0) as usize);
            buf.extend_from_slice(&vec![0u8; pixel_count * BASE_LOGO_PIXEL_SIZE]);
            buf
        }

        /// Amatsukaze 独自部（`LogoHeader` + float 部）だけを組み立てる。
        fn build_extended(&self) -> Vec<u8> {
            let mut buf = Vec::new();
            buf.extend_from_slice(&self.magic.to_le_bytes());
            buf.extend_from_slice(&self.version.to_le_bytes());
            buf.extend_from_slice(&self.w.to_le_bytes());
            buf.extend_from_slice(&self.h.to_le_bytes());
            buf.extend_from_slice(&self.log_uv_x.to_le_bytes());
            buf.extend_from_slice(&self.log_uv_y.to_le_bytes());
            buf.extend_from_slice(&self.imgw.to_le_bytes());
            buf.extend_from_slice(&self.imgh.to_le_bytes());
            buf.extend_from_slice(&self.imgx.to_le_bytes());
            buf.extend_from_slice(&self.imgy.to_le_bytes());
            assert_eq!(buf.len(), 40);

            let mut name_field = [0u8; NAME_LEN];
            let name_bytes = self.name.as_bytes();
            name_field[..name_bytes.len()].copy_from_slice(name_bytes);
            buf.extend_from_slice(&name_field);
            buf.push(0); // padding 1 byte
            assert_eq!(buf.len(), 296);

            buf.extend_from_slice(&self.service_id.to_le_bytes());
            buf.extend_from_slice(&[0u8; 240]); // reserved[60]
            assert_eq!(buf.len(), LOGO_HEADER_LEN);

            for plane in [
                &self.a_y, &self.b_y, &self.a_u, &self.b_u, &self.a_v, &self.b_v,
            ] {
                for v in plane {
                    buf.extend_from_slice(&v.to_le_bytes());
                }
            }
            buf
        }

        fn build(&self) -> Vec<u8> {
            let mut buf = self.build_base();
            buf.extend_from_slice(&self.build_extended());
            buf
        }
    }

    #[test]
    fn logo_header_size_is_540_bytes() {
        // 罠: name[255] の直後のパディング1バイトを忘れると、この assert が失敗する
        // （40 + 255 + 4 + 240 = 539 になってしまう）。
        assert_eq!(LOGO_HEADER_LEN, 540);
    }

    #[test]
    fn parses_known_values_including_all_fields_and_planes() {
        let b = LgdBuilder::new_2x2();
        let bytes = b.build();

        let got = parse(&bytes).expect("既知の値から組み立てたバイト列はパースできるはず");

        assert_eq!(got.w, 2);
        assert_eq!(got.h, 2);
        assert_eq!(got.log_uv_x, 1);
        assert_eq!(got.log_uv_y, 1);
        assert_eq!(got.imgw, 1920);
        assert_eq!(got.imgh, 1080);
        assert_eq!(got.imgx, 10);
        assert_eq!(got.imgy, 20);
        assert_eq!(got.name, "test-logo");
        assert_eq!(got.service_id, 1024);
        assert_eq!(got.a_y, vec![1.0, 2.0, 3.0, 4.0]);
        assert_eq!(got.b_y, vec![5.0, 6.0, 7.0, 8.0]);
        assert_eq!(got.a_u, vec![9.0]);
        assert_eq!(got.b_u, vec![10.0]);
        assert_eq!(got.a_v, vec![11.0]);
        assert_eq!(got.b_v, vec![12.0]);
    }

    #[test]
    fn magic_mismatch_is_an_error() {
        let mut b = LgdBuilder::new_2x2();
        b.magic = 0xdead_beef;
        let bytes = b.build();

        assert_eq!(
            parse(&bytes),
            Err(LgdParseError::MagicMismatch { found: 0xdead_beef })
        );
    }

    #[test]
    fn truncated_file_in_base_header_is_an_error() {
        let bytes = vec![0u8; BASE_FIXED_LEN - 1];
        assert_eq!(
            parse(&bytes),
            Err(LgdParseError::TooShortForBaseHeader {
                actual: bytes.len()
            })
        );
    }

    #[test]
    fn truncated_file_in_base_pixels_is_an_error() {
        let b = LgdBuilder::new_2x2();
        let mut bytes = b.build_base();
        bytes.pop(); // ピクセルデータの末尾を1バイト削る。
        assert!(matches!(
            parse(&bytes),
            Err(LgdParseError::TooShortForBasePixels { .. })
        ));
    }

    #[test]
    fn truncated_file_in_extended_header_is_an_error() {
        let b = LgdBuilder::new_2x2();
        let mut bytes = b.build_base();
        let ext = b.build_extended();
        // LogoHeader の途中までしか無いファイル。
        bytes.extend_from_slice(&ext[..LOGO_HEADER_LEN - 1]);
        assert!(matches!(
            parse(&bytes),
            Err(LgdParseError::TooShortForExtendedHeader { .. })
        ));
    }

    #[test]
    fn truncated_file_in_float_planes_is_an_error() {
        let b = LgdBuilder::new_2x2();
        let mut bytes = b.build();
        bytes.pop(); // float 部の末尾を1バイト削る。
        assert!(matches!(
            parse(&bytes),
            Err(LgdParseError::TooShortForFloatPlanes { .. })
        ));
    }

    #[test]
    fn non_positive_size_is_an_error() {
        let mut b = LgdBuilder::new_2x2();
        b.w = 0;
        // ベース部の w/h も合わせておく(食い違いエラーより先に w<=0 を検出させるため)。
        b.base_w = 0;
        let bytes = b.build();

        assert_eq!(
            parse(&bytes),
            Err(LgdParseError::NonPositiveSize { w: 0, h: 2 })
        );
    }

    #[test]
    fn base_and_extended_size_mismatch_is_an_error() {
        let mut b = LgdBuilder::new_2x2();
        b.base_w = 3; // 独自部の w=2 と食い違わせる。
        let bytes = b.build();

        assert_eq!(
            parse(&bytes),
            Err(LgdParseError::BaseExtendedSizeMismatch {
                base_w: 3,
                base_h: 2,
                ext_w: 2,
                ext_h: 2,
            })
        );
    }

    #[test]
    fn invalid_chroma_shift_is_an_error_not_a_panic() {
        let mut b = LgdBuilder::new_2x2();
        b.log_uv_x = 32; // w=2 に対して 0..32 の範囲外。
        let bytes = b.build();

        assert_eq!(
            parse(&bytes),
            Err(LgdParseError::InvalidChromaShift {
                log_uv_x: 32,
                log_uv_y: 1,
            })
        );
    }

    #[test]
    fn negative_chroma_shift_is_an_error_not_a_panic() {
        let mut b = LgdBuilder::new_2x2();
        b.log_uv_y = -1;
        let bytes = b.build();

        assert_eq!(
            parse(&bytes),
            Err(LgdParseError::InvalidChromaShift {
                log_uv_x: 1,
                log_uv_y: -1,
            })
        );
    }

    #[test]
    fn read_reports_missing_file_with_path_context() {
        let err = read("/nonexistent/path/to/logo.lgd").unwrap_err();
        assert!(
            format!("{err:#}").contains(".lgd の読み込みに失敗しました"),
            "path_ctx の文言が付いているはず: {err:#}"
        );
    }

    #[test]
    fn read_reports_parse_failure_with_path_context() {
        let path = std::env::temp_dir().join(format!(
            "tachikaze-logo-lgd-parse-error-{}.lgd",
            std::process::id()
        ));
        fs::write(&path, [0u8; BASE_FIXED_LEN - 1]).expect("一時ファイルを書けるはず");

        let err = read(&path).unwrap_err();
        let _ = fs::remove_file(&path);

        let rendered = format!("{err:#}");
        assert!(
            rendered.contains(".lgd のパースに失敗しました"),
            "path_ctx の文言が付いているはず: {rendered}"
        );
        assert!(
            rendered.contains(&path.display().to_string()),
            "パースエラーにもパスが含まれるはず: {rendered}"
        );
    }
}
