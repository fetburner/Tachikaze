# Docker で使う

→ 入口: [overview.md](overview.md)

外部3ツール（chapter_exe / join_logo_scp / dtvindex）と ffmpeg を自分でビルド・配置したくない人向けに、`Dockerfile` を用意してある。**変えないもの**: 出力は入力の隣に書く。`analyze` はキャッシュに入力への symlink（`work.mp4`）を張って `chapter_exe` を走らせる。この2つはコンテナ内でも同じ（[architecture.md](architecture.md)「パス解決」節）。

Linux (arm64) で**実際に `docker build` / `docker run` を実行して確認済み**（2026 年 8 月時点。Apple Silicon 上の Colima、`docker` サーバは `linux/arm64`）。

## 結果まとめ（実測）

| 項目 | 結果 |
|---|---|
| `join_logo_scp` の Linux ビルド | ✓ 無修正 |
| `dtvindex` の Linux ビルド | ✓ 無修正 |
| `chapter_exe` の Linux ビルド | ✓ **無修正**（[toolchain-macos.md](toolchain-macos.md) の3点パッチはいずれも不要。下記「macOS の3点パッチが不要だった理由」） |
| `chapter_exe -v` の起動ログ | `AviSynth=enabled, dtvindex=enabled`、`Motion SIMD: NEON`（SSE2 用パッチなしで NEON が自動選択される） |
| `cargo build --release --locked`（tachikaze 本体） | ✓（`rustup` で入れた stable、Ubuntu 24.04 上） |
| `docker build -t tachikaze .` | ✓ arm64 で通る（初回ビルド約1分45秒、レイヤーキャッシュ後は数秒） |
| `docker run` での `auto` 一連動作（`tests/fixtures/sample.mp4`、`--work-dir` 使用） | ✓（`prepare`→`analyze`→gate→`cut`→自己検証→`--verify` の CRC32 検証まで完走） |

## macOS の3点パッチが不要だった理由

[toolchain-macos.md](toolchain-macos.md)「chapter_exe」に書いた3点（`malloc.h` が無い / `memalign()` が無い / `uname -m` が `arm64` を返して SIMD 分岐が外れる）は、いずれも **BSD libc（macOS）と glibc（Linux）の差**、または**古い Makefile の `uname -m` 分岐**が原因だった。Linux 上の `chapter_exe`（本 issue 作業時点の upstream HEAD）では:

- `malloc.h` は glibc に存在する（Linux 固有ヘッダなので macOS だけ無かった）
- `memalign()` は glibc に存在する（POSIX 標準ではないが glibc は独自に提供している）
- upstream の `Makefile` が `SIMD_BACKEND=auto` で NEON / SSE2 / スカラーを自動判定するように既に書き換わっており、`uname -m` 分岐の問題自体が発生しない

そのため Linux ビルドでは **`git clone` してきたソースに一切パッチを当てず** `make` するだけで、`dtvindex=enabled` かつ NEON SIMD の `chapter_exe` が得られる。

## Dockerfile の構成

マルチステージ（`Dockerfile`）:

1. **builder**: `ubuntu:24.04` + `build-essential` / `pkg-config` / ffmpeg の `-dev` パッケージ群 + `rustup`（Ubuntu 24.04 の apt 版 `cargo` は edition 2021 の依存クレートに対して古すぎるため使わない）。`join_logo_scp` / `dtvindex` / `chapter_exe` を `git clone --depth 1` してビルドし、`tachikaze` 本体も `cargo build --release --locked` する。`chapter_exe` のビルド直後に `chapter_exe -v` の出力へ `dtvindex=enabled` が含まれるかを `RUN` の中で検証し、含まれなければビルドを失敗させる（`dtvindex=disabled` のまま静かに動かなくなることを防ぐ。issue の「罠」参照）
2. **runtime**: `ubuntu:24.04` + `ffmpeg` パッケージ（`prepare` と `--verify` が使う `ffmpeg` / `ffprobe` 本体と、`chapter_exe` / `dtvindex` が実行時に要求する `libavformat.so.60` 等の共有ライブラリを兼ねる。builder の `-dev` パッケージと同じ Ubuntu 24.04 の apt リポジトリなのでバージョンが揃う）。4つのバイナリを `/usr/local/bin/` に、`JL/` を `/usr/local/share/join_logo_scp/JL/` に置く

`ENTRYPOINT ["tachikaze"]` なので `docker run tachikaze <サブコマンド> ...` がそのまま `tachikaze <サブコマンド> ...` になる。

3ツールはバージョン固定（pin）していない（`git clone --depth 1` は常に upstream の default branch HEAD を取る）。再現性が必要なら `Dockerfile` の各 `git clone` に commit SHA を指定すること。

## ビルド

```console
$ make docker-build          # docker build -t tachikaze . と同じ
```

## 実行

**メディアディレクトリは rw でマウントする**（出力は入力の隣に書く方針のため）。`--cache-dir` はまだ実装されていない（別 issue #67 の予定インターフェース）。現時点では `--work-dir` を使う。

```console
$ MEDIA_DIR=/path/to/recordings   # ホスト上の絶対パス
$ CACHE_DIR=/path/to/cache        # 同上。#67 で --cache-dir に変わる予定

$ docker run --rm \
    -v "$MEDIA_DIR":"$MEDIA_DIR" \
    -v "$CACHE_DIR":"$CACHE_DIR" \
    tachikaze auto "$MEDIA_DIR/IN.mp4" --work-dir "$CACHE_DIR" --verify
```

