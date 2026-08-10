# 外部ツールの macOS ビルド手順

→ 入口: [overview.md](overview.md)

macOS arm64（Apple Silicon）で**実際にビルド・実行して確認済み**（2026 年 7 月時点）。

## 結果まとめ

| ツール | ビルド | 必要な作業 |
|---|---|---|
| **join_logo_scp** | ✓ | **無修正**（`src` で `make`） |
| **dtvindex** | ✓ | **無修正**（リポジトリ直下で `make`。homebrew の FFmpeg 8.1.2 で通った） |
| **chapter_exe** | ✓ | **3 点のパッチが必要**（下記）。うちパッチ3（`uname -m` の SIMD 分岐）は、[docker.md](docker.md) で確認した Linux 向けビルドの時点の upstream HEAD では **既に `SIMD_BACKEND=auto` を持つ Makefile に書き換わっており、パッチを当てる対象の行自体が無い**（実測。下記「3. `uname -m`」節） |
| Amatsukaze 本体 | ✗ | Windows API が 500 箇所以上。**移植しない** |
| AmatsukazeGUI / Server | ✗ | WPF + .NET Framework 4.5 |

## 前提

```bash
brew install ffmpeg pkg-config
```

`dtvindex` は `libavformat` / `libavcodec` / `libavutil` / `libswscale` / `libswresample` を pkg-config で探す。

## join_logo_scp

```bash
git clone --depth 1 https://github.com/tobitti0/join_logo_scp.git
cd join_logo_scp/src && make
```

無修正で通る。`join_logo_scp` バイナリと `JL/` ディレクトリ（ルールファイル群）を使う。

## dtvindex

```bash
git clone --depth 1 https://github.com/tobitti0/dtvindex.git
cd dtvindex && make
```

`build/dtvindex`（CLI）と `build/libdtvindex.a`（静的ライブラリ）が生成される。無修正で通る。

## chapter_exe

```bash
git clone --depth 1 https://github.com/tobitti0/chapter_exe.git
```

**3 点のパッチが必要。** いずれも macOS 固有の問題で、アップストリームに PR できる規模。

### 1. `malloc.h` が存在しない

`src/chapter_exe.cpp` の `#include <malloc.h>` を `#include <stdlib.h>` にする。

### 2. `memalign()` が存在しない

`posix_memalign` の shim を入れる:

```c
static inline void* memalign(size_t a, size_t s) {
    void* p = NULL;
    if (posix_memalign(&p, a, s)) return NULL;
    return p;
}
```

### 3. `uname -m` が `arm64` を返すため SIMD 分岐が外れる

Makefile が `aarch64` だけを見ているため、macOS では `SYS_ARM64` が定義されず、`mvec.cpp` が `<emmintrin.h>`（SSE2）を include してコンパイルに失敗する。

```make
  UNAME_MACHINE := $(shell uname -m)
- ifeq ($(UNAME_MACHINE),aarch64)
+ ifneq ($(filter $(UNAME_MACHINE),aarch64 arm64),)
```

暫定的には `-DSYS_ARM64` を渡すだけでよい。

**現 upstream では不要（実測）**: [docker.md](docker.md) で `git clone --depth 1` した upstream HEAD の `chapter_exe/src/Makefile` は、この `UNAME_MACHINE` 分岐そのものを持たない書き方（`SIMD_BACKEND ?= auto` で NEON / SSE2 / スカラーを自動判定し、`WITH_DTVINDEX ?= auto` で dtvindex 連携も自動検出する）に既に置き換わっていた。Linux (arm64) 上で無修正の `make` を実行すると `chapter_exe motion SIMD: auto` → 実行時ログ `Motion SIMD: NEON` になる。パッチ1（`malloc.h`）・パッチ2（`memalign()`）は macOS 固有の libc の差なので今も必要（[docker.md](docker.md)「macOS の3点パッチが不要だった理由」）。

### ビルド（dtvindex 連携あり）

`dtvindex` を兄弟ディレクトリ（`../../dtvindex`）に置くと Makefile が自動検出する。手動で指定する場合:

