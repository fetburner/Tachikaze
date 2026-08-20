# 構成と未解決事項

→ 入口: [overview.md](overview.md)

`analyze` / `cut` / `prepare` / `remap-subs` / `auto` の実装は完了している。この文書は**現在の構成**と、**まだ対応していないこと**を書く。

前半（「コマンド構成」〜「未対応の入力」）はツールを使う人向け、後半（「未解決事項」以降）はコードを読む・直す人向け。

## コマンド構成

**解析とカットは別のコマンドに分かれている。** 検出の見逃しが実際に起きるため（[jls-settings.md](jls-settings.md)）、目視確認と手動修正を挟めるようにしてある。`tachikaze --version` は `Cargo.toml` の version をそのまま出す。

| コマンド | 何をするか |
|---|---|
| `prepare` | elst 除去・字幕トラック除去・字幕抽出を1回の ffmpeg 呼び出しにまとめる（`cut` が拒否する構成の前処理） |
| `analyze` | 外部3ツールを走らせて Trim リストを作る。`--report` で診断と gate 判定。`--logo`/`--no-logo` 省略時はロゴ矩形を自動推定、`--no-logo` でロゴ無しに固定、`--logo-dir` で辞書の場所を変更 |
| `cut` | Trim をキーフレーム境界へスナップし、mp4 をサンプル単位でロスレスカット |
| `remap-subs` | 字幕サイドカーの時刻を cut 後タイムラインへ張り替える |
| `auto` | `prepare`→`analyze`→gate→`cut`→`remap-subs` を対話なしで合成 |
| `make-logo` | 入力とロゴ矩形から `.lgd`（ロゴデータ）を作る |

各コマンドの入力は1本。繰り返しはシェルのループに任せる（下記「auto」参照）。手順番号はコマンドごとに 1 から振り、他所からは「cut の手順6」のように参照する。

### prepare

```
tachikaze prepare IN.mp4 [--subs PATH]
```

elst(edit list) 除去・字幕トラック除去・字幕抽出を1回の ffmpeg 呼び出しにまとめる。`cut` は elst 付き・字幕トラック付きの入力を明示エラーで拒否するため（下記「未対応の入力」）、その回避策をここに集約している。出力は入力ごとのキャッシュディレクトリへ書く（下記「パス解決」）。

- elst も字幕も無ければ ffmpeg を呼ばず、入力をそのまま返す
- 映像2本以上・音声2本以上は明示エラーで停止する。`-map 0:v:0 -map 0:a:0` 固定のため、黙って1本目だけを残すと `cut` が本来拒否すべき構成を素通しさせてしまう。`check_track_counts` と同じ制約（`src/prepare.rs` の doc comment「複数トラックの扱い」参照）
- 字幕トラックが2本以上ある場合はエラーにせず、警告のうえ先頭の1本のみ抽出する

### analyze

```
tachikaze analyze IN.mp4 [-o trim.avs|-] [--report] [--cache-dir DIR]
                         [--jls-set KEY=VALUE]... [--jl-file FILE]
                         [--logo FILE.lgd | --no-logo] [--logo-dir DIR]
```

1. `dtvindex build`（外部プロセス）
2. `chapter_exe`（外部プロセス）
3. `join_logo_scp`（外部プロセス）。`-set autocm_sub 11 -set param_cuttr 1` を既定付与
4. `--report` で診断を stderr に出す。内容は、各カット境界とキーフレームの距離 / 余分に残る合計秒数 / 見逃し候補の警告（既知の CM ブロック長と一致する未カット区間）/ gate 判定

補足:

- `-o` は省略できる（キャッシュにだけ書き、その場所を stderr へ案内する）
- `-o -` は `trim.avs` を標準出力へ書く。診断はすべて stderr なので、`tachikaze analyze IN.mp4 -o - | tachikaze cut IN.mp4 --trim - -o OUT.mp4` のようにパイプへ流せる
- **gate 判定**: 見逃し候補・除去フレーム数0 のどちらかで「疑わしいので止める」。保持率・格子誤差ずれは参考値のみ（`src/gate.rs`）。`auto` はこの判定で cut するかどうかを機械的に決める

**`--logo` を指定したときのロゴ検出**（手順2と3の間で行う）。`.lgd` を読み、ffmpeg でロゴ矩形のフレームを流して区間判定する（`src/logo/`・`src/analyze.rs::detect_logo`）。判定結果の logoframe ファイルを `-inlogo` として手順3に渡す（`-set` 群より前に置く）。ただし渡すのは、検出フレーム割合が閾値以上、かつ logoframe テキスト（`logo::interval` の `build_text`）が空でないときだけ:

- 閾値は既定 0.1、映像長7分以下は 0.03（Amatsukaze `CMAnalyze.hpp` と同じ規則）
- 割合が閾値未満なら `-inlogo` を渡さず手順3へ進む。誤ったロゴ情報で判定を崩すより現状維持を選ぶ
- 割合が閾値以上でも、精緻化で text が空になった場合は同様に渡さない。`logo_frames`（判定の数え上げ）と `text`（`build_text` の出力）は別経路のため、割合だけでは text が空のケースを見落とす（`src/analyze.rs::inlogo_decision`）
- `-inlogo` を渡さないときは、キャッシュに残る古い logoframe.txt を削除する
- `.dtvi` の `frame_count` と読み取ったフレーム数が食い違う場合は手順3を実行せず中断する（CLAUDE.md 罠3。この検査は省略不可）

**`--logo`/`--no-logo` を両方省略したときの自動推定（既定、E18-5・#135）。** `--logo` を明示したとき・`--no-logo` を指定したときは上記のとおり（`join_logo_scp` は1回だけ）。それ以外（既定）は次の直列ループで決める（`src/analyze.rs::run_auto_logo_detection`）:

1. まず `join_logo_scp` を `-inlogo` 無しで1回走らせ、「ロゴ無しの結果」として保持する
2. ロゴ辞書（既定 `$XDG_DATA_HOME/tachikaze/logos`、`--logo-dir` で上書き）を見る。対象解像度と一致する候補があれば、学習をスキップしてそのまま検出を試す（`src/logo/dict.rs::select_candidate`）。検出に成功すればここで確定し、下記3・4は実行しない
3. 辞書で決まらなければ、手順1の保持区間の補集合（CM区間、表示順フレーム番号）を求める。それと `.dtvi` のキーパケットの `frame_number` から、標本（キーフレーム）ごとの本編/CM分類器を作る（GOP=120固定の等間隔仮定は使わない、CLAUDE.md 罠3）
4. 入力自身からロゴ矩形の候補列を推定する（`src/logo/estimate.rs::estimate_candidates`、AUC順の採用列）。先頭から最大5件を順に学習し（`src/logo/scan.rs::make-logo` と同じアルゴリズム）、検出まで試す。学習失敗（回帰係数 NaN 等）や検出失敗は次候補へ進み、成功した時点で打ち切る
5. 成功した候補の `.lgd` だけをロゴ辞書へ保存する。全候補が尽きればロゴ無しとして扱う（1回目の結果をそのまま使い、`join_logo_scp` の2回目は走らせない）。見つかった場合だけ `join_logo_scp` を `-inlogo` 付きでもう1回走らせる。その結果を最終の `trim.avs` とする

各段階の結果（辞書ヒット・候補列とAUC・何番目の候補で成功したか・学習の有効フレーム数・検出フレーム割合・`-inlogo` を渡したか）は stderr に出る。

（必要なら trim.avs を人手で編集してから `cut` へ渡す）

### cut

```
tachikaze cut IN.mp4 --trim trim.avs|- -o OUT.mp4 [--dtvi work.mp4.dtvi] [--cache-dir DIR]
                     [--snap outward|inward] [--cm-output CM.mp4]
                     [--video-only] [--verify] [--segment-map PATH]
```

1. mp4-atom でサンプル表を読み、表示順↔デコード順を導出 → `.dtvi` と一致するか assert（不一致なら停止）
2. Trim をキーフレーム境界へスナップ（既定 outward）
3. 映像: デコード順に (E-S) サンプルを選択
4. 音声: 区間ごとに、区間先頭サンプルのソース上の DTS から最近傍の音声パケットを引き当てる
5. mp4-atom で書き出し（stsd は clone、サンプル表のみ再構築）
6. 自己検証（下記「自己検証」）
7. `--cm-output` 指定時は保持区間の補集合について手順3〜6を繰り返す（補集合の両端も同期サンプル上に来るので追加のスナップは不要）。保持側と CM 側でフレーム数の合計 == 総フレーム数、集合が互いに素であることを assert
8. 自己検証を通り最終出力へ rename できた後、保持側の区間マップ（snap 後の境界と出力タイムライン上の開始時刻）を `work.mp4.segmap.json` へ書く。`--segment-map PATH` で任意の場所にも書ける。`--cm-output` 指定時も保持側だけに出す

補足:

- `--trim -` で標準入力から Trim リストを読める（`analyze -o -` の出力をそのまま渡せる）
- 区間マップは、外部で作った字幕やチャプターを cut 後のタイムラインに合わせるための中間データ。処理を始める前（手順1より前）に既定キャッシュパスの古い区間マップを削除し、自己検証を通って新しく書けたときだけ残す（古いマップを `remap-subs` が鮮度チェックなしに使ってしまうのを防ぐ）

### remap-subs

```
tachikaze remap-subs IN.mp4 [--segment-map PATH] [--subs PATH] [-o OUT.ass|OUT.srt]
```

1. 区間マップ・字幕サイドカー（ASS/SRT）をキャッシュから自動解決（`--dtvi` と同じ規則。明示指定が最優先）
2. 区間マップの区分的な線形写像でイベントの Start/End を張り替える（`output_t = output_start_k + (source_t - source_start_dts_k)`）
3. 各イベントを シフト（保持区間に完全一致）/ 破棄（どの保持区間とも重ならない＝CM に完全に含まれる字幕）/ クリップ（境界を跨ぐ）に分類し、件数を必ずログに出す。時刻以外のフィールド・行はそのまま素通しする（`src/subtitle.rs`）

### auto

```
tachikaze auto IN.mp4 -o OUT.mp4 [--cm-output CM.mp4] [--ignore-gate] [-f|--force]
                     [--analyze-only] [--no-subtitles] [--snap] [--verify]
                     [--jl-file] [--jls-set] [--cache-dir]
                     [--logo FILE.lgd | --no-logo] [--logo-dir DIR]
```

`prepare` → `analyze` → gate 判定 → `cut` → `remap-subs` を対話なしで合成する（`src/auto.rs`）。アルゴリズムは持たず、各ステップは上記の関数・処理をそのまま呼ぶ（`commands::execute_cut` を `cut` サブコマンドと共有。詳細は `src/commands.rs` / `src/auto.rs` の doc comment）。

- gate が「疑わしいので止める」と判定したら cut せず exit code 3 で停止し、trim.avs のパスと「直して cut する」コマンド例を出す
- `--ignore-gate` で無視できるのは gate の判定だけで、自己検証や `.dtvi` 必須は変わらない
- `--analyze-only` は `--ignore-gate` の有無に関わらず cut へ進まない。gate が疑わしいと判定していれば exit code は 3 のまま（無視の対象は「cut へ進むかどうか」で、停止コードそのものではない）
- 1プロセスにつき入力は1本。複数ファイルはシェルのループに任せる。1入力1プロセスにすることで、exit code の意味が「その1本に対する答え」に一意になる（下記「exit code」）
- `--logo`/`--no-logo`/`--logo-dir` は値をそのまま `analyze` へ渡すだけ（`auto` 独自のロジックは持たない）。挙動は上記「analyze」の「自動推定」節と同じ

    ```
    for f in *.mp4; do case "$f" in *_CMcut.mp4) continue;; esac; tachikaze auto "$f" -o "${f%.mp4}_CMcut.mp4"; done
    ```

    出力名を `_CMcut.mp4` に固定し、`*.mp4` の glob が前回の出力を再び入力として取り込まないよう `case` で弾いている。