`analyze` / `cut` を個別に叩く場合も同様に、`-v "$MEDIA_DIR":"$MEDIA_DIR"` でホストと同じ絶対パスにマウントする。

```console
$ docker run --rm -v "$MEDIA_DIR":"$MEDIA_DIR" tachikaze \
    analyze "$MEDIA_DIR/IN.mp4" -o "$MEDIA_DIR/trim.avs" --report --work-dir "$CACHE_DIR"
$ docker run --rm -v "$MEDIA_DIR":"$MEDIA_DIR" tachikaze \
    cut "$MEDIA_DIR/IN.mp4" --trim "$MEDIA_DIR/trim.avs" -o "$MEDIA_DIR/OUT.mp4" \
    --dtvi "$CACHE_DIR/work.mp4.dtvi"
```

## キャッシュがホストと共有できない条件

キャッシュディレクトリ名（既定 `${XDG_CACHE_HOME:-~/.cache}/tachikaze/<入力ごと>/`）は**入力ファイルの絶対パスのハッシュ**から決まる（`src/workdir.rs::cache_dir_for_input`、[architecture.md](architecture.md)「パス解決」節）。**コンテナ内のマウント先パスがホストと違うと、同じ入力ファイルでも別のディレクトリ名になり、ネイティブ実行（macOS で直接 `tachikaze` を動かす場合）とキャッシュを共有できない。**

- 共有したい場合: 上記の例のように、**ホストと同じ絶対パスにマウントする**（`-v "$MEDIA_DIR":"$MEDIA_DIR"`）。既定の XDG キャッシュディレクトリを使うなら、コンテナ内の `$HOME`（既定 `/root`）もホストの `$HOME` に合わせて `-v` するか、`TACHIKAZE_CACHE_DIR` / `--work-dir` で明示的に固定する
- 共有しない場合: パスが違っていても動作自体は壊れない（コンテナ内で毎回 `analyze` が作り直すだけ）。ただし `auto` を何度も叩くたびに `dtvindex` / `chapter_exe` / `join_logo_scp` を再実行することになり、ネイティブ実行時に作ったキャッシュは再利用されない

## 実行例で確認したこと（実測）

`tests/fixtures/gen.sh` で作った `sample.mp4`（H.264 + Opus, 20秒, GOP 120 固定）を `$MEDIA_DIR` に置き、`auto --work-dir $CACHE_DIR --verify --force --no-cm` を実行して確認した:

- `dtvindex build` → `chapter_exe -v` → `join_logo_scp` が3つとも正常終了する
- `chapter_exe` がメディアファイルの隣ではなく `--work-dir`（キャッシュディレクトリ）内の `work.mp4`（symlink）の隣に `.dtvi` を作る（コンテナ内でもホスト同様、`analyze` の symlink 回避が効いている）
- `cut` の自己検証（パケット数・表示順・同期サンプル・音声同期）が通り、`--verify` の ffprobe CRC32 検証も通る
- 出力（`sample_CMcut.mp4`）がホスト側のメディアディレクトリに書き戻される（rw マウントが機能している）

**`--no-cm` を付けた理由**: `sample.mp4` は CM 検出テスト用ではなく単体テスト用の合成素材で、実際の CM ブロックを含まない。`join_logo_scp` はこの入力全体を保持区間と判定するため、`auto` が既定で付ける CM 側出力（`--no-cm` 未指定時の `*_CM.mp4`）の保持区間が空になり、`cut` が「出力に含まれるトラックが1本もありません」で失敗する。これは**この合成フィクスチャの内容に起因する既知の挙動**で、Docker 環境固有の問題ではない（ネイティブ実行でも同じ入力・同じオプションなら同じ結果になる）。実際の録画ファイル（CM を含む）であれば `--no-cm` は不要。

`--force` を付けた理由: 同じ理由（検出対象の CM がそもそも無い合成素材）で gate が「除去フレーム数 0 → 疑わしいので止める」と判定するため、smoke test として最後まで通す目的で判定を無視した。実際の運用では gate の停止判定を無視せず、`trim.avs` を確認してから `--force` するか `cut` を直接叩くこと。

## 既知の制約

- **イメージサイズが大きい**（実測 1.14GB）。ランタイムの `ffmpeg` パッケージが X11・フォント関連ライブラリなど大量の推移的依存を持ち込むため。`--no-install-recommends` は既に付けているが、`ffmpeg` パッケージ自体の依存が大きい。サイズを詰めるなら ffmpeg を静的ビルドする、または `apt` の代わりに BtbN の静的ビルド済みバイナリを使う方法があるが、本 issue の範囲では対応しない
- 3ツールはバージョン固定していない（上記「Dockerfile の構成」参照）。ビルドのたびに upstream の最新 HEAD を取るため、upstream 側の変更で本書の実測（パッチ不要）が将来変わる可能性がある
- `--cache-dir` はまだ無い（#67 で実装予定）。実装されたら、上記「実行」の例と「キャッシュがホストと共有できない条件」の節を `--work-dir` から `--cache-dir` に書き換えること
