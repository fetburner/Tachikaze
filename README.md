# Tachikaze

mp4 に変換済みの録画ファイルを、**再エンコードせずに CM カット**するツールです。

CM 検出は既存ツール（chapter_exe → join_logo_scp）に任せ、本ツールは「Trim リスト → ロスレス出力」を担います。

## 最短の使い方

```console
$ tachikaze auto IN.mp4 -o OUT.mp4
```

`auto` は `prepare` → `analyze` → gate → `cut` → `remap-subs` を対話なしで実行します。gate が疑わしいと判定した場合は cut せずに終了します（`--ignore-gate` で無視可）。

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

## ビルド / インストール

```console
$ cargo build --release --locked
$ make install          # 既定は /usr/local。PREFIX=$HOME/.local も可
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

## 既知の制限

- **映像は H.264**、音声は `mp4-atom` が認識する Codec（代表例 Opus / AAC）
- GOP をキーフレーム境界に丸めるため、**カット境界あたり平均 2.1〜2.5 秒の CM が残る**（許容する方針）
- 1 プロセスにつき入力は 1 本

未対応の入力・構成は [docs/architecture.md](docs/architecture.md) を参照してください。

## ドキュメント

詳細な入口は **[docs/overview.md](docs/overview.md)** です。目的別のルーティング表があります。

| 文書 | 内容 |
|---|---|
| [docs/overview.md](docs/overview.md) | スコープと重要事実 |
| [docs/architecture.md](docs/architecture.md) | コマンド構成・パス解決・未対応入力 |
| [docs/pipeline.md](docs/pipeline.md) | 処理の流れと外部ツールの入出力 |
| [docs/docker.md](docs/docker.md) | Docker で使う方法 |
| [docs/toolchain-macos.md](docs/toolchain-macos.md) | 外部ツールのビルドとインストール |

## ライセンス

本体は [MIT](LICENSE) です。外部ツール・移植コードの表記は [THIRD-PARTY-NOTICES.md](THIRD-PARTY-NOTICES.md) を参照してください（GPL の外部ツールは子プロセスとして起動するだけです）。
