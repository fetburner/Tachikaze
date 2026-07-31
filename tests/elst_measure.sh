#!/bin/bash
# elst 除去が A/V の相対時刻を保つかを実測するスクリプト（#60）。
#
# docs/architecture.md「未対応の入力」の elst の行が謳う回避策
#   ffmpeg -i IN.mp4 -c copy -use_editlist 0 -movflags +faststart OUT.mp4
# は「除去後の映像・音声パケットが CRC32 でビット一致する」ことは確認済みだが、
# それはペイロードのみの確認でタイムスタンプは見ていなかった。このスクリプトは
# tests/fixtures/gen.sh が生成する elst 付きフィクスチャ（sample_elst.mp4=Opus,
# sample_elst_aac.mp4=AAC）に対して、
#
#   1. 除去操作(上記コマンド)が生 pts/dts・パケット数を変えない可逆操作であること
#   2. elst の media_time / segment_duration の実値
#   3. 除去前後の「先頭の映像 pts - 先頭の音声 pts」の変化量
#   4. Opus(dOps の pre-skip)と AAC(elst 依存)で priming の伝わり方が違うこと
#
# を実測して表示する。実測結果と方針の記録は docs/measurements.md
# 「elst 除去と A/V 相対時刻」。CLAUDE.md 罠2(md5 でなく CRC32)・罠4(pts でなく
# dts で引き当てる)を踏まえた比較を行う。
#
# 使い方:
#   bash tests/fixtures/gen.sh   # 先にフィクスチャを生成しておく
#   bash tests/elst_measure.sh
#
# 前提: ffmpeg / ffprobe が PATH にあること。生成物は一時ディレクトリに書き、
# 終了時に削除する(tests/fixtures/ 配下のフィクスチャ自体は変更しない)。

set -euo pipefail
cd "$(dirname "$0")"

require_tools() {
    for bin in ffmpeg ffprobe; do
        if ! command -v "$bin" >/dev/null 2>&1; then
            echo "error: $bin が見つかりません。PATH を確認してください。" >&2
            exit 1
        fi
    done
}

video_crc32() {
    ffprobe -v error -select_streams v:0 \
        -show_entries packet=size,data_hash -show_data_hash CRC32 -of csv=p=0 "$1"
}

audio_crc32() {
    ffprobe -v error -select_streams a:0 \
        -show_entries packet=size,data_hash -show_data_hash CRC32 -of csv=p=0 "$1"
}

first_pts() {
    # select_streams, ファイルパス
    # ffprobe は先頭パケットの csv 出力にだけ末尾カンマを付けることがあるため取り除く
    # (side_data 列が先頭パケットのみ空エントリで出るffprobe側の癖。数値には影響しない)。
    ffprobe -v error -select_streams "$1" -show_entries packet=pts_time -of csv=p=0 "$2" \
        | tr -d ',' | sort -n | head -1
}

measure_one() {
    local label="$1"
    local elst_file="$2"
    local no_elst_reference="$3"
    local audio_codec_note="$4"

    echo "=================================================================="
    echo "### $label ($audio_codec_note)"
    echo "=================================================================="

    [ -f "$elst_file" ] || {
        echo "skip: $elst_file が無い(bash tests/fixtures/gen.sh を先に実行すること)" >&2
        return 0
    }

    echo "--- elst の実値 (ffmpeg -v trace) ---"
    ffmpeg -v trace -i "$elst_file" -f null - 2>&1 \
        | grep -E "Processing st: [01], edit list" || echo "(elst が見つかりません)"

    local stripped="$tmp_dir/$(basename "$elst_file" .mp4)_stripped.mp4"
    ffmpeg -y -v error -i "$elst_file" -c copy -use_editlist 0 -movflags +faststart "$stripped"

    echo "--- 除去後に elst が残っていないか ---"
    if ffmpeg -v trace -i "$stripped" -f null - 2>&1 | grep -q "Processing st:.*edit list"; then
        echo "NG: elst が残っています"
    else
        echo "OK: elst は除去されました"
    fi

    echo "--- パケット数(除去前 / 除去後) ---"
    local v_before v_after a_before a_after
    v_before="$(ffprobe -v error -select_streams v:0 -show_entries packet=pts -of csv=p=0 "$elst_file" | wc -l | tr -d ' ')"
    v_after="$(ffprobe -v error -select_streams v:0 -show_entries packet=pts -of csv=p=0 "$stripped" | wc -l | tr -d ' ')"
    a_before="$(ffprobe -v error -select_streams a:0 -show_entries packet=pts -of csv=p=0 "$elst_file" | wc -l | tr -d ' ')"
    a_after="$(ffprobe -v error -select_streams a:0 -show_entries packet=pts -of csv=p=0 "$stripped" | wc -l | tr -d ' ')"
    echo "映像: $v_before -> $v_after (差 $((v_after - v_before)))"
    echo "音声: $a_before -> $a_after (差 $((a_after - a_before)))"

    if [ -f "$no_elst_reference" ]; then
        echo "--- 除去後 vs 最初から -use_editlist 0 で符号化したフィクスチャ ($no_elst_reference) ---"
        if diff <(video_crc32 "$stripped") <(video_crc32 "$no_elst_reference") >/dev/null; then
            echo "OK: 映像パケット CRC32 完全一致"
        else
            echo "NG: 映像パケット CRC32 不一致"
        fi
        if diff <(audio_crc32 "$stripped") <(audio_crc32 "$no_elst_reference") >/dev/null; then
            echo "OK: 音声パケット CRC32 完全一致"
        else
            echo "NG: 音声パケット CRC32 不一致"
        fi
    fi

    echo "--- 先頭の映像 pts - 先頭の音声 pts ---"
    local v_pts_before a_pts_before v_pts_after a_pts_after
    v_pts_before="$(first_pts v:0 "$elst_file")"
    a_pts_before="$(first_pts a:0 "$elst_file")"
    v_pts_after="$(first_pts v:0 "$stripped")"
    a_pts_after="$(first_pts a:0 "$stripped")"
    echo "除去前: 映像=${v_pts_before}s 音声=${a_pts_before}s"
    echo "除去後: 映像=${v_pts_after}s 音声=${a_pts_after}s"

    echo "--- priming の伝わり方 (ffmpeg デコーダログ) ---"
    ffmpeg -v trace -i "$elst_file" -f null - 2>&1 | grep -Ei "skip [0-9]+.*sample|injecting skip" \
        || echo "(priming に関するログなし)"
    echo
}

main() {
    require_tools
    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' EXIT

    measure_one "Opus" "fixtures/sample_elst.mp4" "fixtures/sample.mp4" "libopus, pre-skip は dOps"
    measure_one "AAC" "fixtures/sample_elst_aac.mp4" "fixtures/sample_aac.mp4" "aac, priming は elst のみ"

    echo "詳細な数値と方針は docs/measurements.md「elst 除去と A/V 相対時刻」を参照。"
}

main "$@"