- `-o` は必須（出力先を暗黙に決めない）。CM側は `--cm-output` を指定したときだけ出す
- 本編・CM側・字幕サイドカー（`-o` と同じ stem の `.ass`/`.srt`）のいずれかが既に存在すれば、既定でその入力をスキップする。上書きしたい場合は `-f`/`--force`（`cp -f` の慣習に合わせている）
- スキップ判定には例外が2つある。字幕トラックがある入力で字幕サイドカーだけ欠けている場合は、本編/CM側が揃っていてもスキップせず再試行する。前回 `remap-subs` が失敗した状態を、次回実行で自動的に直すため（`src/auto.rs` の doc comment「既存出力のスキップと -f/--force」参照）。また `analyze` はキャッシュがあっても毎回実行する。キャッシュキーが入力の絶対パスのハッシュだけで、内容の変化を検出できないため（同 doc comment 参照）

### make-logo

```
tachikaze make-logo IN.mp4 --rect x,y,w,h -o OUT.lgd [--threshold N]
```

1. ロゴ検出に使う `.lgd`（Amatsukaze 形式ロゴデータ）を、入力 mp4 とロゴ矩形だけから作る（`.dtvi` も外部3ツールも使わない）。`make-logo` 自体は今もロゴ位置を `--rect`（`x,y,w,h`、2の倍数に丸める）で受け取る低レベルなコマンドである。

   **判断の履歴（2026-08 改訂）**: 当初は「局・番組ごとにロゴの形と色が異なり、対象素材だけから汎用的に検出する既存手段が無い」ため矩形を自動探索しないと決めていた。「Amatsukaze 側にも自動探索は無い」という事実は今も正しい。その後、定常段差のブロック中央値と本編/CM 在不在の AUC という局非依存の統計量が実測で成立し（`src/logo/estimate.rs`、詳細は [cm-detection.md](cm-detection.md)「ロゴ検出」）、判断を変えた。`analyze`/`auto` が `--logo`/`--no-logo` を両方省略したとき、内部でこの矩形推定をして `make-logo` と同じ学習アルゴリズムを呼ぶ（下記「analyze」の「自動推定」節）。`make-logo` コマンド自体（手動で矩形を指定する経路）は変えていない。
2. 矩形の外周1ピクセルが単色（最小値・最大値の差が `--threshold`、既定12、以下）のフレームだけを学習に使い、画素ごとに最小二乗で回帰係数 `a`/`b` を求める（`src/logo/scan.rs`）。既定で入力全体を走らせる。CM区間だけを指定すると「ロゴが無い」ロゴデータができてしまうため
3. 有効フレーム数（何フレーム中いくつ使ったか）を必ず stderr に出す。壊れたロゴデータを黙って書き出さないよう、有効フレームが0件または4件未満（`MIN_USABLE_FRAMES`）の場合と、係数が NaN/inf/`a==0` になった場合は失敗させる。件数そのものを別に検査するのは、少数のフレームでは回帰係数が NaN/inf にならず黙って有限値になりうるため

### exit code

| code | 意味 |
|---|---|
| 0 | 完了（`auto` が既存出力を検出してスキップした場合も 0。失敗でも判定停止でもないため） |
| 1 | エラー |
| 2 | 引数の誤り（clap の既定。実測: `tachikaze --bogus` → exit 2） |
| 3 | `auto` の gate が疑わしいと判定して停止（`analyze`/`cut`/`prepare`/`remap-subs` はこの値を返す経路を持たない） |

**なぜ gate 停止が 2 ではなく 3 なのか**: clap が引数の誤り（usage error）に使う exit code が 2 であるため（実測、上記表参照）。空いている最小の番号が 3 になる。

### `.dtvi` は必須

`.dtvi` はオープン GOP の判定（[lossless-cut.md](lossless-cut.md)）と自己検証 4（表示順/デコード順の突き合わせ）に必須。省略できるのは**パスの指定**だけで、`.dtvi` 無しで動くわけではない。`cut --dtvi` を省略すると、`analyze` と同じ入力ごとのキャッシュディレクトリ規則から `work.mp4.dtvi` を自動的に探す。見つからなければ、`analyze` を実行するコマンド例を添えて停止する（探索順・キャッシュの場所は下記「パス解決」）。

### CLI の設計判断

- **`auto` の入力を1本に絞っている理由**: 1プロセスで複数入力を受けると exit code が「N本中M本失敗」に潰れ、スクリプトから「その入力がどうなったか」を一意に読めない。1プロセス1入力にして繰り返しをシェル（`for` / `xargs -n1`）に任せることで、exit code の意味を一意にしている
- **`-f`/`--force` が上書きの意味である理由**: `cp -f` / `rm -f` の慣習では `-f`/`--force` は「上書き」を指すため、上書きに割り当てている。gate 判定の無視は別に `--ignore-gate` を用意している
- **`auto -o` が必須である理由**: 出力先は暗黙に決めるより明示させる方が自然。CM側も `--cm-output` の指定有無だけで決まるので、CM側出力の有無を切り替える専用フラグは要らない
- **診断を stderr に寄せている理由**: UNIX の作法（stdout はデータ、stderr は診断）に合わせ、進捗・警告・レポートはすべて stderr に出す。空いた stdout は `analyze -o -` で `trim.avs` をパイプに流すために使える

**`analyze` と `cut` を分けたまま `auto` も併設している理由**。検出には見逃しがあるため、`analyze` と `cut` のあいだに人手を挟める設計は崩さない。その上で対話なしの一括処理（`auto`）を安全に足せるのは、次の3つが揃っているから:

1. **やり直しが安い**: `analyze` の中間ファイル（`.dtvi` / `trim.avs` / `detail.jls`）はキャッシュに残り、入力 mp4 自体は無改変（`prepare` の出力もキャッシュに書くだけで `IN.mp4` を書き換えない）。`auto` が誤った判定で走っても、後から `cut` を直接叩き直すだけで直せる（`auto --analyze-only` が出す `cut` コマンド例を使う）
2. **事後確認の手段がある**: `--cm-output` で CM 側を別ファイルに出せるため、`auto` が黙って本編を欠損させていないかを後から目視できる
3. **機械可読な判定材料がある**: gate が `analyze` の成果物だけから「見逃し候補」「除去フレーム数0」を機械的に判定できるため、「疑わしいときだけ人手を呼ぶ」を自動化できる

