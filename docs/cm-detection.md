# CM 検出の仕組み（背景知識）

→ 入口: [overview.md](overview.md) / 実務的な調整: [jls-settings.md](jls-settings.md)

**この文書は理解のための背景知識。** 検出が外れたときの対処は [jls-settings.md](jls-settings.md) を先に読むこと。

## 全体構造

CM 判定は 3 種類の「証拠」を集めて外部ツールが統合判定する分業になっている。

```
                ┌─ ロゴの有無     … tachikaze 自前実装 (src/logo/) ─┐
映像・音声 ─────┼─ 無音区間       … chapter_exe                     ├→ join_logo_scp
                └─ シーンチェンジ … chapter_exe                     ─┘      ↓
                  ＋ PMT 変更点   … Amatsukaze 本体 (applyPmtCut)       Trim(a,b)++…
```

**本プロジェクトは PMT を使わない**（mp4 に PMT が無いため）。**ロゴ検出は自前実装している**（`analyze --logo <path.lgd>` を指定したときだけ使う。省略時はロゴ無しの経路になる）。対象の mp4 には局ロゴが残っていることが多い（[measurements.md](measurements.md)「ロゴの残存」）。

## ロゴ検出（自前実装、任意）

日本の地上波は本編中は局ロゴが出て CM 中は消えるため、これが最も強い手がかりになる。**移植元は Amatsukaze 自前実装（`LogoScan.hpp` の相関方式）**で、最も作り込まれている部分。なお相関方式とは別に、本家 **logoframe**（Yobi 版、GitHub 上の別ツール、GPLv2）という輝度閾値ベースの実装も存在する。Amatsukaze の `LogoFrame`（本プロジェクトが移植した対象）はその logoframe の再実装であり、輝度閾値を持たない相関方式に置き換えている点が異なる。このプロジェクトが移植したのは Amatsukaze 側の相関方式で、logoframe の GPLv2 コードは一切参照・移植していない。

### モデル

ロゴデータは画素ごとに係数 `a`, `b` を持つ。`background = a × observed + b × maxv` というアルファ合成の逆算式（`LogoScan.hpp::EvaluateLogo`）。

### 特徴点の選び方（`LogoScan.hpp`）

1. 32 段階の均一グレー背景にロゴを合成した画像を生成
2. 各画素を中心とする 5×5 窓の分散を計算し、**分散が大きい上位 `maskratio` 割（検出時は 0.35）の画素だけ**を評価対象にする
3. その画素の 5×5 窓から平均を引いたカーネル（ゼロ平均）を作る
4. 背景の明るさ 32 段階それぞれについて相関値が 1 になる正規化係数を事前計算

つまり**ロゴのエッジだけを見て、局所平均を引いた相関**を取る。単純に画素を掛け合わせると画像背景の濃淡に影響されるため。

### 1 フレームで 2 回評価する（`LogoScan.hpp`）

- **`corr0`**（そのまま相関）— 大きいほど「ロゴがある」証拠
- **`corr1`**（ロゴを除去してから相関）— 本当にロゴがあれば除去後は 0 付近。無いのに除去すると画像にロゴ形の凹みが刻まれて**負に振れる**

スコアは `max(0, corr0) + min(0, corr1)`。ロゴ選択の判定は `corr0 > 0.2 かつ |corr1| < 0.2`（検出でき、かつ綺麗に消せる）。

### 時間方向のフィルタ（`writeResult`）

- **MinMax**: 前 0.5 秒の最大値と後 0.5 秒の最大値の**小さい方**。動きの多い映像でロゴがかき消されるのを救済
- **1 秒移動平均**: 薄くても安定表示されている場合を識別
- 両者が食い違ったフレームは「不明」扱いにし、前後が同じ結論なら埋める
- **0.5 秒メディアン**で境界フレーム位置を精緻化

### 出力形式の要点

最良位置だけでなく**可能性の範囲**を渡す（`LogoScan.hpp`）。ロゴのフェードで境界は本質的に曖昧なので、確定させずに join_logo_scp に委ねる設計。形式は [pipeline.md](pipeline.md)。

**fade（フェード区間そのものの検出）と TOP/BTM（片フィールドロゴ、インターレースの上下フィールド別の表示区間）は出力しない**。logoframe 形式の `fade` は常に `0`、`TOP`/`BTM` の区別は使わず常に `ALL` を書く（`src/logo/interval.rs`）。理由: (1) Amatsukaze 自身もこれらを出していない、(2) 本ツールはカットをキーフレーム境界（GOP 120 = 約4秒）に丸めるため、数フレーム単位の境界精度はどのみち残存 CM マージンに吸収されて利得が小さい。

