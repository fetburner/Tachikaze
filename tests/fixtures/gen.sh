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
ffmpeg -y \
  -f lavfi -i "testsrc2=size=640x360:rate=30000/1001" \
  -f lavfi -i "sine=frequency=440:sample_rate=48000" \
  -t 20 -shortest \
  -c:v libx264 -pix_fmt yuv420p \
  -g 120 -keyint_min 120 -sc_threshold 0 -bf 2 -x264-params open-gop=0 \
  -c:a libopus -b:a 96k \
  -use_editlist 0 \
  sample.mp4