`auto` は gate のこの判定を使って人手を安全に外す。見逃し候補ヒューリスティックが効かない番組もあるため（`src/gate.rs`「見逃し候補ヒューリスティックの限界」）、gate が止めないことは検出が完全に当たっている保証ではない。対話しながら都度確認したい場合は `analyze` → 目視 → `cut` を使う。

## パス解決

インストールして（`/usr/local/bin` などに置いて）使う場合を含め、パスの決め方は**実行ファイル / 読み取り専用データ / キャッシュ / 蓄積データ / 出力**の5分類ごとに変える。配置手順は [toolchain-macos.md](toolchain-macos.md)「ビルド後の配置とインストール」。

| 種類 | 探索順・既定 |
|---|---|
| 実行ファイル（`tachikaze` / 外部3ツール / `ffmpeg` / `ffprobe`） | `PATH` のみ（`src/tools.rs::resolve_tool`） |
| 読み取り専用データ（JL コマンドファイル、既定 `JL_標準.txt`） | `--jl-file` → `<join_logo_scp の実体パス>/../../share/join_logo_scp/JL/` |
| キャッシュ（再生成可能な中間物） | `--cache-dir` → 既定 `<ホームディレクトリ>/.cache/tachikaze/` |
| 蓄積データ（ロゴ辞書、`.lgd`） | `--logo-dir`（`analyze`/`auto`）→ 既定 `$XDG_DATA_HOME/tachikaze/logos`。`XDG_DATA_HOME` が未設定または空なら `<ホームディレクトリ>/.local/share/tachikaze/logos` |
| 出力 | 明示指定のみ（`cut`/`auto` の `-o` は必須） |

`--jl-file` / `--cache-dir` / `--dtvi` を明示指定した場合は、いずれも上記の探索より最優先でそのまま使う。分類ごとの詳細:

**実行ファイル**。`PATH` 以外は探さない。別の場所に置いているものを使いたければ `PATH=/opt/jls/bin:$PATH tachikaze ...` のように前置する。

**読み取り専用データ**。`--jl-file` の次の1段は `make install` 配置を前提にしている（`src/tools.rs::default_jl_command_file`）。

**キャッシュ**。どの根からも、入力ごとに `<根>/<入力絶対パスのハッシュ>-<stem>/` を使う（`src/workdir.rs`）。削除はせず、同じ入力を再実行すると再利用する。既定の根はホームディレクトリから決まり（`std::env::home_dir()`）、ホームが特定できない場合は `--cache-dir` を促すエラーで停止する。キャッシュに置くもの:

- `work.mp4` — 入力への symlink
- `work.mp4.dtvi` / `trim.avs` / `detail.jls` — `analyze` の成果物。`cut --dtvi` 省略時はこの規則から `work.mp4.dtvi` を自動的に探す
- `work.mp4.segmap.json` — `cut` が書く区間マップ（`src/segmap.rs`、`workdir::cached_segment_map_path`）。`cut --segment-map PATH` で任意の場所にも書ける
- `input_prepared.mp4` — `prepare` が elst 除去・字幕除去後に書く前処理済み入力（`src/prepare.rs`、`workdir::prepared_input_path`）
- `subs.ass` / `subs.srt` — `prepare` が mp4 内蔵字幕トラックから抽出した字幕サイドカー。`remap-subs` の入力（`workdir::subs_path`）

**蓄積データ**（`src/logo/dict.rs`）。学習済み `.lgd` を貯める辞書ディレクトリ。既定は `$XDG_DATA_HOME/tachikaze/logos`。`XDG_DATA_HOME` が未設定または空文字列なら `<ホームディレクトリ>/.local/share/tachikaze/logos` になる（`dict::resolve_dict_dir`）。キャッシュとは別分類にした理由、環境変数を読む理由は下記「なぜこの形にしたか」参照。

**出力**。`cut -o` / `auto -o`（本編、必須）と `--cm-output`（CM側、`auto` は指定時のみ）はすべて明示指定させる。例外は2つ。`remap-subs` を単体で使うときだけ、入力の隣に `*_CMcut.ass` / `*_CMcut.srt` を既定で置く（`src/commands.rs::default_remap_subs_output_path`）。`auto` の字幕サイドカーは `-o` と同じ stem・別拡張子で書く（`src/auto.rs::subs_sidecar_path`。本編出力と揃えることでプレイヤーが自動で字幕を読み込める）。

なぜこの形にしたか:

- **中間物をキャッシュに置いた理由**: `.dtvi` / `trim.avs` / `detail.jls` はいずれも `analyze` を再実行すれば作り直せる。外部3ツールはいずれも既存の出力先を実害なく上書きすることを実バイナリで確認済みなので、消えても復旧できるデータとしてキャッシュ扱いにした。既定で残すことで、`analyze` → `cut` を `--cache-dir` / `--dtvi` の手打ちなしで繋げられる。ディレクトリ名（`~/.cache/tachikaze`）自体は XDG Base Directory 仕様の既定と同じものを借りている
- **ロゴ辞書をキャッシュと別分類にした理由**: `.lgd` は元の録画を消すと作り直せない。`--cache-dir`（再生成できる中間物の置き場所という規約）にはそぐわないため、蓄積データという別分類を立てた（`src/logo/dict.rs` モジュール doc comment）
- **XDG 由来の環境変数を読まないと決めた理由（ロゴ辞書は例外）**: 置き場所を決める口を `--cache-dir`（キャッシュ）・`--jl-file`（JL）のように引数に一本化し、環境変数の値によって挙動が変わらないようにした。ディレクトリ名だけ XDG の既定（`~/.cache`）を借りているが、環境変数側は一切読まない（`src/workdir.rs::default_cache_root` の doc comment）。**唯一の例外がロゴ辞書**（`src/logo/dict.rs::resolve_dict_dir`）で、`$XDG_DATA_HOME` を実際に読む。理由は2つある。(1) 蓄積データという性質上、XDG のデータディレクトリ規約にそのまま従う方が利用者にとって自然である。`--cache-dir` が借りているのはディレクトリ名だけだが、辞書は規約そのものに従う。(2) 呼び出し側の明示的な上書き引数が環境変数より常に優先する。そのため `--cache-dir` 側の懸念（複数の口が同時に設定されたときどちらが効くか読まないと分からない）は生じない（詳細は `src/logo/dict.rs` モジュール doc comment）
- **ホームディレクトリを `std::env::home_dir()` で取る理由**: `$HOME` 環境変数の読み取りではなく OS への問い合わせなので、`$HOME` が unset な環境でも既定値を作れる場合がある。実測（`src/workdir.rs::default_cache_root` の doc comment）: rustc 1.97.1 時点で非推奨警告は出ない。Unix では `$HOME` が unset でも `getpwuid` 経由でホームディレクトリを引ける。実際にホームが取れない（＝エラーになる）のは「呼び出しユーザーの passwd エントリすら無い」環境（コンテナで存在しない UID として動かす等）に限られる
- **ホームディレクトリが特定できないときにエラーで止める理由**: 黙って一時ディレクトリ等へフォールバックすると、キャッシュが知らない場所に増える。さらに悪いことに、別プロセスが別の一時領域を引くと同じ入力に対して別のキャッシュディレクトリを掴み、`analyze` → `cut`（`--dtvi` 省略）の受け渡しが**エラーを出さずに**外れる。`--cache-dir` を促すエラーで明示的に止める方が安全と判断した
- **外部ツール専用の配置ディレクトリを指定するオプションと「自分の実行ファイルの隣」を削った理由**: `PATH=/opt/jls/bin:$PATH tachikaze ...` のように `PATH` を前置すれば同じことができる。インストールしたくない場合は Docker イメージ（[docker.md](docker.md)）がある。口を1つに絞ることで、複数の口が同時に設定されたときどちらが効くか読まないと分からない状態を無くした
- **作業ディレクトリを明示するオプションと使い捨て一時ディレクトリに戻すオプションを `--cache-dir` に統合した理由**: どちらも「キャッシュの置き場所」という同じ軸の粒度違いにすぎない。使い捨てにしたい場合は `--cache-dir "$(mktemp -d)"` で足りるため、専用オプションを別に持つ必要がない
- **JL ファイルの探索を1段だけにした理由**: かつては5段あった。環境変数によるディレクトリ指定、XDG のデータディレクトリ群、prefix 相対の `share/`、そして `join_logo_scp` と同じディレクトリの `JL/`（1ディレクトリ配布との後方互換）である。`make install` 配置（[toolchain-macos.md](toolchain-macos.md)「ビルド後の配置とインストール」）に一本化したことで、後方互換の段は不要になった。個別配置にしたい場合は `--jl-file` で直接指定する
- **出力だけキャッシュの探索規則から外した理由**: 出力は録画ファイルと同じ場所で管理したいという運用上の要求があり、消えても再生成できるキャッシュとは性質が違う（ユーザーの成果物）ため対象外にした
- **設定ファイル用の置き場所を用意しなかった理由**: 現時点で設定ファイルに載せる項目がなく、XDG の config ディレクトリ相当は使っていない。必要になるまで空けておく方針

## 未対応の入力（`mp4io/support.rs` が明示エラーで落とす）

いずれも「静かに壊れる」より「止まる」を選んでいる。音声コーデック自体は `mp4-atom` が認識する音声 Codec 全般（Opus / AAC など）を同じ経路で扱う。ここに残すのはコーデックと無関係に未対応と判断した構成。

**`elst`（edit list）あり**（`check_no_edit_list`）。`moov.clone()` で引き継いだ `elst` は、`segment_duration` がカット後のトラック長を超え、`media_time` が新しい先頭の正当なフレームを数フレーム分スキップする。「エラーは出ないが結果が壊れる」の実例を実験で確認済み。回避策:

```console
$ ffmpeg -i IN.mp4 -c copy -use_editlist 0 -movflags +faststart OUT.mp4
```

除去後の映像・音声パケットが CRC32 でビット一致することを確認済み。ただしこれはペイロードのみの確認で、A/V の相対タイムスタンプは対象外。除去が A/V 相対時刻に与える影響の実測と方針は [measurements.md](measurements.md)「elst 除去と A/V 相対時刻」。

**`stsd` が複数エントリ**（`check_single_stsd_entry`）。`write.rs` が `sample_description_index: 1` 固定で `stsc` を再構築しているため。対応するにはサンプルごとの index 保持（`read.rs` の `SampleInfo` と `write.rs` の両方の変更）が必要。無劣化 remux ではパラメータ差異という原因自体を解消できないので、事前除去の回避策は提示できない。

**オープン GOP**（`check_closed_gop`）。「S の同期サンプルからデコード順に `E - S` パケット取る」規則が成立しない。`.dtvi` の `leading_frame_count` で判定する（`.dtvi` が無い場合も判定不能として停止）。

**映像2本以上 / 音声2本以上**（`check_track_counts`）。映像1本 + 音声1本のみ対応。`prepare`（`src/prepare.rs::reject_multiple_video_or_audio_tracks`）も同じ制約を入口で課す。`prepare` は `-map 0:v:0 -map 0:a:0` 固定で ffmpeg に渡すため、この検査が無いと二重音声放送のような入力でも黙って1本目だけを残し、`cut` が本来拒否すべき構成を素通しさせてしまう。

**字幕などのトラックあり**（`check_track_counts`）。トラックとしては未対応のまま（`cut` に直接渡すと明示エラーで止まる）。字幕の保持は `prepare` が担う: `cut` へ渡す前に字幕トラックを ASS/SRT のサイドカーへ抽出し、mp4 側からは除去する（上記「prepare」。以下「方式A」）。cut 後タイムラインへの追従は `remap-subs` が区間マップから計算する。

