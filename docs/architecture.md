# 構成と未解決事項

→ 入口: [overview.md](overview.md)

**実装は完了している。** 当初エピック 8 件・サブ issue 31 件に分解したタスクはすべてクローズ済み（経緯は `git log` の `[E1-1]`〜`[E8-4]`、および GitHub issue #3〜#41）。この文書は**現在の構成**と、**まだ対応していないこと**を書く。

## コマンド構成

**解析とカットは別のコマンドに分かれている。** 検出の見逃しが実際に起きるため（[jls-settings.md](jls-settings.md)）、目視確認と手動修正を挟めるようにしてある。

```
tachikaze analyze IN.mp4 -o trim.avs [--report] [--work-dir DIR]
                                     [--jls-set KEY=VALUE]... [--jl-file FILE]
    1. dtvindex build              （外部プロセス）
    2. chapter_exe                 （外部プロセス）
    3. join_logo_scp               （外部プロセス）
                                   -set autocm_sub 11 -set param_cuttr 1 を既定付与
    4. --report で以下を出力
       ・各カット境界とキーフレームの距離
       ・余分に残る合計秒数
       ・見逃し候補の警告（既知の CM ブロック長と一致する未カット区間）

（必要なら trim.avs を人手で編集）

tachikaze cut IN.mp4 --trim trim.avs -o OUT.mp4 --dtvi work.mp4.dtvi
                                     [--snap outward|inward] [--cm-output CM.mp4]
                                     [--video-only] [--verify]
    5. mp4-atom でサンプル表を読み、表示順↔デコード順を導出
       → .dtvi と一致するか assert（不一致なら停止）
    6. Trim をキーフレーム境界へスナップ（既定 outward）
    7. 映像: デコード順に (E-S) サンプルを選択
    8. 音声: 区間ごとに、区間先頭サンプルのソース上の DTS から最近傍の音声パケットを引き当てる
    9. mp4-atom で書き出し（stsd は clone、サンプル表のみ再構築）
   10. 自己検証
   11. --cm-output 指定時は保持区間の補集合について 7〜10 を繰り返す
       （補集合の両端も同期サンプル上に来るので追加のスナップは不要）
       + 保持側と CM 側でフレーム数の合計 == 総フレーム数 / 集合が互いに素 を assert
```

`--dtvi` は必須。オープン GOP の判定（[lossless-cut.md](lossless-cut.md)）と自己検証 4（表示順/デコード順の突き合わせ）に使う。`analyze --work-dir DIR` を使うと `DIR/work.mp4.dtvi` に残る。

一括実行用の `tachikaze auto` は用意していない。

## モジュール構成

**この分割はファイル所有権の単位でもある。** 1 つの関心事が 1 ファイルに収まるように決めてあるので、統合・分割するときは理由を持って行うこと。

| ファイル | 責務 | 参照 |
|---|---|---|
| `cli.rs` | サブコマンドとオプションの定義 | — |
| `commands.rs` | 各モジュールを繋ぐ組み立て（アルゴリズムは持たない） | — |
| `tools.rs` | 外部ツールと JL ファイルの探索 | [toolchain-macos.md](toolchain-macos.md) |
| `external.rs` | 外部プロセスの起動と出力の回収 | [pipeline.md](pipeline.md) |
| `workdir.rs` | 作業ディレクトリと symlink | [toolchain-macos.md](toolchain-macos.md) |
| `order.rs` | `DisplayIdx` / `DecodeIdx` | 下記「型設計の要点」 |
| `trim.rs` | `Trim(a,b)++…` のパース / 生成（半開区間 `[s, e+1)` に正規化） | — |
| `dtvi.rs` | `.dtvi` のパース（タブ区切りテキスト） | [pipeline.md](pipeline.md) |
| `jls.rs` | `detail.jls` のパース | [pipeline.md](pipeline.md) |
| `analyze.rs` | analyze コマンドの組み立て | [jls-settings.md](jls-settings.md) |
| `report/mod.rs` | `--report` の出力 | [measurements.md](measurements.md) |
| `report/missed.rs` | 見逃し候補の警告 | [jls-settings.md](jls-settings.md) |
| `mp4io/read.rs` | サンプル表の読み込み | [mp4-atom.md](mp4-atom.md) |
| `mp4io/order_map.rs` | 表示順↔デコード順の写像、合成時刻の算出 | [pipeline.md](pipeline.md) |
| `mp4io/support.rs` | 未対応構成の判定（下記「未対応の入力」を早期に落とす） | 下記 |
| `mp4io/write.rs` | mp4 の書き出し（stts/ctts の圧縮、インターリーブ、co64） | [mp4-atom.md](mp4-atom.md) |
| `plan.rs` | キーフレーム境界へのスナップ、区間の計画、保持区間の補集合 | [lossless-cut.md](lossless-cut.md) |
| `audio.rs` | 音声パケットの選択（区間のソース時刻から引き当て） | [lossless-cut.md](lossless-cut.md) |
| `verify.rs` | 自己検証 | 下記 |

**解析側（analyze）は mp4 の読み込みに依存しない。** `--report` が必要とするキーフレーム位置を `.dtvi` から取る設計にしてあるため。**この性質を崩さないこと**（キーフレーム位置を mp4 から取る実装に変えると解析とカットが結合する）。

