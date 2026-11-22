#!/usr/bin/env bash
# Linux 向け。macOS の手元ビルドにはパッチが必要なため対象外。
# 必要な apt パッケージ:
#   build-essential pkg-config git ca-certificates libavformat-dev
#   libavcodec-dev libavutil-dev libswscale-dev libswresample-dev
# 使い方: scripts/build-external-tools.sh BUILD_DIR PREFIX
#   または PREFIX=/usr/local scripts/build-external-tools.sh BUILD_DIR

set -euo pipefail

# SHA を更新するときは docs/docker.md の更新手順に従う。
CHAPTER_EXE_SHA=266ff66f1052a684552a0a3e962b4b796862b8ac
JOIN_LOGO_SCP_SHA=4dce4aa71efb7b55764f8dbd2277e5e45df331dd
DTVINDEX_SHA=2fdbe1ba116b2ad6a018149716454635c7dfb7b9

usage() {
    printf '使い方: %s BUILD_DIR PREFIX\n' "$0" >&2
    printf '   または PREFIX=/usr/local %s BUILD_DIR\n' "$0" >&2
}

if [[ $# -eq 2 ]]; then
    BUILD_DIR=$1
    PREFIX=$2
elif [[ $# -eq 1 && -n "${PREFIX:-}" ]]; then
    BUILD_DIR=$1
else
    usage
    exit 2
fi

mkdir -p -- "$BUILD_DIR" "$PREFIX"
BUILD_DIR=$(cd -- "$BUILD_DIR" && pwd -P)
PREFIX=$(cd -- "$PREFIX" && pwd -P)

checkout_pinned_source() {
    local directory=$1
    local repository=$2
    local sha=$3

    # BUILD_DIR は専用の作業用ディレクトリとして使い、既存の同名ディレクトリを削除する。
    rm -rf -- "$directory"
    mkdir -p -- "$directory"
    git -C "$directory" init --quiet
    git -C "$directory" remote add origin "$repository"
    git -C "$directory" fetch --depth 1 origin "$sha"
    git -C "$directory" checkout --quiet --detach FETCH_HEAD
}

JOIN_LOGO_SCP_DIR=$BUILD_DIR/join_logo_scp
DTVINDEX_DIR=$BUILD_DIR/dtvindex
CHAPTER_EXE_DIR=$BUILD_DIR/chapter_exe

# chapter_exe の Makefile は src から ../../dtvindex を自動検出する。
# 離すとエラーを出さずに連携が無効になるため、同じ親ディレクトリに置く。
checkout_pinned_source "$JOIN_LOGO_SCP_DIR" \
    https://github.com/tobitti0/join_logo_scp "$JOIN_LOGO_SCP_SHA"
checkout_pinned_source "$DTVINDEX_DIR" \
    https://github.com/tobitti0/dtvindex "$DTVINDEX_SHA"
checkout_pinned_source "$CHAPTER_EXE_DIR" \
    https://github.com/tobitti0/chapter_exe "$CHAPTER_EXE_SHA"

# join_logo_scp はソースにパッチを当てず、src/ で make すればビルドできる。
(cd -- "$JOIN_LOGO_SCP_DIR/src" && make)
# dtvindex もソースにパッチを当てずにビルドできる。
(cd -- "$DTVINDEX_DIR" && make)
# macOS 向けの malloc.h / memalign / uname -m の SIMD 分岐に関する3点パッチは
# Linux では不要。無修正の make で AviSynth=enabled, dtvindex=enabled になる。
# 詳細は docs/docker.md を参照。
(cd -- "$CHAPTER_EXE_DIR/src" && make)

# chapter_exe -v は設定を表示した後、入力ファイルがないため非ゼロ終了する。
# 終了コードは無視し、出力に dtvindex=enabled が含まれるかだけで判定する。
chapter_version=$( (cd -- "$CHAPTER_EXE_DIR/src" && ./chapter_exe -v) 2>&1 || true )
printf '%s\n' "$chapter_version"
if [[ "$chapter_version" != *'dtvindex=enabled'* ]]; then
    printf 'chapter_exe が dtvindex=enabled でビルドされていません\n' >&2
    exit 1
fi

install -D -m 0755 "$CHAPTER_EXE_DIR/src/chapter_exe" "$PREFIX/bin/chapter_exe"
install -D -m 0755 "$JOIN_LOGO_SCP_DIR/src/join_logo_scp" "$PREFIX/bin/join_logo_scp"
install -D -m 0755 "$DTVINDEX_DIR/build/dtvindex" "$PREFIX/bin/dtvindex"

JL_DIR=$PREFIX/share/join_logo_scp/JL
rm -rf -- "$JL_DIR"
mkdir -p -- "${JL_DIR%/JL}"
cp -a -- "$JOIN_LOGO_SCP_DIR/JL" "$JL_DIR"
