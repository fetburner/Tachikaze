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
$ tachikaze cut IN.mp4 --trim trim.avs -o OUT.mp4
```

複数ファイルはシェルのループに任せます（出力を `*_CMcut.mp4` に固定して glob の再取り込みを避ける）:

```console
$ for f in *.mp4; do
    case "$f" in *_CMcut.mp4) continue;; esac
    tachikaze auto "$f" -o "${f%.mp4}_CMcut.mp4"
  done
```

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
