#!/bin/bash
# テスト用フィクスチャ mp4 を生成するスクリプト。
#
# 実際の録画ファイルはリポジトリに入れられないため、対象素材と同じ映像条件
# (H.264, フレームレート 30000/1001, GOP 120フレーム固定・シーンチェンジ由来の
# IDRなし・オープンGOPなし)を持つ小さな mp4 を、音声コーデック別に合成する。
#
# 生成物:
#   sample.mp4           H.264 + Opus（既存 E2E 用、elst なし）
#   sample_aac.mp4       H.264 + AAC / Mp4a（#42 非 Opus E2E 用、elst なし）
#   sample_flac.mp4      H.264 + FLAC（#47 追加 Codec smoke E2E 用、elst なし）
#   sample_elst.mp4      H.264 + Opus（#60 elst 実測用。ffmpeg 既定で elst あり）
#   sample_elst_aac.mp4  H.264 + AAC（#60 elst 実測用。ffmpeg 既定で elst あり）
#   sample_logo.mp4       H.264 + Opus（#97 analyze --logo E2E 用。半透明の枠を
#                          「本編」区間だけ合成した検出対象）
#   sample_logo_train.mp4 H.264 + Opus（#97 同上。枠を常時合成した学習専用クリップ。
#                          make-logo で .lgd を作るのに使う）
#
# 使い方: bash tests/fixtures/gen.sh
set -euo pipefail
cd "$(dirname "$0")"

# -use_editlist 0: ffmpeg は既定で音声の priming / プリスキップ分を edit list
# (elst)で補正することがあるが、対象の実素材には elst が無い想定
# （mp4io::support が elst を未検証として拒否する）。付けないとこの
# フィクスチャ自身が拒否されてしまう。
#
# sample_elst*.mp4 は逆に -use_editlist 0 を付けず、ffmpeg 既定の elst 付与
# 挙動をそのまま使う（#60: elst 除去が A/V の相対時刻を保つかを実測するための
# フィクスチャ。実測結果は docs/measurements.md「elst 除去と A/V 相対時刻」）。
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
  local editlist_flag=$3
  shift 3

  local editlist_args=()
  if [ "$editlist_flag" = "no-elst" ]; then
    editlist_args=(-use_editlist 0)
  fi

  ffmpeg -y \
    -f lavfi -i "testsrc2=size=640x360:rate=30000/1001" \
    -f lavfi -i "aevalsrc=0.5*sin(2*PI*(200+400*t)*t):s=48000" \
    -t 20 -shortest \
    -c:v libx264 -pix_fmt yuv420p \
    -g 120 -keyint_min 120 -sc_threshold 0 -bf 2 -x264-params open-gop=0 \
    -c:a "$audio_codec" "$@" \
    "${editlist_args[@]}" \
    "$output"
}

generate_fixture sample.mp4 libopus no-elst -b:a 96k
generate_fixture sample_aac.mp4 aac no-elst -b:a 128k
generate_fixture sample_flac.mp4 flac no-elst
generate_fixture sample_elst.mp4 libopus keep-elst -b:a 96k
generate_fixture sample_elst_aac.mp4 aac keep-elst -b:a 128k

# sample_logo.mp4 / sample_logo_train.mp4: analyze --logo の E2E
# （#97、tests/analyze_logo_e2e.rs）専用。sample.mp4 系とは映像条件を独立させて
# よい（issue #97: 「既存の sample.mp4 系の映像パラメータは変えない」だけが制約で、
# 新規フィクスチャ自体の解像度・背景は自由）。
#
# ## 疑似ロゴの形（枠、`t=fill` ではなく `t=<厚み>`）
#
# `drawbox` を塗り潰し（`t=fill`）にすると矩形内部が全画素同一のアルファになり、
# 空間方向の構造（エッジ）が一切無い。ロゴの相関スコア（`src/logo/score.rs`）は
# 局所平均を引いた5x5窓の相関＝**エッジ構造**を見るため、構造の無い塗り潰しでは
# `corr0` が実測でほぼ0のまま検出できなかった（このフィクスチャの作成時に実測）。
# 枠（輪郭だけ描く `t=<厚み>`）にすると枠の縁と内側で alpha が変わり、実在の
# ロゴに近い構造ができる。
#
# ## 背景の明るさを時間変化させる理由（`eq=brightness=...:eval=frame`）
#
# testsrc2 は場所によって時間変化がほぼ無い領域があり、そこにロゴ矩形を置くと
# 学習（`make-logo`）の最小二乗回帰が分散0で発散する（実測で `a=inf, b=-inf` の
# `InvalidCoefficient` になった）。`eq` フィルタで全画面の明るさを sin 波で
# 常時変化させることで、矩形内のどの画素でも時間方向の分散が確保され、回帰が
# 安定する。
#
# ## 学習用クリップと検出対象クリップを分ける理由
#
# ロゴを「本編」区間だけに合成すると、本編とCMでは alpha が 0 と 0.6 の2値に
# なる。`sample_logo.mp4`（検出対象）をそのまま `make-logo` に渡すと、2つの
# alpha が混在した分布に単一の回帰直線を当てるため学習結果が本編側の実際の
# 合成関係を表さず、検出（`corr0`/`corr1`）が実測で常に閾値未満になった。
# ロゴが常時合成された `sample_logo_train.mp4` で学習し、その `.lgd` を
# `sample_logo.mp4` の検出に使うことで、実際の運用（本編で学習し、CM を含む
# 全体に適用する）に近い形にする。
#
# ロゴ矩形は `616,4,16,16`、枠の厚みは3px、alpha は 0.6（`color=white@0.6`）。
# `sample_logo.mp4` は矩形を「本編」区間（0〜8秒・13〜20秒、8〜13秒がCM相当）
# だけに合成し、`sample_logo_train.mp4` は常時合成する。
logo_video_filter() {
  local enable_clause=$1
  echo "eq=brightness='0.15*sin(2*PI*t/3)':eval=frame,drawbox=x=616:y=4:w=16:h=16:color=white@0.6:t=3${enable_clause}"
}

ffmpeg -y \
  -f lavfi -i "testsrc2=size=640x360:rate=30000/1001" \
  -f lavfi -i "aevalsrc=0.5*sin(2*PI*(200+400*t)*t):s=48000" \
  -t 20 -shortest \
  -filter:v "$(logo_video_filter ":enable='between(t\,0\,8)+between(t\,13\,20)'")" \
  -c:v libx264 -pix_fmt yuv420p \
  -g 120 -keyint_min 120 -sc_threshold 0 -bf 2 -x264-params open-gop=0 \
  -c:a libopus -b:a 96k \
  -use_editlist 0 \
  sample_logo.mp4

ffmpeg -y \
  -f lavfi -i "testsrc2=size=640x360:rate=30000/1001" \
  -f lavfi -i "aevalsrc=0.5*sin(2*PI*(200+400*t)*t):s=48000" \
  -t 20 -shortest \
  -filter:v "$(logo_video_filter "")" \
  -c:v libx264 -pix_fmt yuv420p \
  -g 120 -keyint_min 120 -sc_threshold 0 -bf 2 -x264-params open-gop=0 \
  -c:a libopus -b:a 96k \
  -use_editlist 0 \
  sample_logo_train.mp4
