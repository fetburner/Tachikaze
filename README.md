# Tachikaze

mp4 に変換済みの録画ファイルを、**再エンコードせずに CM カット**するツールです。

## Amatsukaze について

本プロジェクトは [nekopanda/Amatsukaze](https://github.com/nekopanda/Amatsukaze) に**強く影響を受けています**。Amatsukaze は MPEG2-TS を入力に CM カットとエンコードを行い mp4 を出力する、録画まわりの定番ツールです。Tachikaze は次のような動機から始まっています。

- **一度 mp4 にしてしまった録画**にも、Amatsukaze 流の CM カットを後から適用したい
- 同じ系統の処理を **macOS / Linux でも動かしたい**（Amatsukaze 本体は Windows 向けで、移植対象にはしていない）

パイプラインの考え方（無音・シーンチェンジ検出 → join_logo_scp 系の判定 → Trim に基づくカット）、ロゴ検出まわりの設計など、実装面でも Amatsukaze と周辺エコシステムから多くを学んでいます。Amatsukaze および作者の Nekopanda 氏、関連ツール作者の方々に感謝します。

Tachikaze 本体が担うのは主に「Trim リスト → ロスレスな mp4 切り出し」です。CM 検出自体は既存ツール（chapter_exe → join_logo_scp）に任せます。

## 最短の使い方

実行には外部ツールのセットアップが必要です（下記「依存」→「ビルド / インストール」の順に用意してください）。

```console
$ tachikaze auto IN.mp4 -o OUT.mp4
```

`auto` は `prepare` → `analyze` → gate → `cut` → `remap-subs` を対話なしで実行します。gate が疑わしいと判定した場合は cut せずに終了します（`--ignore-gate` で無視可）。

`analyze`/`auto` はロゴ検出を既定で有効にし、ロゴ矩形も入力から自動推定します。無効化するには `--no-logo` を指定してください（詳細は [docs/architecture.md](docs/architecture.md)「analyze」）。

判断を挟みたい場合:

```console
$ tachikaze analyze IN.mp4 -o trim.avs --report
$ tachikaze review IN.mp4   # 確認画面（キャッシュ内の review/index.html）を作る。その絶対パスを標準出力に出す
$ tachikaze cut IN.mp4 --trim trim.avs -o OUT.mp4
```

切らずに CM の位置だけを残す場合は `mark` を使います。`-o` はチャプターを足した mp4 と隣の `.edl` を作り、`--edl-only` は `.edl` だけを元の録画の隣に作ります:

```console
$ tachikaze mark IN.mp4 --trim trim.avs -o OUT.mp4
$ tachikaze mark IN.mp4 --trim trim.avs --edl-only
```

複数ファイルはシェルのループに任せます（出力を `*_CMcut.mp4` に固定して glob の再取り込みを避ける）:

```console
$ for f in *.mp4; do
    case "$f" in *_CMcut.mp4) continue;; esac
    tachikaze auto "$f" -o "${f%.mp4}_CMcut.mp4"
  done
```

### ロゴ用 ffmpeg の上書き

`analyze`、`auto`、`make-logo` では、実行環境に合わせてロゴ矩形のデコードに使う ffmpeg の設定を指定できます。

- `--logo-ffmpeg-input-arg=<ARG>` は `-i` より前に渡す引数です。ffmpeg の引数1個につき1回指定し、オプション名と値は別々に渡します。空白を含む値は `--logo-ffmpeg-input-arg='path with spaces'` のように引用します。Tachikaze は受け取った値を順番どおり ffmpeg へ渡します。
- `--logo-ffmpeg-vf=<FILTER>` は `-vf` の値で、1回だけ指定できます。`{x}`、`{y}`、`{w}`、`{h}` はロゴ矩形の座標とサイズに展開されます。全フレームを元の表示順で出力し、各フレームを幅 `{w}`、高さ `{h}` にするフィルターを指定してください。8ビットのグレースケールへの変換は、Tachikaze が `-pix_fmt gray` で指定します。

いずれかを指定すると、ロゴ矩形が決まった後の学習と検出では、上書き設定でそれぞれ全編を1回デコードします。
自動推定では通常、全編デコード1回分の画素列を学習と検出で共有します。再生バッファの見積もりが1 GiBを超える場合は、両方を同じ設定で別々にデコードします。
候補を選ぶための全画面キーフレーム走査には上書き設定を使いません。`--no-logo` との併用はエラーです。

J4125 と i965 で測定・採用した HW-crop 経路は、GPU 上で `crop` と `scale_vaapi` を適用し、矩形の画素を `hwdownload` で転送します。
次のコマンドは J4125 と i965 の環境に固有の構成例であり、Tachikaze の既定値ではありません。
測定時のコマンド文字列は記録に残っておらず、掲載例の文字列そのものは実機で検証していません。
他の GPU やドライバーでの動作や画素の一致は確認していません。iHD では `scale_vaapi` が `VAProfile is not supported` で失敗しました。

```console
$ LIBVA_DRIVER_NAME=i965 tachikaze auto IN.mp4 -o OUT.mp4 \
    --logo-ffmpeg-input-arg=-hwaccel \
    --logo-ffmpeg-input-arg=vaapi \
    --logo-ffmpeg-input-arg=-hwaccel_device \
    --logo-ffmpeg-input-arg=/dev/dri/renderD128 \
    --logo-ffmpeg-input-arg=-hwaccel_output_format \
    --logo-ffmpeg-input-arg=vaapi \
    --logo-ffmpeg-vf='crop={w}:{h}:{x}:{y},scale_vaapi=w={w}:h={h}:format=nv12,hwdownload,format=nv12'
```

Apple Silicon の VideoToolbox は実測でソフトウェア経路より約7〜10倍の時間がかかり、画素も一致しなかったため採用していません。
両環境の測定結果は [docs/measurements.md「VA-API ハードウェアデコードの検証（Issue #164）」](docs/measurements.md#va-api-ハードウェアデコードの検証issue-164) にあります。

## 依存

本体に加え、次の外部ツールが `PATH` 上に必要です。

| ツール | 役割 |
|---|---|
| chapter_exe | 無音・シーンチェンジ検出 |
| join_logo_scp | CM 判定（Trim 生成） |
| dtvindex | 共通フレーム番号の索引 |
| ffmpeg / ffprobe | prepare・検証など |

- macOS でのビルド手順: [docs/toolchain-macos.md](docs/toolchain-macos.md)
- 外部ツールを自分でビルドせず試す: [docs/docker.md](docs/docker.md)（Dockerfile あり。ビルド済みイメージのレジストリ配布はしていません）

## ビルド / インストール

```console
$ cargo build --release --locked
$ make install          # 既定は /usr/local。PREFIX=$HOME/.local も可
```

## 既知の制限

- 動作確認済みの環境は **macOS（Apple Silicon）と Linux（arm64、Docker）**。Windows は未対応
- **映像は H.264**、音声は `mp4-atom` が認識する Codec（代表例 Opus / AAC）
- GOP をキーフレーム境界に丸めるため、**カット境界あたり平均 2.1〜2.5 秒の CM が残る**（許容する方針）
- 1 プロセスにつき入力は 1 本

未対応の入力・構成は [docs/architecture.md](docs/architecture.md)「未対応の入力」を参照してください。

## ドキュメント

詳細な文書索引は **[docs/overview.md](docs/overview.md)** です。目的別のルーティング表があります。

使う人向け:

| 文書 | 内容 |
|---|---|
| [docs/overview.md](docs/overview.md) | 文書索引と重要事実 |
| [docs/architecture.md](docs/architecture.md) | コマンド構成・パス解決・未対応入力（前半が利用者向け） |
| [docs/jls-settings.md](docs/jls-settings.md) | CM 検出が外れたときの調整 |
| [docs/docker.md](docs/docker.md) | Docker で使う方法 |
| [docs/toolchain-macos.md](docs/toolchain-macos.md) | 外部ツールのビルドとインストール |

開発する人向け:

| 文書 | 内容 |
|---|---|
| [docs/pipeline.md](docs/pipeline.md) | 処理の流れと外部ツールの入出力 |
| [docs/lossless-cut.md](docs/lossless-cut.md) | ロスレスカットの実装知識 |
| [docs/mp4-atom.md](docs/mp4-atom.md) | mp4 読み書きの検証済みコードと落とし穴 |
| [docs/measurements.md](docs/measurements.md) | 実測データ |

## ライセンスと帰属

- **Tachikaze 本体**は [MIT](LICENSE) です（Copyright (c) 2026 Masayuki Mizuno）。
- **Amatsukaze 本体は移植していません。** Windows API 依存が大きく、アルゴリズムやパイプラインの**参照元・着想源**として扱っています（[docs/overview.md](docs/overview.md)）。
- **ロゴ検出の一部は Amatsukaze 由来の MIT コードを移植しています。**  
  `src/logo/score.rs` / `src/logo/interval.rs` は Amatsukaze の `LogoScan.hpp` 等に基づく移植で、原典どおり MIT（Copyright (c) 2017-2019 Nekopanda）。著作権表示とライセンス文の保持義務に従っています。
- **ライセンスが不明な delogo 由来のコード（MakKi 氏）は移植していません。** 該当処理は数式から書き下ろしています（詳細は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md)）。
- **実行時に呼ぶ外部ツール**（chapter_exe / join_logo_scp / dtvindex / ffmpeg）は GPL 系が多いですが、**別プロセスとして起動するだけ**で、リンクや同一アドレス空間の共有はありません。したがって本体の MIT には及びません。受け渡しはファイルとコマンドライン引数のみです。

表記の詳細・根拠は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) にまとめています。Docker イメージを配布する場合は外部ツール同梱の条件が別途付くため、現状は Dockerfile の公開にとどめています。