字幕トラックをそのまま `cut` に持たせてサンプル単位でコピーする方式（「方式B」: トラックごとの区間選択に加え、字幕のようにサンプルが疎なトラックの扱いが別途要る）は採らなかった。理由は2つ:

1. ARIB 字幕をデコードした一部のスタイル情報（色・位置などの装飾）は、`mov_text` のようなプレーンテキスト中心の字幕コーデックに変換する時点で失われる。トラックとして持たせても方式Bだけでは解決しない
2. 方式Bは対応工数が方式Aより一桁大きい。トラックごとの区間選択と疎なサンプルの扱いを `mp4io` 全体に持ち込む必要がある

---

ここから先はコードを読む・直す人向け。

## 未解決事項

### 複数音声トラック

未対応（上記「未対応の入力」のとおり明示エラー）。対応するならトラックごとの区間選択を実装する必要がある。

### 字幕のトラック対応

`cut` にトラックとして持たせる方式（方式B）は未対応のまま（上記「未対応の入力」参照）。`prepare` のサイドカー方式（方式A）で実用上の字幕保持は満たしているため、方式Bを実装する優先度は無い。

### 継ぎ目の MDCT 過渡（クリックノイズ）

許容する方針で決定済み。継ぎ目は残存 CM マージンの内側に来る（[lossless-cut.md](lossless-cut.md)）。

### `prepare` の elst 除去による AAC の残存ずれ

許容する方針で決定済み（実測）。elst 除去により、保持した最初の区間がソースの先頭フレームから始まる場合に限り、音声の priming 分がそのまま音声として残る（実測 21.333ms、他エンコーダでは見積もりで最大43ms程度）。ファイルにつき高々1回・1フレーム未満・非累積のずれで、`prepare`/`auto` は elst を自動除去してよいと判断している（[measurements.md](measurements.md)「elst 除去と A/V 相対時刻」）。Opus は `dOps` がコーデックレベルで pre-skip を伝えるため影響なし。

### `--snap inward` と `--cm-output` の併用

拒否する。inward では保持区間が退化（`end < start`）しうるため、補集合の順序も壊れる。

### `mp4io::read` のテストが並列実行で稀に落ちる

未対応。`src/mp4io/read.rs::tests::ffprobe_packets` が ffprobe の**起動失敗**を `.expect("ffprobe を起動できること")` で panic させる。多数の ffprobe を同時に起動すると稀に起動に失敗し、テストが落ちる（master で8回中4回再現）。同モジュールには起動できない場合をスキップ扱いにする `skip_if_ffprobe_missing` があるので、`ffprobe_packets` の起動失敗も同じ扱いにすれば直る。テストだけの問題で、製品コードには影響しない。

### `analyze` のテストが `--include-ignored` で競合する

未対応。`src/analyze.rs::tests::analyze_run_produces_trim_list_with_real_tools` は `TOOL_PATH_ENV_LOCK` を取らずに実 `PATH` からツールを解決する。一方、同モジュールの他3テストはそのロックの下でプロセス全体の `PATH` を差し替える。並列に走ると前者が差し替え後の `PATH` を読んで落ちうる。`--ignored` 単独では他3テストが走らないため出ず、`--include-ignored` でのみ再現する。前者にも同じロックを取らせれば直る。

### キャッシュ鍵の弱さ

キャッシュディレクトリ名は入力の**絶対パスのハッシュのみ**から決まる（`workdir::cache_dir_for_input`、FNV-1a）。同じパスに別内容のファイルが後から置かれても（録画ファイルの上書き・再利用）区別できず、古い `.dtvi` / `trim.avs` / `input_prepared.mp4` を新しい入力に対して誤って再利用しうる。`auto` は `analyze`/`prepare` を毎回作り直すことでこの穴を避けているが（`src/auto.rs` の doc comment）、`cut --dtvi` を省略してキャッシュから自動解決する経路には対策が無い。size + mtime の突き合わせなどの対策は、要求されていない現時点では追加しないと判断している（理由は `src/auto.rs` の doc comment 参照）。

### `-CutMrgIn` / `-CutMrgOut` を CLI から渡す口

追加しないと決定。これらは join_logo_scp の起動オプション（`-set` 変数ではない）で、シーンチェンジからロゴ表示開始/終了までの局固有の固定遅延を補正する。実測（BS11、[jls-settings.md](jls-settings.md)「`-CutMrgIn`/`-CutMrgOut`」）では、`JL_標準.txt` の自動判断（`-CutMrgWI 2`/`-CutMrgWO 2`）が既に妥当な値（`CutMrgIn=5 CutMrgOut=8`）を検出していた。`0`〜`20` の範囲で手動指定に変えても、最終出力の `trim.avs` に変化は無かった（中間表現の `detail.jls` はラベル・ロゴ秒が変化した。詳細は jls-settings.md 側）。

やらないと決めた理由: 実測した1局では `trim.avs` に効果が確認できず、CLI にオプションを追加する実装コスト（`-set` ではないため `--jls-set` と別の口が要る）に見合う利得が無いと判断した。局によっては固定遅延が大きく効果が出る可能性は残るため、実際に精度不足が観測された局が出たら対応を検討する。

### ロゴ矩形推定・AUC 採用のパラメータは4局からの較正

既知の限界として残す（このエピック E18 では直さない）。閾値ラダー・サイズ上限・AUC 採用閾値 0.9・CM 標本ガード 20 枚は、実ファイル5本＋追検証4本のいずれも同じ4局（BS日テレ / TOKYO MX / フジテレビ / テレビ朝日）から決めた値である（[measurements.md](measurements.md)「ロゴ矩形の自動推定」）。局が増えれば見直しが必要になる可能性がある。

### 相関方式の検出限界と、AUC 採用が誤ったロゴでも trim を改善しうること

TOKYO MX の ED 区間（90秒）は、ロゴ自体は出ているのに段差が番組全体平均の半分まで落ちて相関方式が検出できず、CM と誤判定される。矩形推定ではなく相関方式そのものの限界であり、矩形の余白を振っても解消しない（[jls-settings.md](jls-settings.md)「既知の失敗モード: ロゴ検出」）。