```bash
cd chapter_exe/src
make CPPFLAGS="-I../extras -I. -DSYS_ARM64 -DHAVE_AVISYNTH=1 -I../avisynth \
      -DHAVE_DTVINDEX=1 -I../../dtvindex/include \
      $(pkg-config --cflags libavformat libavcodec libavutil libswscale libswresample)" \
     LDLIBS="-pthread ../../dtvindex/build/libdtvindex.a \
      $(pkg-config --libs libavformat libavcodec libavutil libswscale libswresample)"
```

起動時に有効な入力方式が表示される:

```console
$ ./chapter_exe
chapter_exe: AviSynth=enabled, dtvindex=enabled
```

**AviSynth は macOS に無いので実際には dtvindex 経路だけを使う。** `-DHAVE_AVISYNTH=0` でもよい。

## SIMD は移植の障害ではない

`chapter_exe` の SSE2（`mvec.cpp`）も、Amatsukaze の AVX（`ComputeKernel.cpp`、121 行）も、**スカラー版へのフォールバックを持つ設計**になっている。

Amatsukaze 側の例（`LogoScan.hpp`）:

```cpp
pCalcCorrelation5x5 = IsAVXAvailable() ? CalcCorrelation5x5_AVX : CalcCorrelation5x5;
```

arm64 では該当ファイルをビルド対象から外し、`IsAVXAvailable()` が `false` を返すスタブを置けば通る。性能は落ちる（5×5 = 25 要素の内積が最内ループ）が、必要なら NEON 版は 30 行程度。

**ただし本プロジェクトでは Amatsukaze 側のコードを使わないので、この話は参考情報。**

## 実行例

3 ツールをビルドしたディレクトリ直下で、`JL/`（`join_logo_scp` と同じディレクトリ）を相対指定して直接実行する場合:

```bash
dtvindex build IN.mp4 -o IN.dtvi
chapter_exe -v IN.mp4 -o scp.txt
join_logo_scp -inscp scp.txt -incmd JL/JL_標準.txt \
              -o trim.avs -oscp detail.jls \
              -set autocm_sub 11 -set param_cuttr 1
```

`chapter_exe` はメディアファイルの隣に `<media>.dtvi` を自動生成する。元ファイルのディレクトリを汚したくない場合は**シンボリックリンクを作業ディレクトリに張る**とよい（実測: 800 MB のファイルでもコピー不要）。

```bash
ln -sf /path/to/IN.mp4 work.mp4
chapter_exe -v work.mp4 -o scp.txt      # work.mp4.dtvi が作業ディレクトリに作られる
```

これは3ツールを直接叩く場合の例。`tachikaze` 経由で使う場合は下記「ビルド後の配置とインストール」の構成にすれば、`JL/JL_標準.txt` のような相対指定は不要になる（`tachikaze` 側が探索する。[architecture.md](architecture.md)「パス解決」節）。

## ビルド後の配置とインストール

3 ツールと `JL/` は本リポジトリに含まれないため、`tachikaze` 側の `make install`（後述）の対象にはならない。配置先は `tachikaze` 本体の探索仕様（[architecture.md](architecture.md)「パス解決」節）に合わせる。

| もの | 配置先 |
|---|---|
| `chapter_exe` / `dtvindex` / `join_logo_scp` | `$PREFIX/bin`（`PATH` に通せば `tachikaze` 側の探索で解決できる） |
| `JL/`（ルールファイル群） | `$PREFIX/share/join_logo_scp/JL/` |
| `tachikaze`（本リポジトリ） | `make install PREFIX=...`（本リポジトリのルートで実行） |

例（`$PREFIX = /usr/local`）:

```bash
install -m 755 chapter_exe dtvindex join_logo_scp /usr/local/bin/
install -d /usr/local/share/join_logo_scp
cp -R JL /usr/local/share/join_logo_scp/

cd <tachikaze リポジトリ>
make install PREFIX=/usr/local
```

この構成であれば、`tachikaze` は `--jl-file` を指定せずに動く（外部ツール自体は `PATH` 経由でしか解決しないため、`$PREFIX/bin` を `PATH` に通しておくことが前提）。JL の配置先は `$PREFIX/share/join_logo_scp/JL/` だけが有効で、`tachikaze` はここ以外（旧 `tachikaze/JL/` のような専用データディレクトリなど）を探さない。`$HOME/.local` など非 root なプレフィックスでも同様（`make install PREFIX=$HOME/.local` と `$PREFIX/bin` を `PATH` に追加すればよい）。