## 型設計の要点

**表示順とデコード順は別の型。** この 2 つの混同が唯一の重大バグ源であり、混同するとエラーを出さずに間違った位置で切る。

```rust
struct DisplayIdx(u32);   // 表示順（Trim / .dtvi の frame_number）
struct DecodeIdx(u32);    // デコード順（mp4 のサンプル番号 / .dtvi の sample_number）
```

相互変換は明示的な関数だけを通す（`OrderMap::to_display` / `to_decode`）。

## 自己検証（手順 10）

区間ごとに以下を assert し、1 つでも失敗したら**出力を破棄して停止**する:

1. 映像パケット数 == `E - S`
2. 表示順（pts 昇順）に欠落がない
3. 先頭パケットが同期サンプル
4. `.dtvi` のフレーム番号と自前導出が一致
5. **音声が正しい位置から取れている**（区間先頭の音声パケットがソースの `dts_src(S)` 近傍のパケットと一致し、かつ元ファイルと出力で「映像 pts − 音声 pts」が保たれている）

   長さの一致とパケットの集合比較だけでは**中身が別の位置の音声でも通ってしまう**。実際に 2 回それで壊れた（[lossless-cut.md](lossless-cut.md)「実際に起きた誤り 1 / 2」）。**期待値を実装と同じ式で計算する検査は、式自体が間違っているときに無力**なので、元ファイルとの pts 関係を見る検査を持つ。

加えて全体で:

6. 音声の丸め誤差の最大値をログ出力（`AudioDiffInfo` 相当）
7. `--verify` 指定時は ffprobe のパケット単位 CRC32 で元ファイルとの一致を確認（[lossless-cut.md](lossless-cut.md)）
8. `--cm-output` 指定時は保持側と CM 側でフレーム数の合計 == 総フレーム数、`DecodeIdx` の集合が互いに素

## 未対応の入力（`mp4io/support.rs` が明示エラーで落とす）

**いずれも「静かに壊れる」より「止まる」を選んでいる。** 音声コーデック自体は `mp4-atom` が認識する音声 Codec 全般（Opus / AAC など）を同じ経路で扱う。ここに残すのはコーデックと無関係に未対応と判断した構成。

| 構成 | 判定関数 | 理由と回避策 |
|---|---|---|
| `elst`（edit list）あり | `check_no_edit_list` | `moov.clone()` で引き継いだ `elst` の `segment_duration` がカット後のトラック長を超え、`media_time` が新しい先頭の正当なフレームを数フレーム分スキップする——「エラーは出ないが結果が壊れる」の実例を実験で確認済み。**回避策**: `ffmpeg -i IN.mp4 -c copy -use_editlist 0 -movflags +faststart OUT.mp4`（除去後の映像・音声パケットが CRC32 でビット一致することを確認済み） |
| `stsd` が複数エントリ | `check_single_stsd_entry` | `write.rs` が `sample_description_index: 1` 固定で `stsc` を再構築しているため。対応にはサンプルごとの index 保持（`read.rs` の `SampleInfo` と `write.rs` の両方の変更）が必要。無劣化 remux ではパラメータ差異という原因自体を解消できないので、事前除去の回避策は提示できない |
| オープン GOP | `check_closed_gop` | 「S の同期サンプルからデコード順に `E - S` パケット取る」規則が成立しない。`.dtvi` の `leading_frame_count` で判定する（`.dtvi` が無い場合も判定不能として停止） |
| 映像 2 本以上 / 音声 2 本以上 / 字幕などのトラックあり | `check_track_counts` | 映像 1 本 + 音声 1 本のみ対応 |

## 未解決事項

| 項目 | 状況 |
|---|---|
| 複数音声トラック / 字幕トラック | **未対応**（上表のとおり明示エラー）。対応するならトラックごとの区間選択と、字幕のようにサンプルが疎なトラックの扱いを決める必要がある |
| 継ぎ目の MDCT 過渡（クリックノイズ） | **許容する方針で決定済み。** 継ぎ目は残存 CM マージンの内側に来る（[lossless-cut.md](lossless-cut.md)） |
| `--snap inward` と `--cm-output` の併用 | **拒否する。** inward では保持区間が退化（`end < start`）しうるため補集合の順序も壊れる |

## 方針として作らないもの

| 項目 | 理由 |
|---|---|
| CM 検出アルゴリズム | 既存ツール（chapter_exe → join_logo_scp）が担当 |
| ロゴ検出 | delogo 済み mp4 では原理的に不可 |
| 映像の再エンコード | 数秒の CM 残りを許容する方針 |
| 音声の再エンコード | 継ぎ目のノイズは残存 CM の範囲内 |

**ただし「境界 GOP だけ再エンコードすればフレーム精度になる」ことは覚えておく。** 実測で境界あたり 4 秒（120 フレーム）分のエンコードで済む。30 分番組・8 境界なら 32 秒分。方針が変わったときの逃げ道として安い。

## 将来的な拡張候補

- **バッチ処理**: ディレクトリを指定して一括処理
- **チャプター出力**: dtvindex に `create_join_logo_scp_chapters` があるのでそれを呼ぶだけ
- **局別 JL ファイルの選択**: 現状は `JL_標準.txt` 既定（`--jl-file` で差し替えは可能）
