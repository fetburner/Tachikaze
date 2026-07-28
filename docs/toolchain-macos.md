# 外部ツールの macOS ビルド手順

→ 入口: [overview.md](overview.md)

macOS arm64（Apple Silicon）で**実際にビルド・実行して確認済み**（2026 年 7 月時点）。

## 結果まとめ

| ツール | ビルド | 必要な作業 |
|---|---|---|
| **join_logo_scp** | ✓ | **無修正**（`src` で `make`） |
| **dtvindex** | ✓ | **無修正**（リポジトリ直下で `make`。homebrew の FFmpeg 8.1.2 で通った） |
| **chapter_exe** | ✓ | **3 点のパッチが必要**（下記） |
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

`src/chapter_exe.cpp:7` の `#include <malloc.h>` を `#include <stdlib.h>` にする。

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

Amatsukaze 側の例（`LogoScan.hpp:123`）:

```cpp
pCalcCorrelation5x5 = IsAVXAvailable() ? CalcCorrelation5x5_AVX : CalcCorrelation5x5;
```

arm64 では該当ファイルをビルド対象から外し、`IsAVXAvailable()` が `false` を返すスタブを置けば通る。性能は落ちる（5×5 = 25 要素の内積が最内ループ）が、必要なら NEON 版は 30 行程度。

**ただし本プロジェクトでは Amatsukaze 側のコードを使わないので、この話は参考情報。**

## 実行例

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
