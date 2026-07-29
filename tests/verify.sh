#!/bin/bash
# 手元の実ファイルで「映像パスの無劣化カット」を検証するためのヘルパースクリプト。
#
# tests/video_e2e.rs の CRC32 比較ロジックを、実際の録画ファイル (chapter_exe /
# join_logo_scp で得た trim.avs と、tachikaze cut --video-only の出力) に対して
# 手動で確認したいときに使う。
#
# CLAUDE.md / docs/lossless-cut.md の罠2:
#   無劣化の検証に md5 を使ってはいけない（h264_mp4toannexb が IDR ごとに SPS/PPS
#   を再挿入するため、バイト数が一致してもハッシュがずれる）。
#   → ffprobe -show_data_hash CRC32 でパケット単位に比較する。
#
# CLAUDE.md / docs/lossless-cut.md の罠1:
#   切り出しはパケット数で行う。時間指定 (ffmpeg -t) は使わない。
#   → 参照セグメントの生成には -frames:v (厳密にパケット数を数える) を使う。
#
# 使い方:
#
#   1) 元ファイルのキーフレーム一覧を確認する（保持区間の開始点探しに使う）:
#        tests/verify.sh keyframes IN.mp4
#
#   2) 元ファイルの区間と cut 済み出力の映像パケット CRC32 を比較する:
#        tests/verify.sh compare IN.mp4 OUT.mp4 SEEK1:COUNT1 [SEEK2:COUNT2 ...]
#
#      SEEK は元ファイルの同期サンプル(キーフレーム)の pts_time（秒、`keyframes`
#      サブコマンドの出力から拾う）、COUNT はその区間で保持したパケット数
#      ( E - S、docs/lossless-cut.md の規則どおり)。
#      複数区間を渡すと、それぞれを元ファイルから抜き出して連結したものを
#      OUT.mp4 の全パケットと突き合わせる。
#
#   例 (tests/fixtures/sample.mp4 で表示順 [0,120) と [360,480) を保持した場合):
#        tests/verify.sh keyframes tests/fixtures/sample.mp4
#        tests/verify.sh compare tests/fixtures/sample.mp4 out.mp4 \
#            0.066733:120 8.141733:120
#
# 前提: ffmpeg / ffprobe が PATH にあること。

set -euo pipefail

# -ss に加える微小な補正値。浮動小数点誤差でキーフレームの手前へ落ちるのを防ぐ
# (docs/lossless-cut.md「参考: 検証で通した手順」と同じ値。1フレーム=33.4msに対して
# 十分小さい)。カットの実装そのものには使わず、あくまで比較対象をffmpegで作るための値。
SEEK_EPSILON="0.005"

usage() {
    echo "使い方:" >&2
    echo "  $0 keyframes IN.mp4" >&2
    echo "  $0 compare IN.mp4 OUT.mp4 SEEK1:COUNT1 [SEEK2:COUNT2 ...]" >&2
    exit 1
}

require_tools() {
    for bin in ffmpeg ffprobe; do
        if ! command -v "$bin" >/dev/null 2>&1; then
            echo "error: $bin が見つかりません。PATH を確認してください。" >&2
            exit 1
        fi
    done
}

# 映像ストリームのパケット CRC32 一覧をファイル(=デコード)順に表示する。
video_packet_crc32() {
    local path="$1"
    ffprobe -v error -select_streams v:0 \
        -show_entries packet=size,data_hash -show_data_hash CRC32 \
        -of csv=p=0 "$path"
}

cmd_keyframes() {
    local input="${1:-}"
    [ -n "$input" ] || usage

    echo "# デコード順インデックス(0始まり), pts_time(秒), 同期サンプルか" >&2
    ffprobe -v error -select_streams v:0 \
        -show_entries packet=pts_time,flags \
        -of csv=p=0 "$input" \
    | awk -F',' '{
        is_sync = (index($2, "K") == 1) ? "sync" : "-";
        printf "%d\t%s\t%s\n", NR - 1, $1, is_sync;
    }' \
    | awk -F'\t' '$3 == "sync" { print }'
}

cmd_compare() {
    local input="${1:-}"
    local output="${2:-}"
    shift 2 || usage
    [ -n "$input" ] && [ -n "$output" ] || usage
    [ "$#" -ge 1 ] || usage

    # trap は関数を抜けたあと(スクリプト終了時)に発火するため、tmp_dir は
    # local にしない(local だとスコープ外になり `set -u` で unbound variable になる)。
    tmp_dir="$(mktemp -d)"
    trap 'rm -rf "$tmp_dir"' EXIT

    local ref_crc32="$tmp_dir/reference_crc32.txt"
    : > "$ref_crc32"

    local i=0
    for range in "$@"; do
        local seek="${range%%:*}"
        local count="${range##*:}"
        if [ "$seek" = "$range" ] || [ "$count" = "$range" ]; then
            echo "error: 区間の指定は SEEK:COUNT の形式にしてください: $range" >&2
            exit 1
        fi

        local seek_with_epsilon
        seek_with_epsilon="$(awk -v s="$seek" -v e="$SEEK_EPSILON" 'BEGIN { printf "%.6f", s + e }')"

        local seg_path="$tmp_dir/ref_$i.mp4"
        echo "# 区間 $i: -ss $seek_with_epsilon -frames:v $count (seek=$seek + epsilon=$SEEK_EPSILON)" >&2
        ffmpeg -y -v error -ss "$seek_with_epsilon" -i "$input" \
            -frames:v "$count" -c copy -map 0:v:0 "$seg_path"

        local got_count
        got_count="$(video_packet_crc32 "$seg_path" | wc -l | tr -d ' ')"
        if [ "$got_count" != "$count" ]; then
            echo "error: 区間 $i: 抜き出せたパケット数($got_count)が指定値($count)と一致しません。" >&2
            echo "       ファイル末尾を超えていないか、SEEK が正しいキーフレームか確認してください。" >&2
            exit 1
        fi

        video_packet_crc32 "$seg_path" >> "$ref_crc32"
        i=$((i + 1))
    done

    local out_crc32="$tmp_dir/output_crc32.txt"
    video_packet_crc32 "$output" > "$out_crc32"

    local ref_count out_count
    ref_count="$(wc -l < "$ref_crc32" | tr -d ' ')"
    out_count="$(wc -l < "$out_crc32" | tr -d ' ')"

    if [ "$ref_count" != "$out_count" ]; then
        echo "NG: パケット数が一致しません (出力=$out_count, 参照(元ファイルの該当区間を連結)=$ref_count)" >&2
        exit 1
    fi

    local first_mismatch
    first_mismatch="$(
        paste -d'|' "$ref_crc32" "$out_crc32" \
        | awk -F'|' '$1 != $2 { print NR - 1; exit }'
    )"

    if [ -n "$first_mismatch" ]; then
        echo "NG: 最初に食い違ったパケット番号 = $first_mismatch" >&2
        echo "    参照: $(sed -n "$((first_mismatch + 1))p" "$ref_crc32")" >&2
        echo "    出力: $(sed -n "$((first_mismatch + 1))p" "$out_crc32")" >&2
        exit 1
    fi

    echo "OK: 映像パケット $out_count 個すべて CRC32 が一致しました。"
}

main() {
    require_tools
    local sub="${1:-}"
    [ -n "$sub" ] || usage
    shift

    case "$sub" in
        keyframes)
            cmd_keyframes "$@"
            ;;
        compare)
            cmd_compare "$@"
            ;;
        *)
            usage
            ;;
    esac
}

main "$@"