### 解像度の制約（実装済みの前提）

**ロゴデータは対象の mp4 と同じ解像度で作る必要がある**（Amatsukaze の `LogoScan.hpp` も `logo.getImgWidth() != vi.width` なら評価しない）。`tachikaze make-logo` は対象の mp4 自身から作るため、この制約は自動的に満たす。

### ロゴ辞書（`.lgd` の再利用、`src/logo/dict.rs`）

同じ局のロゴは番組が変わっても同一である。そのため学習済み `.lgd` を辞書ディレクトリに蓄積し、次回以降は毎回学習し直さずに使い回す。既定は `$XDG_DATA_HOME/tachikaze/logos`。`XDG_DATA_HOME` が未設定または空文字列なら `~/.local/share/tachikaze/logos` になる（macOS では未設定が通常）。

Amatsukaze もサービスIDごとにロゴ候補を貯め（`LogoSetting`）、`LogoFrame::selectLogo` がスコアで1つ選ぶ。同じ構造だが、本ツールは mp4 に PMT が無くサービスIDが取れない。そこで**「解像度が一致するものを全部候補にしてスコアで選ぶ」**に置き換えている。

選択は `LogoFrame::selectLogo` 相当である。キーフレーム標本での `corr0`/`corr1` から検出フレームを数え、`(検出フレームでの|corr1|平均) × (全フレーム数/検出フレーム数)` が最小の候補を選ぶ。これに加え、検出フレーム割合の絶対閾値（`--logo` フォールバックと同じ値、通常0.1・7分以下の映像は0.03）で「該当なし」を弾く。スコアは候補間の相対比較でしかない。この絶対閾値を省くと、同解像度の別局のロゴが「一番マシ」として必ず1つ選ばれてしまう。

辞書ディレクトリは `--cache-dir` とは別系統である。`.lgd` は元の録画を消すと作り直せないため、再生成できる中間物の置き場所という `--cache-dir` の規約に反する。

## join_logo_scp の構造

**26,000 行の C++。** 実体は「JL スクリプトのインタプリタ」＋「番組構成の推測エンジン」。

```
JlsIF                     ← CLI
 |- JlsDataset            ← データ保持（ロゴ区間 / 無音シーンチェンジ）
 |- JlsScript             ← JL スクリプト実行（6,000 行超）
     |- JlsScriptState    ← 条件分岐制御
     |- JlsScrReg         ← 変数（階層別）
     |- JlsAutoScript     ← Auto 系コマンド
         |- JlsAutoReform ← 基本構成推測処理（188 KB）
```

### 基本原則（`JL/doc/JLコマンド説明_全般.txt`）

> - ロゴ表示終了から補正した場所が CM カット開始位置、次のロゴ表示開始から補正した場所が CM カット終了位置となる。
> - カット位置が確定した所は以降の実行条件は無視する。
> - **カット位置が最後まで確定しなかった所は CM カットを行わない。**

3 行目が重要で、**確信が持てない箇所はカットしない側に倒れる**（本編を削るより CM を残す）。本ツールのスナップ方針（外側に寄せる）と同じ思想。

`-CutMrgIn` / `-CutMrgOut` は「シーンチェンジからロゴ表示開始までのフレーム数」「ロゴ表示終了からシーンチェンジまでのフレーム数」で、ロゴが CM 明けから遅れて出て CM 入りの前に消える局ごとの固定遅延を補正する。

### JL スクリプトは本物のスクリプト言語

| コマンド | 動作 |
|---|---|
| **`Find S/E/B 中心 範囲先頭 範囲末尾`** | ロゴ位置を基準に**無音シーンチェンジを探し、範囲内なら中心指定に最も近い場所を確定** |
| `Force S/E/B 指定位置` | 無音シーンチェンジを使わず、ロゴ位置＋固定オフセットで確定 |
| `Abort S/E/B` | そこはカットしないと確定 |
| `MkLogo` | 無音シーンチェンジを起点に**仮想ロゴを作る**（ロゴなし時の救済） |
| `DivLogo` | ロゴ区間を分割（Trim も分割される） |
| `Select` | ロゴ位置が自動検出できない時、**ロゴ可能性範囲内**から無音シーンチェンジを選ぶ |