一方でテレビ朝日は局ロゴを天気パネルや時計から分離できないが、代わりに時計を採用すると `trim.avs` が改善した（CM 約4.25分を追加除去）。**効くのは「局ロゴを当てること」ではなく「検出結果が本編/CM と相関すること」であり、AUC 採用がこれを自動で拾う**。フジテレビ（L字放送）では本物のロゴと L字帯・台風テロップの断片の AUC が僅差で並んだ例もあり、僅差の逆転はあり得る。ただし AUC が高い候補は定義上本編/CM と相関しており、誤採用しても trim が悪化しにくいことをテレビ朝日の実例で確認済み。検出フレーム割合の絶対閾値と `auto` の gate が残りの防御。

### 見逃し候補ヒューリスティックとロゴ

`src/report/missed.rs` は `.jls` のラベルを見ず、ギャップ長の一致だけで判定するため、ロゴの有無でロジックは変わらない。実測2本（BS11・TOKYO MX2、[jls-settings.md](jls-settings.md)「ロゴありでの見逃し警告」）ではロゴあり/なしで見逃し候補の件数に変化が無かった（どちらも0件）。ただし検出済み CM ブロック長が揃わない番組でロゴありを試した実測が無いため、一般に誤警告が増えないとは言えない。加えて MX2 は元々このヒューリスティックが発火しない構成だった。`find_missed_candidates` は保持区間内部の `.jls` エントリと長さを突き合わせる実装のため、見逃しブロックが単一の長い `:L` エントリの内側にあると検出対象にならない。「両方0件」という結果は、ロゴの有無というより検出条件自体が MX2 では成立していなかった可能性がある。

## 方針として作らないもの

| 項目 | 理由 |
|---|---|
| CM 検出アルゴリズム | 既存ツール(chapter_exe → join_logo_scp)が担当 |
| 映像の再エンコード | 数秒の CM 残りを許容する方針 |
| 音声の再エンコード | 継ぎ目のノイズは残存 CM の範囲内 |
| `auto` の複数入力・ディレクトリ一括処理(glob 展開) | 1プロセス1入力にすると exit code の意味が一意になる。繰り返しはシェル(`for` / `xargs -n1`)の仕事 |
| ロゴ検出の fade(フェード区間そのもの)・TOP/BTM(片フィールドロゴ)の出力 | Amatsukaze も出していない。カットはキーフレーム境界(GOP 120 = 約4秒)に丸めるため、数フレーム単位の境界精度は残存 CM マージンに吸収され利得が小さい(詳細は [cm-detection.md](cm-detection.md)「出力形式の要点」) |

ただし「境界 GOP だけ再エンコードすればフレーム精度になる」ことは覚えておく。実測で境界あたり 4 秒（120 フレーム）分のエンコードで済む。30 分番組・8 境界なら 32 秒分。方針が変わったときの逃げ道として安い。

## 将来的な拡張候補

- **チャプター出力**: dtvindex に `create_join_logo_scp_chapters` があるのでそれを呼ぶだけ
- **局別 JL ファイルの選択**: 現状は `JL_標準.txt` 既定（`--jl-file` で差し替えは可能）

## モジュール構成

**この分割はファイル所有権の単位でもある。** 1 つの関心事が 1 ファイルに収まるように決めてあるので、統合・分割するときは理由を持って行うこと。

