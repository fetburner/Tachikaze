# インストール先の既定は /usr/local。`make install PREFIX=$HOME/.local` のように
# 上書きすれば非 root でも完結する。DESTDIR は staging 用（PREFIX 自体には
# 含めず、install 時に単純連結する。パッケージングで root 以外の場所へ
# 一時的に敷き詰めるための慣習）。
PREFIX ?= /usr/local
DESTDIR ?=
BINDIR = $(PREFIX)/bin
DATADIR = $(PREFIX)/share

# `CARGO_TARGET_DIR` を設定している環境ではビルド成果物のパスが変わるため、
# 変数で受ける（既定は cargo の既定と同じ `target`）。
CARGO_TARGET_DIR ?= target
TACHIKAZE_BIN = $(CARGO_TARGET_DIR)/release/tachikaze

.PHONY: build install uninstall clean test

build:
	cargo build --release --locked

# JL コマンドファイルと外部3ツール（chapter_exe/dtvindex/join_logo_scp）は
# 本リポジトリに含まれないため install 対象にしない（配置先の取り決めは
# docs/toolchain-macos.md 参照）。
#
# macOS の `install` は BSD 版で GNU の `-D`/`-t` が無いため、ディレクトリ
# 作成（`install -d`）とファイル設置（`install -m`）を分けて書く。
install: build
	install -d "$(DESTDIR)$(BINDIR)"
	install -m 755 "$(TACHIKAZE_BIN)" "$(DESTDIR)$(BINDIR)/tachikaze"

uninstall:
	rm -f "$(DESTDIR)$(BINDIR)/tachikaze"

clean:
	cargo clean

test:
	cargo test
