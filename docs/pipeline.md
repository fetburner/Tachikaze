# 処理パイプラインと外部ツールの入出力形式

→ 入口: [overview.md](overview.md)

## 全体像

```
IN.mp4
  │
  ├─ dtvindex build IN.mp4 -o IN.dtvi        ← 共通フレーム番号の索引
  │
  ├─ chapter_exe -v IN.mp4 -o scp.txt        ← 無音 + シーンチェンジ検出
  │     （FFmpeg 直接入力モード。AviSynth は不要）
  │
  ├─ join_logo_scp -inscp scp.txt -incmd JL_標準.txt \
  │       -o trim.avs -oscp detail.jls \
  │       -set autocm_sub 11 -set param_cuttr 1     ← CM 判定
  │
  │   （必要なら trim.avs を人手で修正）
  │
  └─ [本ツール] trim.avs + IN.mp4 → OUT.mp4  ← ロスレスカット
```

上流 3 つは既存ツール。本ツールが実装するのは最後の 1 段と、全体の起動・検証のみ。

## AviSynth が不要な理由

chapter_exe の Linux/macOS 版には **FFmpeg 直接入力モード**（dtvindex 経由）がある。README に実測比較が記載されている:

> MPEG-2 放送 TS 3 本を AviSynth+／L-SMASH Works 入力と比較した場合、無音・シーンチェンジ位置は ±30 フレームの範囲で 85.2% から 95.5% が対応し、最終フレーム番号の差は 0 から 3 フレームだった。**Logoframe および JoinLogoScp まで組み合わせた最終 Trim は、放送 TS 5 本すべてで AviSynth+ 入力と同一になった。**

中間の検出位置は多少ずれるが最終 Trim は一致する。macOS arm64 に AviSynth+ を用意する必要がない。

## 各ツールの入出力形式

すべてテキストで、**単位はフレーム番号**。

### chapter_exe の `-o` 出力（= join_logo_scp の `-inscp`）

```
CHAPTER01=00:00:01.602
CHAPTER01NAME=28フレーム  SCPos:63 62
CHAPTER02=00:00:09.343
CHAPTER02NAME=42フレーム ＿ SCPos:320 319
CHAPTER06=00:03:01.581
CHAPTER06NAME=30フレーム ★ SCPos:5458 5457
```

- `SCPos:<フレーム番号>` が無音シーンチェンジ位置
- 先頭の「Nフレーム」は無音の長さ
- `★` / `＿` / `＠` は候補の強さを示す印。`★` は 15 秒間隔で並んでいる候補（＝ CM の可能性が高い）

Amatsukaze 側では `-o` ではなく **stdout** を別に解析している（`CMAnalyze.hpp:329` で stdout をファイルに落とし、`CMAnalyze.hpp:411-439` で `mute N: a - b` と `SCPos: N` を正規表現で抽出）。本ツールでは `-o` 出力を join_logo_scp に渡すだけでよい。

### logoframe の出力（= join_logo_scp の `-inlogo`）

**本ツールでは使わない**（delogo 済みのため）。形式は参考として記録する。Amatsukaze の `LogoScan.hpp:1818` が生成:

```
  1234 S 0 ALL   1220   1250     ← ロゴ開始: 最良位置 1234、可能性範囲 1220〜1250
  8765 E 0 ALL   8750   8780     ← ロゴ終了: 最良位置 8765、可能性範囲 8750〜8780
```

最良位置だけでなく**可能性の範囲**を渡すのが要点。join_logo_scp 側は `rise` / `rise_l` / `rise_r` / `fall` / `fall_l` / `fall_r` として受け取る（`JlsDataset::displayLogo()` のデバッグ出力で確認できる）。

`-inlogo` を省略すると join_logo_scp は**全フレームがロゴ表示中とみなす**。よって `:L` ラベルは「ロゴあり」ではなく「情報なし」を意味する。

### join_logo_scp の `-o` 出力（Trim）

```
Trim(66,34201) ++ Trim(37798,53591) ++ Trim(57189,70974)
```