| ファイル | 責務 | 参照 |
|---|---|---|
| `cli.rs` | サブコマンドとオプションの定義 | — |
| `commands.rs` | 各モジュールを繋ぐ組み立て（アルゴリズムは持たない） | — |
| `auto.rs` | `auto` コマンドの組み立て（`prepare`→`analyze`→gate→`cut`→`remap-subs`、アルゴリズムは持たない） | 上記「auto」 |
| `tools.rs` | 外部ツールと JL ファイルの探索 | 上記「パス解決」、[toolchain-macos.md](toolchain-macos.md) |
| `external.rs` | 外部プロセスの起動と出力の回収 | [pipeline.md](pipeline.md) |
| `ffprobe.rs` | ffprobe への問い合わせ（CSV 行を返す `csv_rows` / 単一スカラー値を返す `scalar_entry`。`-show_entries` / `-show_data_hash CRC32`）を1か所に集約する責務 | 罠2（md5 ではなくパケット単位の CRC32）、[lossless-cut.md](lossless-cut.md) |
| `errctx.rs` | 「〜に失敗しました: <パス>」というエラー文脈の付与を1行で書くための拡張トレイト（`path_ctx`） | — |
| `workdir.rs` | 作業ディレクトリ（既定は入力ごとのキャッシュ）と symlink | 上記「パス解決」 |
| `order.rs` | `DisplayIdx` / `DecodeIdx` | 下記「型設計の要点」 |
| `trim.rs` | `Trim(a,b)++…` のパース / 生成（半開区間 `[s, e+1)` に正規化） | — |
| `dtvi.rs` | `.dtvi` のパース（タブ区切りテキスト） | [pipeline.md](pipeline.md) |
| `jls.rs` | `detail.jls` のパース | [pipeline.md](pipeline.md) |
| `analyze.rs` | analyze コマンドの組み立て | [jls-settings.md](jls-settings.md) |
| `prepare.rs` | `prepare` 本体: elst 除去・字幕トラック除去・字幕抽出を1回の ffmpeg 呼び出しにまとめる | 上記「prepare」 |
| `gate.rs` | `analyze` の成果物（`TrimList`/`JlsEntry`/`Dtvi`）だけから検出結果が機械的に疑わしいか判定する（mp4 は読まない） | 上記「analyze」手順4・「auto」 |
| `report/mod.rs` | `--report` の出力 | [measurements.md](measurements.md) |
| `report/missed.rs` | 見逃し候補の警告 | [jls-settings.md](jls-settings.md) |
| `mp4io/read.rs` | サンプル表の読み込み | [mp4-atom.md](mp4-atom.md) |
| `mp4io/order_map.rs` | 表示順↔デコード順の写像、合成時刻の算出 | [pipeline.md](pipeline.md) |
| `mp4io/support.rs` | 未対応構成の判定（上記「未対応の入力」を早期に落とす） | 上記「未対応の入力」 |
| `mp4io/write.rs` | mp4 の書き出し（stts/ctts の圧縮、インターリーブ、co64） | [mp4-atom.md](mp4-atom.md) |
| `plan.rs` | キーフレーム境界へのスナップ、区間の計画、保持区間の補集合 | [lossless-cut.md](lossless-cut.md) |
| `audio.rs` | 音声パケットの選択（区間のソース時刻から引き当て） | [lossless-cut.md](lossless-cut.md) |
| `verify.rs` | 自己検証 | 下記「自己検証」 |
| `segmap.rs` | `cut` が書く区間マップ（snap 後の境界と出力タイムライン上の開始時刻）の構造体、JSON への書き出しと読み込み | 上記「cut」手順8 |
| `subtitle.rs` | `remap-subs` 本体: ASS/SRT の Start/End を区間マップの区分的な線形写像で張り替える（シフト/破棄/クリップの分類、丸め方向） | 上記「remap-subs」 |
| `logo/lgd.rs` | Amatsukaze 形式ロゴデータ `.lgd`（AviUtl 互換のベース部 + Amatsukaze 独自の float 部）の読み込み | [cm-detection.md](cm-detection.md) |
| `logo/frames.rs` | ffmpeg を子プロセスとして起動し、ロゴ矩形の輝度平面をフレーム順にストリームで読む（`stream_luma_frames`、読み取ったフレーム数と `.dtvi` の `frame_count` の一致検査あり）。ロゴ矩形推定専用に、クロップせず全画面をキーフレームだけ読む関数（`stream_keyframe_luma_frames`、フレーム数一致検査なし）も持つ | [cm-detection.md](cm-detection.md) |
| `logo/estimate.rs` | `estimate_candidates`: 入力自身からロゴ矩形の候補列を推定する。定常段差のブロック中央値で候補を作り（閾値ラダー・大構造除去・分裂併合）、本編/CM の在/不在 AUC で採点して採用列にする | [cm-detection.md](cm-detection.md)「ロゴ検出」、[measurements.md](measurements.md)「ロゴ矩形の自動推定」 |
| `logo/score.rs` | ロゴマスク生成と相関スコア（`corr0`/`corr1`）。Amatsukaze `LogoScan.hpp` の相関方式を移植 | [cm-detection.md](cm-detection.md) |
| `logo/scan.rs` | `make-logo` 本体: 外周1ピクセルが単色のフレームだけを使い、画素ごとに最小二乗で `.lgd` の係数 `a`/`b` を求める。`.lgd` の書き出し（ベース部はゼロ埋め） | 上記「make-logo」 |
| `logo/interval.rs` | `corr0`/`corr1` の列からロゴ表示区間を判定し logoframe 形式で出力。Amatsukaze `LogoScan.hpp` の `LogoFrame::writeResult` を移植 | [cm-detection.md](cm-detection.md) |
| `logo/dict.rs` | 学習済み `.lgd` を辞書ディレクトリ（既定 `$XDG_DATA_HOME/tachikaze/logos`、未設定時 `~/.local/share/tachikaze/logos`）に蓄積し、対象映像と解像度が一致する候補をスコア（Amatsukaze `LogoFrame::selectLogo` 相当）で自動選択する | [cm-detection.md](cm-detection.md)、上記「パス解決」 |

**解析側（analyze）は mp4 の読み込みに依存しない。** `--report` が必要とするキーフレーム位置を `.dtvi` から取る設計にしてあるため。**この性質を崩さないこと**（キーフレーム位置を mp4 から取る実装に変えると解析とカットが結合する）。

## 型設計の要点

**表示順とデコード順は別の型。** この 2 つの混同が唯一の重大バグ源であり、混同するとエラーを出さずに間違った位置で切る。

```rust
struct DisplayIdx(u32);   // 表示順（Trim / .dtvi の frame_number）
struct DecodeIdx(u32);    // デコード順（mp4 のサンプル番号 / .dtvi の sample_number）
```

相互変換は明示的な関数だけを通す（`OrderMap::to_display` / `to_decode`）。

## 自己検証（cut の手順6）

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
9. `analyze --logo` 指定時: ffmpeg から読み取ったロゴ矩形のフレーム数が `.dtvi` の `frame_count` と一致するか（不一致なら明示エラーで停止）。検査の実体は `src/logo/frames.rs::stream_luma_frames`。ロゴ検出は `cut` とは別の ffmpeg 供給経路を使うため、上記 4 とは別に必要な検査
10. **自動推定（`--logo`/`--no-logo` 両方省略）でも罠3の防御は緩んでいない**。候補の推定（`logo/estimate.rs::estimate_candidates`）は `stream_keyframe_luma_frames` を使う。ロゴ辞書候補の採点（`logo/dict.rs::select_candidate`）も同じ関数を使う。この関数は意図的にフレーム数一致検査を持たない（キーフレームだけを読むため `.dtvi` の `frame_count` とは一致しない）。候補の推定は `classify_sample` が「標本の通し番号」で `.dtvi` 由来の配列を引くため、フレーム数がずれると静かに誤ったラベルを返しうる。そのため代わりに `verify_keyframe_count_matches_dtvi`（`src/analyze.rs`）で、ffmpeg が実際に流したキーフレーム数と `.dtvi` のキーフレーム数を突き合わせる。**辞書候補の採点（`dict::select_candidate`）にはこの突き合わせが無い。** `corr0`/`corr1` の平均を取るだけで通し番号を配列の添字に使わないため、フレーム数のずれが起きても「候補を1つ見送る」以上の実害が無い。**候補が決まった後の学習（`scan::run`）と検出（`detect_logo`）は、明示 `--logo` 指定時と同じ `stream_luma_frames` を通る**ため、上記9の検査を必ず経由する。推定用の供給関数が検出経路から直接呼ばれることはない。
