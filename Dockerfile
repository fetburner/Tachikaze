# syntax=docker/dockerfile:1
#
# Linux (arm64) 向けマルチステージビルド。実測は docs/docker.md 参照。
#
# tools:     外部3ツール（chapter_exe / join_logo_scp / dtvindex）をソースからビルド。
#            commit SHA は scripts/build-external-tools.sh で固定する。
#            runtime と同じ debian:trixie でビルドし、libav の共有ライブラリ版を揃える。
# tachikaze: 公式 rust イメージで本体を cargo build。rustup は不要。
# runtime:   ビルド済みバイナリと join_logo_scp の JL ルールファイル、ffmpeg/ffprobe
#            （`prepare` と `--verify` が使う）だけを積んだ実行用イメージ。
#
# apt / cargo は BuildKit の cache mount を使う。キャッシュはイメージレイヤーに
# 入らないので、レイヤー内で `rm -rf /var/lib/apt/lists/*` する必要はない。

# ---------------------------------------------------------------------------
# 外部3ツール
# ---------------------------------------------------------------------------
FROM debian:trixie AS tools

# docker-clean があると install 後に /var/cache/apt を消すため、cache mount が効かない。
RUN rm -f /etc/apt/apt.conf.d/docker-clean

# libavformat-dev 等は dtvindex と chapter_exe の pkg-config 依存（両方とも
# libavformat/libavcodec/libavutil/libswscale/libswresample を要求する）。
# --no-install-recommends: ビルド時点で推奨パッケージ（ドキュメント類等）は不要。
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    build-essential \
    pkg-config \
    git \
    ca-certificates \
    libavformat-dev \
    libavcodec-dev \
    libavutil-dev \
    libswscale-dev \
    libswresample-dev

WORKDIR /build

COPY scripts/build-external-tools.sh /build/build-external-tools.sh
RUN /build/build-external-tools.sh /build /usr/local

# ---------------------------------------------------------------------------
# tachikaze 本体
# ---------------------------------------------------------------------------
# rust:1-trixie = 最新 stable の 1.x。tools / runtime と同じ Debian 13 (trixie)。
FROM rust:1-trixie AS tachikaze

WORKDIR /usr/src/tachikaze
# .dockerignore で target/ と tests/data/ を除いたソースだけをコンテキストに含める。
COPY Cargo.toml Cargo.lock ./
COPY src ./src

# registry / git / target を cache mount に載せる。target はマウント先にしか無いので、
# 後段の COPY --from 用に成果物をレイヤーへコピーする。
RUN --mount=type=cache,target=/usr/local/cargo/registry,sharing=locked \
    --mount=type=cache,target=/usr/local/cargo/git,sharing=locked \
    --mount=type=cache,target=/usr/src/tachikaze/target,sharing=locked \
    cargo build --release --locked \
    && cp target/release/tachikaze /usr/local/bin/tachikaze

# ---------------------------------------------------------------------------
# 実行用
# ---------------------------------------------------------------------------
FROM debian:trixie AS runtime

RUN rm -f /etc/apt/apt.conf.d/docker-clean

# ffmpeg パッケージが `prepare`/`--verify` 用の ffmpeg/ffprobe CLI 本体と、
# chapter_exe/dtvindex が実行時に要求する libavformat 等の共有ライブラリを
# 両方提供する（tools ステージの *-dev パッケージと同じ apt リポジトリ・同じ
# debian:trixie なのでバージョンが揃う）。
# --no-install-recommends: 実測で 1.14GB → 753MB（-34%、当時 ubuntu:24.04）。
# X11/フォント関連の推奨パッケージを削っても auto --verify のフルパイプラインは
# 通ることを確認済み（docs/docker.md「既知の制約」）。
ARG TACHIKAZE_EXTRA_APT=""
RUN --mount=type=cache,target=/var/cache/apt,sharing=locked \
    --mount=type=cache,target=/var/lib/apt,sharing=locked \
    apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    ffmpeg \
    ca-certificates \
    $TACHIKAZE_EXTRA_APT

COPY --from=tools /usr/local/bin/chapter_exe /usr/local/bin/chapter_exe
COPY --from=tools /usr/local/bin/join_logo_scp /usr/local/bin/join_logo_scp
COPY --from=tools /usr/local/bin/dtvindex /usr/local/bin/dtvindex
COPY --from=tachikaze /usr/local/bin/tachikaze /usr/local/bin/tachikaze
COPY --from=tools /usr/local/share/join_logo_scp/JL/ /usr/local/share/join_logo_scp/JL/

# 出力は入力の隣に書く方針（変更しない）。メディアディレクトリは docker run 時に
# rw でマウントすること（docs/docker.md）。
ENTRYPOINT ["tachikaze"]
CMD ["--help"]
