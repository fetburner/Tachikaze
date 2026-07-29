# Tachikaze — 概要と文書索引

mp4 に変換済みの録画ファイルを、**再エンコードせずに CM カット**するツール。

Amatsukaze は MPEG2-TS を入力して CM カットとエンコードを行い mp4 を出力するが、一度 mp4 にしてしまったファイルは扱えない。そのまま録画してしまったファイルを後から CM カットしたい、という需要に応える。

**状態**: 実装完了（`analyze` / `cut` の両コマンドが動作し、E2E テストでビット一致を確認済み）。残っている未対応構成は [architecture.md](architecture.md) に一覧がある。

**このファイルは入口です。必要な文書だけを開いてください。** 各文書は独立して読めるように書かれています。

## 目的別ルーティング

| やりたいこと | 読む文書 |
|---|---|
| なぜ Rust / mp4-atom なのか、他をなぜ却下したか | [tech-stack.md](tech-stack.md) |
| 処理の流れと外部ツールの入出力形式を知る | [pipeline.md](pipeline.md) |
| CM 検出が外れる・当たらない原因を調べる | [jls-settings.md](jls-settings.md) |
| CM 検出の仕組みを理解する（背景知識） | [cm-detection.md](cm-detection.md) |
| カット処理を実装する / バグを直す | [lossless-cut.md](lossless-cut.md) |
| mp4 の読み書きコードを書く | [mp4-atom.md](mp4-atom.md) |
| 外部ツールを macOS でビルドする | [toolchain-macos.md](toolchain-macos.md) |
| 実測値（GOP 長・カット精度・検出品質）を見る | [measurements.md](measurements.md) |
| 現在の構成・自己検証・未対応の入力・未解決事項を知る | [architecture.md](architecture.md) |

## スコープ

**作るもの**

- 外部ツール（chapter_exe → join_logo_scp）のオーケストレーション
- Trim リストをキーフレーム境界へ丸める処理
- mp4 のサンプル単位コピーによるロスレスカット
- 音声パケットの区間選択とドリフト補正
- **CM 側（除去した区間）の別ファイル出力**（検出が切りすぎていないかの目視確認用）
- 自己検証（パケット数・表示順の連続性・同期サンプル）

**作らないもの**

| 項目 | 理由 |
|---|---|
| CM 検出アルゴリズム | chapter_exe + join_logo_scp が担当 |
| ロゴ検出 | delogo 済み mp4 では原理的に使えない |
| 映像の再エンコード | 数秒の CM 残りを許容する方針のため不要 |
| 音声の再エンコード | 継ぎ目のノイズは残存 CM の範囲に収まるため無視可 |
| チャプター生成 | dtvindex に実装済み（`create_join_logo_scp_chapters`） |

## 最初に知っておくべき 6 つの事実

他の文書を開かずに済むよう、ここに集約しておきます。

1. **CM 検出は既存ツールに任せる。** 自作するのは「Trim リスト → ロスレス出力」だけ。解析側は macOS でビルド・動作確認済み。

2. **対象ファイルの GOP は 4.004 秒（120 フレーム）完全固定**で、シーンチェンジ由来の IDR が 1 つも存在しない（3 ファイルで実測）。よってキーフレーム境界に丸めると**カット境界あたり平均 2.1〜2.5 秒**の CM が残る。**これは許容する方針**で決定済み。

3. **音声は Opus**（AAC ではない）。`mp4` クレートは Opus を書き出せないため `mp4-atom` を使う。

4. **切り出しは必ずパケット数で行う。** 時間指定（`ffmpeg -t`）は B フレームの並べ替え深度ぶん余分に取り込み、**表示順に穴が空くのにエラーが出ない**。→ [lossless-cut.md](lossless-cut.md)

5. **無劣化の検証に md5 を使ってはいけない。** ffprobe の `-show_data_hash CRC32` によるパケット単位比較を使う。→ [lossless-cut.md](lossless-cut.md)

6. **音声パケットは区間のソース上の絶対時刻から引き当てる。** 出力側の累積時間を起点に詰めると、**長さは合うのに中身が別の位置の音声になる**（実際に起きた。既存の集合比較・長さ比較はどちらも素通りする）。→ [lossless-cut.md](lossless-cut.md)

## 参照する外部リポジトリ

| リポジトリ | 役割 |
|---|---|
| [nekopanda/Amatsukaze](https://github.com/nekopanda/Amatsukaze) | 元となった TS 用ツール。**アルゴリズムの参照元であり移植対象ではない**（Windows API を 500 箇所以上使用） |
| [tobitti0/chapter_exe](https://github.com/tobitti0/chapter_exe) | 無音・シーンチェンジ検出 |
| [tobitti0/join_logo_scp](https://github.com/tobitti0/join_logo_scp) | CM 判定（Trim 生成） |
| [tobitti0/dtvindex](https://github.com/tobitti0/dtvindex) | 共通フレーム番号の索引・Trim/jls パーサ・チャプター生成 |

## 記述の約束

- **実測** とある数値は実ファイルで確認済み。
- **未検証** とある箇所は推測。鵜呑みにせず確認すること。
- `ファイル名:行番号` は上表リポジトリ内の位置。行番号は記録時点のもので、ずれている可能性がある。
