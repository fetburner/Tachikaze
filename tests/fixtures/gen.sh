#!/bin/bash
# テスト用フィクスチャ mp4 (tests/fixtures/sample.mp4) を生成するスクリプト。
#
# 実際の録画ファイルはリポジトリに入れられないため、対象素材と同じ性質
# (H.264 + Opus, フレームレート 30000/1001, GOP 120フレーム固定・シーンチェンジ
# 由来のIDRなし・オープンGOPなし) を持つ小さな mp4 を ffmpeg で合成する。
#
# 使い方: bash tests/fixtures/gen.sh
set -euo pipefail
cd "$(dirname "$0")"

# -use_editlist 0: ffmpeg は既定で Opus のプリスキップ分を edit list (elst) で
# 補正するが、対象の実素材には elst が無い想定（mp4io::support が elst を
# 未検証として拒否する）。付けないとこのフィクスチャ自身が拒否されてしまう。
#
# 音声は「一定周波数のサイン波」ではなく周波数スイープ（時間とともに周波数が
# 変化する信号）にしている。一定周波数だと Opus パケットの中身がほぼ全て
# 同一バイト列になり、「音声パケットをソース上のどの位置から取ったか」という
# 区間選択のバグ（src/audio.rs::select_audio_segments、docs/lossless-cut.md
# 「実際に起きた誤り」参照）を原理的に検出できない。周波数スイープならパケット
# ごとに中身が変わるため、CRC32 比較で位置ずれを検出できる。
ffmpeg -y \
  -f lavfi -i "testsrc2=size=640x360:rate=30000/1001" \
  -f lavfi -i "aevalsrc=0.5*sin(2*PI*(200+400*t)*t):s=48000" \
  -t 20 -shortest \
  -c:v libx264 -pix_fmt yuv420p \
  -g 120 -keyint_min 120 -sc_threshold 0 -bf 2 -x264-params open-gop=0 \
  -c:a libopus -b:a 96k \
  -use_editlist 0 \
  sample.mp4
