// #26 / #27 で消費されるまで未使用。配線されたら外す。
#![allow(dead_code)]

//! mp4 の `moov` を取り出し、映像/音声トラックを識別する読み込み側。
//!
//! `mdat` は読み飛ばすため、ファイル全体をメモリに載せずに `moov` だけを取得できる
//! （検証済みコード: docs/mp4-atom.md「トップレベルから moov を取り出す」）。

use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::Path;

use mp4_atom::{Atom, Codec, Header, Moov, ReadAtom, ReadFrom, Trak};

/// トラックごとに異なりうる時間の基準単位。
///
/// 映像と音声で timescale は別々なので、トラックごとに保持する
/// （前提: 対象素材は H.264 + Opus で、両者の timescale は一致しない）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackInfo {
    pub timescale: u32,
}

/// mp4 ファイルパスから `moov` アトムを取り出す。
///
/// トップレベルのアトムを順に読み、`moov` に到達するまで他のアトム（特に巨大な
/// `mdat`）はシークして読み飛ばす。`Header::size` が `None`（サイズ 0 = ファイル
/// 末尾まで）のアトムは、その時点で `moov` が見つかっていないことを意味するため
/// エラーとして扱う。
pub fn read_moov<P: AsRef<Path>>(path: P) -> std::result::Result<Moov, anyhow::Error> {
    let mut r = BufReader::new(File::open(path)?);

    loop {
        let header = Header::read_from(&mut r)?;

        if header.kind == Moov::KIND {
            return Ok(Moov::read_atom(&header, &mut r)?);
        }

        match header.size {
            Some(size) => {
                r.seek(SeekFrom::Current(size as i64))?;
            }
            None => {
                // サイズ 0 は「ファイル末尾まで」を意味する。moov に到達する前に
                // このようなアトム（通常は末尾の mdat）に当たった場合、moov は
                // このファイルに存在しない。
                anyhow::bail!(
                    "moov atom not found before size-to-eof atom '{}'",
                    header.kind
                );
            }
        }
    }
}

/// トラックの `stsd` に入っている先頭のサンプルエントリから `Codec` を取り出す。
///
/// 対象素材の `stsd` は 1 トラックにつき 1 エントリのみを想定している。
fn track_codec(trak: &Trak) -> Option<&Codec> {
    trak.mdia.minf.stbl.stsd.codecs.first()
}

/// トラックの `mdhd` から timescale を取り出す。
fn track_info(trak: &Trak) -> TrackInfo {
    TrackInfo {
        timescale: trak.mdia.mdhd.timescale,
    }
}

/// `moov` から映像トラック（`Codec::Avc1`）を 1 本だけ見つける。
///
/// 複数本存在する場合は最初に見つかったものを返す（対象素材は映像 1 本を想定）。
pub fn find_video_track(moov: &Moov) -> Option<(&Trak, TrackInfo)> {
    moov.trak.iter().find_map(|trak| match track_codec(trak) {
        Some(Codec::Avc1(_)) => Some((trak, track_info(trak))),
        _ => None,
    })
}

/// `moov` から音声トラック（`Codec::Opus`）を 1 本だけ見つける。
///
/// 複数本存在する場合は最初に見つかったものを返す（対象素材は音声 1 本を想定）。
pub fn find_audio_track(moov: &Moov) -> Option<(&Trak, TrackInfo)> {
    moov.trak.iter().find_map(|trak| match track_codec(trak) {
        Some(Codec::Opus(_)) => Some((trak, track_info(trak))),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// フィクスチャ: H.264 (Avc1) + Opus, GOP 120, 30000/1001fps の mp4。
    /// 生成手順は issue #15 で整備される想定（tests/fixtures/sample.mp4）。
    ///
    /// 実行環境にフィクスチャが無いため #[ignore] にしている。手元では以下の
    /// ffmpeg コマンドで生成した一時ファイルに対して実行し、通ることを確認済み:
    ///
    /// ```sh
    /// ffmpeg -y -f lavfi -i "testsrc=size=320x240:rate=30000/1001:duration=5" \
    ///   -f lavfi -i "sine=frequency=1000:duration=5" \
    ///   -c:v libx264 -pix_fmt yuv420p -g 120 -keyint_min 120 -sc_threshold 0 \
    ///   -c:a libopus \
    ///   tests/fixtures/sample.mp4
    /// ```
    ///
    /// フィクスチャが揃ったら #[ignore] を外す。
    #[test]
    #[ignore = "tests/fixtures/sample.mp4 が未整備（issue #15 待ち）"]
    fn finds_video_and_audio_tracks_with_distinct_timescales() {
        let moov = read_moov("tests/fixtures/sample.mp4").expect("moov を読めること");

        let (_video_trak, video_info) =
            find_video_track(&moov).expect("映像トラックが 1 本見つかること");
        let (_audio_trak, audio_info) =
            find_audio_track(&moov).expect("音声トラックが 1 本見つかること");

        assert!(video_info.timescale > 0);
        assert!(audio_info.timescale > 0);
        // 映像と音声で timescale は異なる(前提: CLAUDE.md)。
        assert_ne!(video_info.timescale, audio_info.timescale);
    }

    #[test]
    #[ignore = "tests/fixtures/sample.mp4 が未整備（issue #15 待ち）"]
    fn codec_kinds_match_expected_material() {
        let moov = read_moov("tests/fixtures/sample.mp4").expect("moov を読めること");

        let (video_trak, _) = find_video_track(&moov).expect("映像トラックが見つかること");
        let (audio_trak, _) = find_audio_track(&moov).expect("音声トラックが見つかること");

        assert!(matches!(track_codec(video_trak), Some(Codec::Avc1(_))));
        assert!(matches!(track_codec(audio_trak), Some(Codec::Opus(_))));
    }

    #[test]
    fn find_video_track_returns_none_on_empty_moov() {
        let moov = Moov::default();
        assert!(find_video_track(&moov).is_none());
        assert!(find_audio_track(&moov).is_none());
    }
}
