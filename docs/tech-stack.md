# 技術選定

→ 入口: [overview.md](overview.md)

## 決定

| 項目 | 選択 |
|---|---|
| 言語 | **Rust** |
| mp4 読み書き | **`mp4-atom` クレート**（純 Rust、FFI なし） |
| 外部ツール | サブプロセスとして起動（chapter_exe / join_logo_scp / dtvindex） |
| 配布 | 単一静的バイナリ |

FFmpeg へのリンク、C++ への FFI はいずれも不要。

## Rust を選んだ理由

1. **クロスプラットフォームが要件。** 開発・実行は macOS arm64、将来的に Windows。
2. **仕事の内容が向いている。** バイナリのパース（サンプルテーブル、`avcC`）と、表示順↔デコード順の管理が中心。この2つを別の型にして混同をコンパイルエラーにできる利点が大きい（混同は静かに壊れるバグの主要因）。
3. **配布が既存の作法に合う。** 外部ツール群と同じディレクトリにバイナリを1つ置くだけで済む。

## mp4-atom で FFI なしに完結する理由

本ツールはコーデックを理解する必要がなく、**`stsd` を不透明なバイト列としてコピーしたいだけ**。`mp4-atom` は次を満たすため、mp4 のサンプル単位 read/write を純 Rust で書ける（C++ ライブラリや libav への FFI は要らない）:

- `Codec::Opus`（`dOps`）/ `Codec::Mp4a`（AAC）を含む主要コーデックを網羅（認識する音声 Codec 全般を同じカット経路で扱う。対応一覧は [mp4-atom.md](mp4-atom.md)）
- `Any::Unknown(FourCC, Vec<u8>)` で未知アトムを不透明なバイト列として保持
- `stsz` / `stsc` / `stco` / `stts` / `ctts` / `stss` をすべて公開

詳細と検証済みコードは [mp4-atom.md](mp4-atom.md)。

## 却下した選択肢

### `mp4` クレート（v0.14）— Opus を書き出せない

`MediaConfig` が**閉じた列挙**で、`AvcConfig` / `HevcConfig` / `Vp9Config` / `AacConfig` / `TtxtConfig` しかない。実ファイルの Opus 音声トラックは `media_type()` が `Err(InvalidData("unsupported media type"))` を返す（読み取り自体は可能でサンプル数は正しく取れる）。

**根本的な設計の不一致**: このクレートは「既知のコーデックを理解する」モデルだが、本ツールはコーデックを理解する必要がなく、`stsd` を不透明なバイト列としてコピーしたいだけ。

### C# — Windows 専用

Amatsukaze の GUI / Server は C#（`AmatsukazeServer.csproj` の `TargetFrameworkVersion` = **v4.5**）。WPF + Livet + .NET Framework 4.5 で macOS では動かない。.NET 8 + Avalonia への書き換えは別プロジェクトの規模。

なお Amatsukaze の C# 側は `AmatsukazeCLI.exe` をサブプロセスとして起動する構成（`AmatsukazeServer/Server/EncodeServer.cs`）なので、**別言語のツールを足すこと自体は既存の作法**である。

### Go — 悪くないが Rust で足りた

`Eyevinn/mp4ff` は ISO BMFF のボックス操作で実績豊富、cgo 不要で単一バイナリ。`mp4-atom` が要件を満たしたため Rust を維持。**mp4-atom が行き詰まった場合の第一候補**。

### C++ — dtvindex 直結の利点はあったが不要

dtvindex は C++ 静的ライブラリ（`VideoReader` / `Index` / `FrameRecord` / `TrimPlan` / チャプター生成の API を持つ）で、C++ なら FFI ゼロで使える。ただし

- カット処理に dtvindex は不要（`mp4-atom` の `stss` / `ctts` から自力で導出可能）
- Amatsukaze 本体とは別に「Windows API を使わない移植可能な C++」を書く前提が必要

なため採用せず。

### libav\* への FFI（`rusty_ffmpeg` / `ffmpeg-sys-next`）— 不要

ffmpeg の mov マルチプレクサは Opus を問題なく扱える（検証で実際に使用）。しかし `mp4-atom` で完結したため、native 依存とバージョン固定の負担を負う理由がない。

### SML# — 看板機能に的がない

SML# の売りは **C との直接 FFI**。しかし FFI を使わない設計に着地したため最大の利点が活きない。加えて

- FFI は C 向けであり、dtvindex の公開 API は C++（`std::string` / `std::vector` / クラス）なので `extern "C"` のシムが必要。Rust から呼ぶのと同じ手間
- mp4 ライブラリが存在せず、ISO BMFF を手書きする必要がある
- Homebrew に formula がなく、Apple Silicon 対応状況は**未検証**
- Windows 対応が厳しい

**ただし着眼点は有効**: join_logo_scp の作り直しなら ML 系は真面目な候補。独自スクリプト言語のインタプリタ＋構成分類器で、代数的データ型とパターンマッチが向く題材。→ [cm-detection.md](cm-detection.md)

## 依存クレート

| クレート | 用途 |
|---|---|
| `mp4-atom` 0.14 | mp4 のアトム読み書き（**実ファイルで検証済み**、[mp4-atom.md](mp4-atom.md)） |
| `clap` 4.6（`derive`） | サブコマンドと引数のパース |
| `anyhow` 1.0 | エラーの伝播（`Context` で経路情報を足す） |
| `serde` 1.0（`derive`） / `serde_json` 1.0 | `SegmentMap`（`src/segmap.rs`）の JSON 読み書き |

`serde` / `serde_json` は `remap-subs` が `cut` の書いた区間マップを読み戻すために使う。依存はこの5つだけ。**増やすときは代替手段がないことを確認すること**（ロスレスカットの処理は標準ライブラリと `mp4-atom` で足りている）。

外部プロセスとして呼ぶもの（クレート依存ではない）: `chapter_exe` / `join_logo_scp` / `dtvindex` /（検証用に）`ffprobe`。
