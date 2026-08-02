# syntax=docker/dockerfile:1
#
# Linux (arm64) 向けマルチステージビルド。実測は docs/docker.md 参照。
#
# builder: 外部3ツール（chapter_exe / join_logo_scp / dtvindex）と tachikaze 本体を
#          ソースからビルドする。3ツールは本リポジトリに含まれないため git clone する
#          （バージョン留めなし。pin したい場合はここに commit SHA を指定する）。
# runtime: ビルド済みバイナリと join_logo_scp の JL ルールファイル、ffmpeg/ffprobe
#          （`prepare` と `--verify` が使う）だけを積んだ実行用イメージ。
FROM ubuntu:24.04 AS builder

# libavformat-dev 等は dtvindex と chapter_exe の pkg-config 依存（両方とも
# libavformat/libavcodec/libavutil/libswscale/libswresample を要求する）。
# --no-install-recommends: ビルド時点で推奨パッケージ（ドキュメント類等）は不要。
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    build-essential \
    pkg-config \
    git \
    ca-certificates \
    curl \
    libavformat-dev \
    libavcodec-dev \
    libavutil-dev \
    libswscale-dev \
    libswresample-dev \
    && rm -rf /var/lib/apt/lists/*

# rustup で最新の stable を入れる。Ubuntu 24.04 の apt 版 cargo は edition 2021 の
# 依存クレート（clap 4.6 系など）に対して古すぎるため使わない。
ENV RUSTUP_HOME=/usr/local/rustup \
    CARGO_HOME=/usr/local/cargo \
    PATH=/usr/local/cargo/bin:$PATH
RUN curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \
    | sh -s -- -y --profile minimal --default-toolchain stable

WORKDIR /build

# join_logo_scp: 無修正でビルドできる（docs/toolchain-macos.md で macOS でも無修正と
# 確認済み。Linux でも同様。実測は docs/docker.md）。
RUN git clone --depth 1 https://github.com/tobitti0/join_logo_scp.git \
    && cd join_logo_scp/src && make

# dtvindex: 無修正でビルドできる。chapter_exe の Makefile が兄弟ディレクトリ
# （../../dtvindex、つまり /build/dtvindex）を自動検出するため、chapter_exe と
# 同じ /build 直下に置く。
RUN git clone --depth 1 https://github.com/tobitti0/dtvindex.git \
    && cd dtvindex && make

# chapter_exe: docs/toolchain-macos.md に書かれている3点パッチ（malloc.h /
# memalign / uname -m の SIMD 分岐）はいずれも macOS 固有の問題で、Linux (arm64)
# では不要（実測。詳細は docs/docker.md）。無修正で `make` が
# `AviSynth=enabled, dtvindex=enabled` になることをビルド時ログで確認する。
RUN git clone --depth 1 https://github.com/tobitti0/chapter_exe.git \
    && cd chapter_exe/src && make \
    && ./chapter_exe -v 2>&1 | grep -q 'dtvindex=enabled' \
    || (echo 'chapter_exe が dtvindex=enabled でビルドされていません' >&2 && exit 1)

# tachikaze 本体。.dockerignore で target/ と tests/data/ を除いたソースだけを
# コンテキストに含める。
COPY Cargo.toml Cargo.lock /usr/src/tachikaze/
COPY src /usr/src/tachikaze/src
WORKDIR /usr/src/tachikaze
RUN cargo build --release --locked

FROM ubuntu:24.04 AS runtime

# ffmpeg パッケージが `prepare`/`--verify` 用の ffmpeg/ffprobe CLI 本体と、
# chapter_exe/dtvindex が実行時に要求する libavformat.so.60 等の共有ライブラリを
# 両方提供する（builder の *-dev パッケージと同じ apt リポジトリ・同じ
# Ubuntu 24.04 なのでバージョンが揃う）。
# --no-install-recommends: 実測で 1.14GB → 753MB（-34%）。X11/フォント関連の
# 推奨パッケージを削っても auto --verify のフルパイプラインは通ることを確認済み
# （docs/docker.md「既知の制約」）。
RUN apt-get update -qq && DEBIAN_FRONTEND=noninteractive apt-get install -y -qq --no-install-recommends \
    ffmpeg \
    ca-certificates \
    && rm -rf /var/lib/apt/lists/*

COPY --from=builder /build/chapter_exe/src/chapter_exe /usr/local/bin/chapter_exe
COPY --from=builder /build/join_logo_scp/src/join_logo_scp /usr/local/bin/join_logo_scp
COPY --from=builder /build/dtvindex/build/dtvindex /usr/local/bin/dtvindex
COPY --from=builder /usr/src/tachikaze/target/release/tachikaze /usr/local/bin/tachikaze
COPY --from=builder /build/join_logo_scp/JL/ /usr/local/share/join_logo_scp/JL/

# 出力は入力の隣に書く方針（変更しない）。メディアディレクトリは docker run 時に
# rw でマウントすること（docs/docker.md）。
ENTRYPOINT ["tachikaze"]
CMD ["--help"]
