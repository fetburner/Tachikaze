# Tachikaze

mp4 に変換済みの録画ファイルを、**再エンコードせずに CM カット**するツール。CM 検出は既存ツール（chapter_exe → join_logo_scp）に任せ、本ツールは「Trim リスト → ロスレス出力」だけを担う。

**状態**: 設計と実現可能性の検証は完了。実装は未着手。言語は Rust、mp4 の読み書きは `mp4-atom` クレート。

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
docs/implementation-plan.md 実装計画と未解決事項
```

## 静かに壊れる 3 つの罠

いずれも**エラーを出さずに間違った結果を生む**。コードを書く前に必ず確認すること。

1. **切り出しはパケット数で行う。時間指定（`ffmpeg -t`）は使わない。** B フレームの並べ替え深度ぶん余分に取り込み、表示順に穴が空くのにフレーム数は一見合ってしまう。正しい規則は「S の同期サンプルからデコード順に `E - S` パケット取る」。→ [docs/lossless-cut.md](docs/lossless-cut.md)

2. **無劣化の検証に md5 を使わない。** `h264_mp4toannexb` が IDR ごとに SPS/PPS を再挿入するため、バイト数が一致してもハッシュがずれる。ffprobe の `-show_data_hash CRC32` でパケット単位に比較する。→ [docs/lossless-cut.md](docs/lossless-cut.md)

3. **表示順とデコード順を型で分ける。** 混同が唯一の重大バグ源で、間違った位置で切っても例外は飛ばない。`.dtvi` のフレーム番号と自前導出の一致を assert する。→ [docs/implementation-plan.md](docs/implementation-plan.md)

## 前提

- 対象ファイルは H.264 + **Opus**（AAC ではない）、GOP は 120 フレーム固定でシーンチェンジ由来の IDR なし
- キーフレーム境界に丸めるため**カット境界あたり平均 2.1〜2.5 秒の CM が残る**。これは許容する方針で決定済み
- 開発・実行は macOS arm64。Amatsukaze 本体は移植しない（Windows API を 500 箇所以上使用）