変数、`If/ElsIf/Else/EndIf`、`Call` によるサブルーチン、ローカル変数、リスト変数、ファイル出力まである。局別 JL ファイルが 20〜30 KB あるのは、これが**局ごとに書かれたプログラム**だから。

システム変数に `$MAXFRAME` / `$MAXTIME` / **`$NOLOGO`**（ロゴなし時 1）/ `$LASTEXE` などがある。

### Auto 系 = 15 秒格子への当てはめ

中核は `ScpArType` 列挙（`src/JlsNameSpace.hpp`）:

```cpp
SCP_AR_L_UNIT,     // ロゴ有 １５秒単位
SCP_AR_L_OTHER,    // ロゴ有 その他
SCP_AR_L_MIXED,    // ロゴ有 ロゴ無も混合
SCP_AR_N_UNIT,     // ロゴ無 １５秒単位
SCP_AR_N_OTHER,    // ロゴ無 その他
SCP_AR_N_AUNIT,    // ロゴ無 合併で１５秒の中間地点
SCP_AR_N_BUNIT,    // ロゴ無 合併で１５秒の端
SCP_AR_B_UNIT,     // ロゴ境界 １５秒単位
SCP_AR_B_OTHER     // ロゴ境界 その他
```

**「ロゴの有無」×「15 秒単位に乗っているか」の 2 軸で全区間を分類する**のが基本アイデア。`N_AUNIT`（合併すると 15 秒になる中間点）があるのは、15 秒 CM の途中に無音シーンチェンジが入った場合を吸収するため。

許容誤差もパラメータ化されている:

```cpp
msecMgnCmDetect,   // CM構成で15秒単位ではない可能性と認識する誤差フレーム期間
msecMgnCmDivide,   // CM構成内分割を許す１秒単位からの誤差フレーム期間
secWCompSPMin,     // Autoコマンド番組提供で標準最小秒数
secWCompSPMax,     // Autoコマンド番組提供で標準最大秒数
```

その上に `AutoCut TR/EC`（予告・エンドカードのカット）、`AutoAdd SP/EC/TR`（番組提供・エンドカード・予告の認識）、`AutoCM`、`AutoEdge`、`AutoIns` / `AutoDel` が乗る。`AutoAdd SP` の `-code` は「構成期間 6-13 秒」「15 秒限定」といった条件を数値でエンコードする形式で、**番組提供は 5〜15 秒**といった経験則が数値表になっている。

### `detail.jls` のラベル一覧

`src/JlsDataset.cpp` の `JlsDataset::outputResultDetailGetLineLabel()` が生成:

| ラベル | 意味 |
|---|---|
| `:CM` | ロゴ無 15 秒単位（`N_UNIT` / `N_AUNIT` / `N_BUNIT`） |
| `:Nologo` | ロゴ無 その他 |
| `:Border15s` / `:Border` | ロゴ境界 |
| `:L` | ロゴ有（`L_UNIT` / `L_OTHER`） |
| `:Mix` | ロゴ有・ロゴ無混合 |
| `:Trailer(add)` / `:Trailer(cut)` / `:Trailer(cut-cancel)` | 予告・番宣 |
| `:Sponsor(add)` / `:Sponsor(cut)` | 番組提供 |
| `:Endcard(add)` | エンドカード |
| `:L-Edge(cut/add)` / `:N-Edge(cut/add)` | ロゴ端部分 |

`(cut-cancel)` は「カット対象と判定したが設定でキャンセルした」。→ [jls-settings.md](jls-settings.md)

Amatsukaze の `CMAnalyze.hpp::MakeChapter`はこれらを `startsWith` で分類してチャプター名にしている。

## ML 系言語との相性（余談）

join_logo_scp は**独自スクリプト言語のインタプリタ＋構成分類器**であり、代数的データ型とパターンマッチが向く題材。`JlsScriptDecode.cpp` に

```cpp
castErrInternal("(numArg) type:" + static_cast<int>(orgOptType));
```

という警告（文字列リテラルに int を足してポインタ演算になっている）が残っているのも、型が弱い言語で 26,000 行を書いている副作用。

**将来 join_logo_scp を作り直すなら SML# / OCaml は真面目な候補になる。** ただし本ツール（バイト列の選択とコピー）とは別プロジェクト。→ [tech-stack.md](tech-stack.md)
