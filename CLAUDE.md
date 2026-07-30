# Tachikaze

mp4 に変換済みの録画ファイルを、**再エンコードせずに CM カット**するツール。CM 検出は既存ツール（chapter_exe → join_logo_scp）に任せ、本ツールは「Trim リスト → ロスレス出力」だけを担う。

**状態**: **実装完了**（当初のエピック 8 件・サブ issue 31 件はすべてクローズ済み。経緯は `git log` の `[E1-1]`〜`[E8-4]`）。言語は Rust、mp4 の読み書きは `mp4-atom` クレート。

```console
$ tachikaze analyze IN.mp4 -o trim.avs --report --work-dir work
$ tachikaze cut IN.mp4 --trim trim.avs -o OUT.mp4 --dtvi work/work.mp4.dtvi
```

手元のファイルを一通しで処理するときは `scripts/cmcut.sh`（edit list 除去・パス結線・`--cm-output` 付き cut。既定は analyze 後に確認、`--yes` で省略）。

現在のコマンド構成・モジュール構成・自己検証の一覧は
[docs/architecture.md](docs/architecture.md)。

## ドキュメント

**[docs/overview.md](docs/overview.md) に目的別のルーティング表がある。** 必要な文書だけを開くこと。各文書は独立して読める。

```
docs/overview.md            入口。スコープと重要事実
docs/tech-stack.md          技術選定の根拠（却下した選択肢も）
docs/pipeline.md            処理の流れと外部ツールの入出力形式
docs/cm-detection.md        CM 検出の仕組み（背景知識）
docs/jls-settings.md        検出が外れたときの調整と既知の失敗モード
docs/lossless-cut.md        カット処理の実装知識
docs/mp4-atom.md            mp4 読み書きの検証済みコードと落とし穴
docs/toolchain-macos.md     外部ツールのビルド手順
docs/measurements.md        実測データ
docs/architecture.md        現在の構成・自己検証・未対応の入力・未解決事項
```

## テスト

```console
$ cargo test                  # 単体テスト
$ cargo test -- --ignored     # E2E（要 ffmpeg / ffprobe とフィクスチャ）
$ bash tests/fixtures/gen.sh   # フィクスチャ生成（コミットされていない）
```

フィクスチャの**音声は時間変化する信号**にしてある（定常サイン波ではコーデックによって音声パケットが同一バイト列になり、罠 4 を検出できない）。`gen.sh` は Opus 版 `sample.mp4` と AAC 版 `sample_aac.mp4` を同じ映像条件で生成する。映像側のパラメータを変えると `tests/data/sample.dtvi` が使えなくなるので変えないこと。

## 静かに壊れる 4 つの罠

いずれも**エラーを出さずに間違った結果を生む**。コードを書く前に必ず確認すること。

1. **切り出しはパケット数で行う。時間指定（`ffmpeg -t`）は使わない。** B フレームの並べ替え深度ぶん余分に取り込み、表示順に穴が空くのにフレーム数は一見合ってしまう。正しい規則は「S の同期サンプルからデコード順に `E - S` パケット取る」。→ [docs/lossless-cut.md](docs/lossless-cut.md)

2. **無劣化の検証に md5 を使わない。** `h264_mp4toannexb` が IDR ごとに SPS/PPS を再挿入するため、バイト数が一致してもハッシュがずれる。ffprobe の `-show_data_hash CRC32` でパケット単位に比較する。→ [docs/lossless-cut.md](docs/lossless-cut.md)

3. **表示順とデコード順を型で分ける。** 混同が唯一の重大バグ源で、間違った位置で切っても例外は飛ばない。`.dtvi` のフレーム番号と自前導出の一致を assert する。→ [docs/architecture.md](docs/architecture.md)

4. **音声パケットは「区間先頭サンプルのソース上の DTS」から引き当てる。** 出力側の累積時間を起点に詰めると**長さは合うのに中身が別の位置の音声になる**。合成時刻（pts）を使うと `cts_offset`（並べ替え深度 = 実測 67ms）ぶん**音声が映像より先行する**。どちらも実際に起きた。パケットの集合比較も区間ごとの長さ比較も素通りするので、「元ファイルと出力で『映像 pts − 音声 pts』が保たれているか」を見る検査が必要。→ [docs/lossless-cut.md](docs/lossless-cut.md)

## ドキュメントと issue を最新に保つ

**コードを変えたら同じ変更でドキュメントを直す。** 実装が全部終わった後も「未実装」「〜すべき」が複数残っていた実例がある（`stts`/`ctts` の圧縮、co64、見逃し候補の警告はいずれも実装済みなのに「本実装で直すべき点」として残っていた）。次に読む人が既にあるものを作りかける。

- **ドキュメントとコードが食い違ったらコードが正。** 直すのはドキュメント側。ただし「なぜそうしたか」はコードに書けないので、判断の根拠だけはドキュメントに残す
- **「未実装」「未検証」「〜すべき」と書いた行は、実装したら消す。** 残す価値があるのは「**なぜ実装しないと決めたか**」（[docs/architecture.md](docs/architecture.md) の「未対応の入力」がその形）
- **実測値は歴史的記録なので消さない。** ただし**その測定で何を見ていなかったか**を併記する（「合計値しか見ていない」と書いてあったおかげで音声の位置ずれの原因を特定できた）
- **静かに壊れた実例は再現手順ごと残す**（[docs/lossless-cut.md](docs/lossless-cut.md) の「実際に起きた誤り」）。上の「罠」の節に足すのは**実際に壊れたもの**だけにする。仮説は足さない
- **新しく判明した未対応事項は [docs/architecture.md](docs/architecture.md) の「未解決事項」に足す。** issue は実装が終わったらクローズし、方針を変えたときは該当コードの doc comment とドキュメントの両方に書く
- **ドキュメントを改名・移動したら `grep -rn '<旧ファイル名>' --include='*.rs' --include='*.md'` で参照を直す。** `src/` のコメントがドキュメントを節名つきで参照している箇所がある

**実測** / **未検証** の使い分けは [docs/overview.md](docs/overview.md) の「記述の約束」。

## 前提

- 映像は H.264、音声は `mp4-atom` が認識する音声 Codec 全般（代表例 Opus / AAC=`Mp4a`。判定は `src/mp4io/read.rs::is_audio_codec`、対応一覧と検証状況は [docs/mp4-atom.md](docs/mp4-atom.md)）。GOP は 120 フレーム固定でシーンチェンジ由来の IDR なし
- キーフレーム境界に丸めるため**カット境界あたり平均 2.1〜2.5 秒の CM が残る**。これは許容する方針で決定済み
- 開発・実行は macOS arm64。Amatsukaze 本体は移植しない（Windows API を 500 箇所以上使用）
