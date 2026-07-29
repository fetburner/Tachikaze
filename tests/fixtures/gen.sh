#!/bin/bash
# テスト用フィクスチャ mp4 を生成するスクリプト。
#
# 実際の録画ファイルはリポジトリに入れられないため、対象素材と同じ映像条件
# (H.264, フレームレート 30000/1001, GOP 120フレーム固定・シーンチェンジ由来の
# IDRなし・オープンGOPなし)を持つ小さな mp4 を、音声コーデック別に合成する。
#
# 生成物:
#   sample.mp4      H.264 + Opus（既存 E2E 用）
#   sample_aac.mp4  H.264 + AAC / Mp4a（#42 非 Opus E2E 用）
#
# 使い方: bash tests/fixtures/gen.sh
set -euo pipefail
cd "$(dirname "$0")"

# -use_editlist 0: ffmpeg は既定で音声の priming / プリスキップ分を edit list
# (elst)で補正することがあるが、対象の実素材には elst が無い想定
# （mp4io::support が elst を未検証として拒否する）。付けないとこの
# フィクスチャ自身が拒否されてしまう。
#
# 音声は「一定周波数のサイン波」ではなく周波数スイープ（時間とともに周波数が
# 変化する信号）にしている。一定周波数だとコーデックによってはパケットの中身が
# ほぼ同一バイト列になり、「音声パケットをソース上のどの位置から取ったか」という
# 区間選択のバグ（src/audio.rs::select_audio_segments、docs/lossless-cut.md
# 「実際に起きた誤り」参照）を原理的に検出できない。周波数スイープならパケット
# ごとに中身が変わるため、CRC32 比較で位置ずれを検出できる。
#
# 映像側のパラメータを変えると tests/data/sample.dtvi が使えなくなる。音声 Codec
# を増やす場合も、下の映像入力・x264 オプションは変更しないこと。
generate_fixture() {
  local output=$1
  local audio_codec=$2
  shift 2

  ffmpeg -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30000/1001" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*(200+400*t)*t):s=48000" \
    -t 20 -shortest \
    -c:v libx264 -pix_fmt yuv420p \
    -g 120 -keyint_min 120 -sc_threshold 0 -bf 2 -x264-params open-gop=0 \
    -c:a "$audio_codec" "$@" \
    -use_editlist 0 \
    "$output"
}

generate_fixture sample.mp4 libopus -b:a 96k
generate_fixture sample_aac.mp4 aac -b:a 128k
