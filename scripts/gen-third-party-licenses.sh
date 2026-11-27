#!/usr/bin/env bash
# 配布物に同梱する THIRD-PARTY-LICENSES.md を cargo-about で生成する（Linux / macOS 共通）。
# 使い方: scripts/gen-third-party-licenses.sh OUTPUT_PATH
# 生成物はコミットしない（.gitignore 済み）。リリースのときに作る。

set -euo pipefail

# cargo-about の版。更新するときは #220 のワークフローと揃えること。
CARGO_ABOUT_VERSION=0.9.3

if [ "$#" -ne 1 ]; then
    printf '使い方: %s OUTPUT_PATH\n' "$0" >&2
    exit 2
fi

root=$(cd "$(dirname "$0")/.." && pwd)

# cargo-about は相対パスを呼び出し元のディレクトリ基準で扱うので、出力先を絶対パスにする。
out=$1
case "$out" in
    /*) ;;
    *) out="$PWD/$out" ;;
esac

installed=$(cargo about --version 2>/dev/null | awk '{print $2}') || installed=
if [ "$installed" != "$CARGO_ABOUT_VERSION" ]; then
    printf 'cargo-about %s が必要です（検出: %s）。次で入れてください:\n' \
        "$CARGO_ABOUT_VERSION" "${installed:-なし}" >&2
    printf '  cargo install --locked --features cli cargo-about@%s\n' "$CARGO_ABOUT_VERSION" >&2
    exit 1
fi

cargo about generate \
    --manifest-path "$root/Cargo.toml" \
    -c "$root/about.toml" \
    "$root/about.hbs" \
    -o "$out"

printf '生成: %s\n' "$out"