- `Trim(s,e)` は **両端含む**フレーム範囲。半開区間に直すと `[s, e+1)`
- Amatsukaze の実装は `CMAnalyze.hpp:377` の `readTrimAVS`（正規表現 1 本、終端に +1）

### join_logo_scp の `-oscp` 出力（detail.jls）

```
開始   終了  秒数 誤差 ロゴ秒 ラベル
   0    73    2   14    0 :Nologo
  74  6127   15    0   15 :L
6128  6577   15    0    0 :CM
```

列の意味（readme.txt に記載）:

1. 単位フレーム開始位置
2. 単位フレーム終了位置
3. 期間（秒数）
4. **期間秒数からの誤差（フレーム数）** ← 15 秒格子にどれだけ乗っているか。小さいほど確信度が高い
5. 期間内のロゴ表示期間（秒数）
6. 推測した構成（ラベル）

ラベルの一覧は [cm-detection.md](cm-detection.md) を参照。

### dtvindex の `.dtvi`

**UTF-8 タブ区切りテキスト。** ヘッダ + `FRAMES` マーカー + フレーム行。1 行あたり:

```
frame_number  sample_number  random_access_sample  file_offset  pts  dts  duration  flags
```

- `frame_number`: 0 始まりの**表示順**
- `sample_number`: 0 始まりの**デコード順**
- `random_access_sample`: デコード順で直前のキーパケット
- flags: `0x01` キーパケット / `0x02` 先行提示サンプル / `0x04` 有効 PTS / `0x08` 有効 DTS / `0x10` 有効バイト位置 / `0x20` 最近傍キーパケットより前の RAP が必要

仕様は dtvindex の `docs/index-format-v1.md`。**Rust から直接パースできる**（依存ゼロ）。

CLI からも引ける:

```console
$ dtvindex seek-plan IN.dtvi 100
target_frame              100
target_sample             102     ← 表示順 → デコード順
random_access_sample      75      ← 直前のキーフレーム
packets_to_submit         28
random_access_file_offset 18358
target_is_leading         0
```

## dtvindex の位置づけ

表示順↔デコード順と GOP 境界そのものは、`mp4-atom` の `stss`（同期サンプル）と `ctts`（合成オフセット）から**自力で導出できる**（`src/mp4io/order_map.rs`）。それでも **`.dtvi` は必須にしている**（`cut --dtvi` を省略した場合も、`.dtvi` 無しで動くわけではなくキャッシュから自動的に見つける。[architecture.md](architecture.md)「パス解決」節）。理由は 2 つ:

1. **整合性チェック**: `.dtvi` のフレーム番号と自分の導出結果が一致するかを assert する。フレーム番号の解釈がずれると**エラーを出さずに間違った位置で切る**ため、これが唯一の実効的な防御になる（`order_map.rs::verify_against_dtvi`）
2. **オープン GOP の判定**: `leading_frame_count` を見る手段が `.dtvi` 以外にない。判定できないまま処理すると「パケット数 == フレーム数」規則が静かに破れるので、`.dtvi` が無い場合は**チェックをスキップして警告ではなく明示エラーで停止**する（`support.rs::check_closed_gop`）

`analyze` が `dtvindex build` を走らせるので、既定（入力ごとの XDG キャッシュディレクトリ）のままでも `work.mp4.dtvi` が残り、`cut` から自動的に見つかる。

## Amatsukaze 側の対応実装（アルゴリズム参照用）

移植はしないが、同等処理の参照先として:

| 処理 | 場所 |
|---|---|
| Trim パース | `CMAnalyze.hpp:377` |
| シーンチェンジ抽出 | `CMAnalyze.hpp:411-439` |
| チャプター生成 | `CMAnalyze.hpp:462-679`（`MakeChapter`） |
| フレーム番号の写像 | `CMAnalyze.hpp:604`（`lower_bound` で切り詰め後の番号へ変換） |
| 音声の同期追従 | `StreamReform.hpp:1287-1420`（`fillAudioFrames`） |
| Mux コマンド組み立て | `TranscodeSetting.hpp:263`（`makeMuxerArgs`） |
