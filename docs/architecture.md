# 構成と未解決事項

→ 入口: [overview.md](overview.md)

**`analyze` / `cut` / `prepare` / `remap-subs` / `auto` の実装は完了している。** エピック E1〜E11 とそれぞれのサブ issue に分解したタスクはすべてクローズ済み（経緯は `git log` の `[E1-1]`〜`[E11-7]`。E11 は字幕の保持と `auto`、#56）。この文書は**現在の構成**と、**まだ対応していないこと**を書く。

## コマンド構成

**解析とカットは別のコマンドに分かれている。** 検出の見逃しが実際に起きるため（[jls-settings.md](jls-settings.md)）、目視確認と手動修正を挟めるようにしてある。

`tachikaze --version` でバージョンを表示する（#71、`Cargo.toml` の version をそのまま出す）。

```
tachikaze prepare IN.mp4 [--subs PATH]
    0. elst(edit list) 除去・字幕トラック除去・字幕抽出を1回の ffmpeg 呼び出しに
       まとめる（#58）。`cut` が elst 付き / 字幕トラック付き入力を明示エラーで
       拒否するため（下記「未対応の入力」）、その回避策をここに集約する。
       出力は入力ごとのキャッシュディレクトリへ（下記「パス解決」節）
       elst も字幕も無ければ ffmpeg を呼ばず入力をそのまま返す
       映像2本以上・音声2本以上は明示エラーで停止する（`-map 0:v:0 -map 0:a:0`
       固定のため、黙って1本目だけ残すと cut が本来拒否すべき構成を素通しさせて
       しまう。`check_track_counts` と同じ制約、`src/prepare.rs` の doc comment
       「複数トラックの扱い」参照）。字幕トラックが2本以上ある場合はエラーに
       せず警告のうえ先頭の1本のみ抽出する

tachikaze analyze IN.mp4 [-o trim.avs|-] [--report] [--cache-dir DIR]
                                     [--jls-set KEY=VALUE]... [--jl-file FILE]
                                     [--logo FILE.lgd]
    1. dtvindex build              （外部プロセス）
    2. chapter_exe                 （外部プロセス）
    3. join_logo_scp               （外部プロセス）
                                   -set autocm_sub 11 -set param_cuttr 1 を既定付与
                                   `--logo` 指定時は 2 と 3 の間で自前のロゴ検出
                                   （`.lgd` を読み、ffmpeg でロゴ矩形のフレームを
                                   流して区間判定、`src/logo/`・`src/analyze.rs`
                                   の `detect_logo`）を行い、検出フレーム割合が
                                   閾値（既定 0.1、映像長7分以下は 0.03。
                                   Amatsukaze `CMAnalyze.hpp:301` と同じ規則）
                                   以上、かつ logoframe テキスト（`logo::interval`
                                   の `build_text`）が空でない場合にだけ logoframe
                                   をキャッシュへ書いて `-inlogo` を 3 に渡す
                                   （`-set` 群より前に置く。E14-8、#97）。割合が
                                   閾値未満、または割合は閾値以上でも精緻化で
                                   text が空になった場合は `-inlogo` を渡さず
                                   3 へ進む（誤ったロゴ情報で判定を崩すより現状
                                   維持。`logo_frames`（判定の数え上げ）と
                                   `text`（`build_text` の出力）は別経路のため、
                                   割合だけでは text が空のケースを見落とす。
                                   `src/analyze.rs::inlogo_decision`）。フォール
                                   バック時はキャッシュに残る古い logoframe.txt
                                   を削除する。`.dtvi` の `frame_count` と読み取った
                                   フレーム数が食い違う場合は 3 を実行せず中断
                                   する（CLAUDE.md 罠3、この検査は省略不可）
    4. --report で以下を stderr に出力
       ・各カット境界とキーフレームの距離
       ・余分に残る合計秒数
       ・見逃し候補の警告（既知の CM ブロック長と一致する未カット区間）
       ・gate 判定（見逃し候補・除去フレーム数0のどちらかで「疑わしいので止める」、
         保持率・格子誤差ずれは参考値のみ。`src/gate.rs`、#61）。`auto` はこの判定を
         使って cut するかどうかを機械的に決める（下記参照）

    `-o` は省略できる（キャッシュにだけ書き、その場所を stderr へ案内する）。
    `-o -` は `trim.avs` を標準出力へ書く（stdout はこの内容だけで、進捗・
    レポート等の診断はすべて stderr。`tachikaze analyze IN.mp4 -o - > trim.avs` や
    `tachikaze analyze IN.mp4 -o - | tachikaze cut IN.mp4 --trim - -o OUT.mp4` のようにパイプへ
    流せる。#72）

（必要なら trim.avs を人手で編集）

tachikaze cut IN.mp4 --trim trim.avs|- -o OUT.mp4 [--dtvi work.mp4.dtvi] [--cache-dir DIR]
                                     [--snap outward|inward] [--cm-output CM.mp4]
                                     [--video-only] [--verify] [--segment-map PATH]
    `--trim -` で標準入力から Trim リストを読める（`analyze -o -` の出力を
    そのまま渡せる。#72）
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
   12. 自己検証を通り最終出力へ rename できた後、保持側の区間マップ（snap 後の境界と
       出力タイムライン上の開始時刻。外部で作った字幕やチャプターを cut 後のタイムライン
       に合わせるための中間データ、#57）をキャッシュ（work.mp4.segmap.json）へ書く。
       --segment-map PATH で任意の場所にも書ける。--cm-output 指定時も保持側だけに出す。
       処理を始める前（手順5より前）に既定キャッシュパスの古い区間マップは削除して
       おり、自己検証を通って新しいマップを書けた場合だけキャッシュにマップが残る
       （以前は cut が失敗しても古いマップが残ったままになり、remap-subs が鮮度
       チェックなしにそれを使ってしまう問題があった）

tachikaze remap-subs IN.mp4 [--segment-map PATH] [--subs PATH] [-o OUT.ass|OUT.srt]
   13. 区間マップ・字幕サイドカー（ASS/SRT）をキャッシュから自動解決
       （`--dtvi` と同じ規則。明示指定が最優先）
   14. 区間マップの区分的な線形写像でイベントの Start/End を張り替える
       （`output_t = output_start_k + (source_t - source_start_dts_k)`、#59）
   15. 各イベントを 保持区間に完全一致=シフト / どの保持区間とも重ならない=破棄
       （CM に完全に含まれる字幕を残さない） / 境界を跨ぐ=クリップ に分類し、
       件数を必ずログに出す。時刻以外のフィールド・行はそのまま素通しする
       （`src/subtitle.rs`）

tachikaze auto IN.mp4 -o OUT.mp4 [--cm-output CM.mp4] [--ignore-gate]
                                [-f|--force] [--analyze-only] [--no-subtitles]
                                [--snap] [--verify] [--jl-file] [--jls-set] [--cache-dir]
                                [--logo FILE.lgd]
   16. prepare(0) → analyze(1〜4) → gate 判定 → cut(5〜12) → remap-subs(13〜15) を
       対話なしで合成する（`src/auto.rs`、#62）。アルゴリズムは持たない: 各ステップは
       上記の関数・処理をそのまま呼ぶ（`commands::execute_cut` を `cut` サブコマンドと
       共有。詳細は `src/commands.rs` / `src/auto.rs` の doc comment）
   17. gate が「疑わしいので止める」と判定したら cut せず exit code 3 で停止し、
       trim.avs のパスと「直して cut する」コマンド例を出す（`--ignore-gate` で
       無視できるが、無視できるのは gate の判定だけで自己検証や `.dtvi` 必須は
       変わらない）。`--analyze-only` を付けた場合は `--ignore-gate` の有無に
       関わらず cut へ進まないため、gate が疑わしいと判定していれば exit code
       は 3 のままになる（無視の対象は「cut へ進むかどうか」で、停止コード
       そのものではない）
   18. 1プロセスにつき入力は1本（#70）。複数ファイルを処理する場合はシェルの
       ループに任せる（`for f in *.mp4; do case "$f" in *_CMcut.mp4) continue;; esac; tachikaze auto "$f" -o "${f%.mp4}_CMcut.mp4"; done`。
       出力名を `_CMcut.mp4` サフィックス付きに固定し、`*.mp4` の glob が
       前回の出力を再び入力として取り込まないよう `case` で弾く）。
       1入力1プロセスにすることで、exit code の意味が「その1本に対する答え」に
       一意になる（下記の表参照）
   19. `-o` は必須（出力先を暗黙に決めない）。CM側は `--cm-output` を指定した
       ときだけ出す（未指定なら作らない）。本編・CM側・字幕サイドカー（`-o` と
       同じ stem の `.ass`/`.srt`）のいずれかが既に存在すれば既定でその入力を
       スキップする（`-f`/`--force` で上書き。`cp -f` の慣習に合わせた改名、#73）。
       ただし字幕トラックがある入力で字幕サイドカーだけ欠けている場合は、
       本編/CM側が揃っていてもスキップせず再試行する（前回 `remap-subs` が
       失敗した状態を次回実行で自動的に直すため、`src/auto.rs` の doc
       comment「既存出力のスキップと -f/--force」参照）。`analyze` はキャッシュ
       があっても毎回実行する（キャッシュキーが入力の
       絶対パスのハッシュだけで、内容の変化を検出できないため。`src/auto.rs`
       の doc comment参照）

tachikaze make-logo IN.mp4 --rect x,y,w,h -o OUT.lgd [--threshold N]
   20. ロゴ検出に使う `.lgd`（Amatsukaze 形式ロゴデータ）を、入力 mp4 とロゴ矩形
       だけから作る（`.dtvi` も外部3ツールも使わない、E14-6、#95）。ロゴ位置は
       CLI の `--rect`（`x,y,w,h`、2の倍数に丸める）で手動指定する。位置を自動
       探索しないのは、ロゴの形・色は局や番組ごとに異なり、対象素材だけから
       汎用的に検出する既存手段（Amatsukaze 側にも自動探索は無い）が無いため
   21. 矩形の外周1ピクセルが単色（最小値・最大値の差が `--threshold`、既定12、
       以下）のフレームだけを学習に使い、画素ごとに最小二乗で回帰係数 `a`/`b` を
       求める（`src/logo/scan.rs`）。入力全体を既定で走らせる（CM区間だけを
       指定すると「ロゴが無い」ロゴデータができてしまうため）
   22. 有効フレーム数（何フレーム中いくつ使ったか）を必ず stderr に出す。0件、
       4件未満（`MIN_USABLE_FRAMES`。少数点では回帰係数がNaN/infにならずに
       黙って有限値になりうるため、件数そのものを別に検査する）、または係数が
       NaN/inf/`a==0` になった場合は失敗させる（壊れたロゴデータを黙って
       書き出さない）
```

### exit code

| code | 意味 |
|---|---|
| 0 | 完了（`auto` が既存出力を検出してスキップした場合も 0。失敗でも判定停止でもないため） |
| 1 | エラー |
| 2 | 引数の誤り（clap の既定。実測: `tachikaze --bogus` → exit 2） |
| 3 | `auto` の gate が疑わしいと判定して停止（`analyze`/`cut`/`prepare`/`remap-subs` はこの値を返す経路を持たない） |

**なぜ gate 停止が 2 ではなく 3 なのか**: clap が引数の誤り（usage error）に使う exit code が 2 であるため（実測、上記表参照）。空いている最小の番号が 3 になる（#71）。

`.dtvi` はオープン GOP の判定（[lossless-cut.md](lossless-cut.md)）と自己検証 4（表示順/デコード順の突き合わせ）に必須で、これ自体は変わっていない。省略できるようにしたのは**パスの指定**だけで、`.dtvi` 無しで動くようにしたわけではない（`cut --dtvi` を省略すると、`analyze` と同じ入力ごとのキャッシュディレクトリ規則から `work.mp4.dtvi` を自動的に探す。見つからなければ `analyze` を実行するコマンド例を添えて停止する。探索順・キャッシュの場所は次節「パス解決」参照）。

**なぜ `auto` の入力を1本に絞ったのか**: 複数の入力ファイルを1コマンドで並べて渡す形を受け付けていた時期があったが、1プロセスの exit code が「N本中M本失敗」という集計に潰れてしまい、スクリプトから「その入力がどうなったか」を一意に読み取れなかった。1プロセス1入力にして、繰り返しをシェル（`for` / `xargs -n1`）に任せることで、exit code の意味を一意にした（#70）。

**なぜ `-f`/`--force` が上書きの意味なのか**: 以前は「gate の判定を無視する」意味のフラグに `--force` を、「既存出力を上書きする」意味のフラグには別の長いオプション名を使っていた。`cp -f` / `rm -f` の慣習では `-f`/`--force` は「上書き」を指すため、`--force` が「上書きする」と誤読されやすかった。gate 無視は `--ignore-gate` に改名し、`-f`/`--force` を慣習どおり「上書き」の意味に統一した（#73）。

**なぜ `auto -o` を必須にしたのか**: 以前は `-o` を省略すると `<stem>_CMcut.mp4` を暗黙に導出していた。出力先は暗黙に決めるより明示させる方が自然で、CM側も `--cm-output` の指定有無だけで決まるようにしたことで、CM側出力の有無だけを切り替える専用フラグが不要になった（#73）。

**なぜ診断を stderr に寄せたのか**: 以前は進捗・警告・レポート等の診断と `analyze` の `trim.avs` 本体が両方 stdout に混在していた。UNIX の作法（stdout はデータ、stderr は診断）に合わせて診断をすべて stderr に移し、空いた stdout を `analyze -o -` で `trim.avs` をパイプに流すために使えるようにした（#72）。

**かつては「CLI に `auto` は用意していない」方針だった**（検出の見逃しがあるため、`analyze` と `cut` のあいだに人手を挟める設計を崩さない、という判断）。**この方針を変えて `auto`（#62）を追加した理由は3つ**:

1. **やり直しが安い**: `analyze` の中間ファイル（`.dtvi` / `trim.avs` / `detail.jls`）はキャッシュに残り、入力 mp4 自体は無改変（`prepare` の出力もキャッシュに書くだけで `IN.mp4` を書き換えない）。`auto` が誤った判定で走っても、後から `cut` を直接叩き直すだけで直せる（`auto --analyze-only` が出す `cut` コマンド例を使う）
2. **事後確認の手段がある**: `--cm-output` で CM 側を別ファイルに出せるため、`auto` が黙って本編を欠損させていないかを後から目視できる
3. **機械可読な判定材料がある**: gate（#61）が `analyze` の成果物だけから「見逃し候補」「除去フレーム数0」を機械的に判定できるようになったため、「疑わしいときだけ人手を呼ぶ」を自動化できる

**`tachikaze auto`（#62）は gate（#61）のこの判定を使って人手を安全に外す。** 見逃し候補ヒューリスティックが効かない番組もあるため（`src/gate.rs`「見逃し候補ヒューリスティックの限界」）、gate が止めないことは検出が完全に当たっている保証ではない。対話しながら都度確認したい場合は従来どおり `analyze` → 目視 → `cut` を使う。

## パス解決

インストールして（`/usr/local/bin` などに置いて）使う場合を含め、パスの決め方は**実行ファイル / 読み取り専用データ / キャッシュ / 出力**の4分類ごとに変える。配置手順は [toolchain-macos.md](toolchain-macos.md)「ビルド後の配置とインストール」。

| 種類 | 中身 | 探索順・既定 |
|---|---|---|
| 実行ファイル | `tachikaze` / `chapter_exe` / `join_logo_scp` / `dtvindex` / `ffmpeg` / `ffprobe` | `PATH` のみ（`src/tools.rs::resolve_tool`）。別の場所に置いているものを使いたければ `PATH=/opt/jls/bin:$PATH tachikaze ...` のように前置する |
| 読み取り専用データ | JL コマンドファイル（既定 `JL_標準.txt`） | `--jl-file` → `<join_logo_scp の実体パス>/../../share/join_logo_scp/JL/`（`make install` 配置前提の1段のみ、`src/tools.rs::default_jl_command_file`） |
| キャッシュ（再生成可能な中間物） | `work.mp4.dtvi` / `trim.avs` / `detail.jls` / `work.mp4`（入力への symlink） / `work.mp4.segmap.json`（`cut` が書く区間マップ、`src/segmap.rs`、#57） / `input_prepared.mp4`（`prepare` が elst 除去・字幕除去後に書く前処理済み入力、`src/prepare.rs`、#58） / `subs.ass`・`subs.srt`（`prepare` が mp4 内蔵字幕トラックから抽出した字幕サイドカー。`remap-subs` の入力） | `--cache-dir`（グローバルオプション、キャッシュの根） → 既定 `<ホームディレクトリ>/.cache/tachikaze/`（`std::env::home_dir()` から決まる。ホームが特定できない場合は `--cache-dir` を促すエラーで停止する）。いずれの根からも入力ごとに `<根>/<入力絶対パスのハッシュ>-<stem>/` を使い、削除せず、同じ入力を再実行すると再利用する（`src/workdir.rs`）。`cut --dtvi` 省略時もこの規則から `work.mp4.dtvi` を自動的に探す。`work.mp4.segmap.json` / `input_prepared.mp4` / `subs.*` も同じ規則（`workdir::cached_segment_map_path` / `workdir::prepared_input_path` / `workdir::subs_path`）で、`cut --segment-map PATH` で区間マップだけは任意の場所にも書ける |
| 出力 | `cut -o` / `auto -o`（本編、必須） / `--cm-output`（CM側、`auto` は指定時のみ） / `*_CMcut.ass`・`*_CMcut.srt`（`remap-subs` 単体実行時の既定出力、`src/commands.rs::default_remap_subs_output_path`、#59） | いずれも明示指定（`cut`/`auto` の `-o` は必須、#73）。`remap-subs` を単体で使うときだけ入力の隣に `*_CMcut.<ext>` を既定で置く。`auto` の字幕サイドカーは `-o` と同じ stem・別拡張子（`src/auto.rs::subs_sidecar_path`）で、本編出力と揃えることでプレイヤーが自動で字幕を読み込める |

`--jl-file` / `--cache-dir` / `--dtvi` を明示指定した場合は、いずれも上記の探索より最優先でそのまま使う。

**なぜこの形にしたか**:

- **中間物をキャッシュに置いた理由**: `.dtvi` / `trim.avs` / `detail.jls` はいずれも `analyze` を再実行すれば作り直せる。`dtvindex` / `chapter_exe` / `join_logo_scp` はいずれも既存の出力先を実害なく上書きすることを実バイナリで確認済みなので、消えても実害なく復旧できるデータとしてキャッシュ扱いにし、既定で残すことで `analyze` → `cut` を `--cache-dir` / `--dtvi` の手打ちなしで繋げられるようにした。ディレクトリ名（`~/.cache/tachikaze`）自体は XDG Base Directory 仕様の既定と同じものを借りている
- **XDG 由来の環境変数を読まないと決めた理由（E12-2）**: 置き場所を決める口を `--cache-dir`（キャッシュ）・`--jl-file`（JL）のように引数に一本化し、環境変数の値によって挙動が変わらないようにした。ディレクトリ名だけ XDG の既定（`~/.cache`）を借りているが、環境変数側は一切読まない（`src/workdir.rs::default_cache_root` の doc comment）
- **ホームディレクトリを `std::env::home_dir()` で取る理由**: `$HOME` 環境変数の読み取りではなく OS への問い合わせなので、`$HOME` が unset な環境でも既定値を作れる場合がある。実測（`src/workdir.rs::default_cache_root` の doc comment）: rustc 1.97.1 時点で非推奨警告は出ず、Unix では `$HOME` が unset でも `getpwuid` 経由でホームディレクトリを引ける。実際にホームが取れない（＝エラーになる）のは「呼び出しユーザーの passwd エントリすら無い」環境（コンテナで存在しない UID として動かす等）に限られる
- **ホームディレクトリが特定できないときにエラーで止める理由**: 黙って一時ディレクトリ等の別の場所へフォールバックすると、キャッシュが知らない場所に増えるだけでなく、別プロセスが別の一時領域を引けば同じ入力に対して別のキャッシュディレクトリを掴み、`analyze` → `cut`（`--dtvi` 省略）の受け渡しが**エラーを出さずに**外れる。`--cache-dir` を促すエラーで明示的に止める方が安全と判断した
- **外部ツール専用の配置ディレクトリを指定するオプションと「自分の実行ファイルの隣」を削った理由（E12-1）**: `PATH=/opt/jls/bin:$PATH tachikaze ...` のように `PATH` を前置すれば同じことができ、インストールしたくない場合は Docker イメージ（[docker.md](docker.md)）がある。口を1つに絞ることで、複数の口が同時に設定されたときどちらが効くか読まないと分からない状態を無くした
- **作業ディレクトリを明示するオプションと使い捨て一時ディレクトリに戻すオプションを `--cache-dir` に統合した理由（E12-2）**: どちらも「キャッシュの置き場所」という同じ軸の粒度違いにすぎない。使い捨てにしたい場合は `--cache-dir "$(mktemp -d)"` で足りるため、専用オプションを別に持つ必要がない
- **JL ファイルの探索を1段だけにした理由（E12-1）**: かつては環境変数によるディレクトリ指定・XDG のデータディレクトリ群・prefix 相対の `share/`・`join_logo_scp` と同じディレクトリの `JL/`（1ディレクトリ配布との後方互換）まで5段あった。`make install` 配置（[toolchain-macos.md](toolchain-macos.md)「ビルド後の配置とインストール」）に一本化したことで、1ディレクトリ配布の後方互換段は不要になった。個別配置にしたい場合は `--jl-file` で直接指定する
- **出力だけキャッシュの探索規則から外した理由**: 出力は録画ファイルと同じ場所で管理したいという運用上の要求があり、消えても再生成できるキャッシュとは性質が違う（ユーザーの成果物）ため対象外にした。`cut`/`auto` は `-o` で出力先を明示指定させる（#73）。`remap-subs` を単体で使うときだけ、利便性のために入力の隣へ既定の出力名を置く（`src/commands.rs::default_remap_subs_output_path`）
- **設定ファイル用の置き場所を用意しなかった理由**: 現時点で設定ファイルに載せる項目がなく、XDG の config ディレクトリ相当は使っていない。必要になるまで空けておく方針

## モジュール構成

**この分割はファイル所有権の単位でもある。** 1 つの関心事が 1 ファイルに収まるように決めてあるので、統合・分割するときは理由を持って行うこと。

| ファイル | 責務 | 参照 |
|---|---|---|
| `cli.rs` | サブコマンドとオプションの定義 | — |
| `commands.rs` | 各モジュールを繋ぐ組み立て（アルゴリズムは持たない） | — |
| `auto.rs` | `auto` コマンドの組み立て（`prepare`→`analyze`→gate→`cut`→`remap-subs`、アルゴリズムは持たない） | 上記「コマンド構成」手順16〜19 |
| `tools.rs` | 外部ツールと JL ファイルの探索 | 上記「パス解決」節、[toolchain-macos.md](toolchain-macos.md) |
| `external.rs` | 外部プロセスの起動と出力の回収 | [pipeline.md](pipeline.md) |
| `ffprobe.rs` | ffprobe への問い合わせ（CSV 行を返す `csv_rows` / 単一スカラー値を返す `scalar_entry`。`-show_entries` / `-show_data_hash CRC32`）を1か所に集約する責務 | 罠2（md5 ではなくパケット単位の CRC32）、[lossless-cut.md](lossless-cut.md) |
| `errctx.rs` | 「〜に失敗しました: <パス>」というエラー文脈の付与を1行で書くための拡張トレイト（`path_ctx`） | — |
| `workdir.rs` | 作業ディレクトリ（既定は入力ごとのキャッシュ）と symlink | 上記「パス解決」節 |
| `order.rs` | `DisplayIdx` / `DecodeIdx` | 下記「型設計の要点」 |
| `trim.rs` | `Trim(a,b)++…` のパース / 生成（半開区間 `[s, e+1)` に正規化） | — |
| `dtvi.rs` | `.dtvi` のパース（タブ区切りテキスト） | [pipeline.md](pipeline.md) |
| `jls.rs` | `detail.jls` のパース | [pipeline.md](pipeline.md) |
| `analyze.rs` | analyze コマンドの組み立て | [jls-settings.md](jls-settings.md) |
| `prepare.rs` | `prepare` 本体: elst 除去・字幕トラック除去・字幕抽出を1回の ffmpeg 呼び出しにまとめる | 上記「コマンド構成」手順0 |
| `gate.rs` | `analyze` の成果物（`TrimList`/`JlsEntry`/`Dtvi`）だけから検出結果が機械的に疑わしいか判定する（mp4 は読まない） | 上記「コマンド構成」手順4・16〜17 |
| `report/mod.rs` | `--report` の出力 | [measurements.md](measurements.md) |
| `report/missed.rs` | 見逃し候補の警告 | [jls-settings.md](jls-settings.md) |
| `mp4io/read.rs` | サンプル表の読み込み | [mp4-atom.md](mp4-atom.md) |
| `mp4io/order_map.rs` | 表示順↔デコード順の写像、合成時刻の算出 | [pipeline.md](pipeline.md) |
| `mp4io/support.rs` | 未対応構成の判定（下記「未対応の入力」を早期に落とす） | 下記 |
| `mp4io/write.rs` | mp4 の書き出し（stts/ctts の圧縮、インターリーブ、co64） | [mp4-atom.md](mp4-atom.md) |
| `plan.rs` | キーフレーム境界へのスナップ、区間の計画、保持区間の補集合 | [lossless-cut.md](lossless-cut.md) |
| `audio.rs` | 音声パケットの選択（区間のソース時刻から引き当て） | [lossless-cut.md](lossless-cut.md) |
| `verify.rs` | 自己検証 | 下記 |
| `segmap.rs` | `cut` が書く区間マップ（snap 後の境界と出力タイムライン上の開始時刻）の構造体、JSON への書き出しと読み込み | 上記「コマンド構成」手順12 |
| `subtitle.rs` | `remap-subs` 本体: ASS/SRT の Start/End を区間マップの区分的な線形写像で張り替える（シフト/破棄/クリップの分類、丸め方向） | 上記「コマンド構成」手順13〜15 |
| `logo/lgd.rs` | Amatsukaze 形式ロゴデータ `.lgd`（AviUtl 互換のベース部 + Amatsukaze 独自の float 部）の読み込み | [E14](https://github.com/fetburner/Tachikaze/issues/89) |
| `logo/frames.rs` | ffmpeg を子プロセスとして起動し、ロゴ矩形の輝度平面をフレーム順にストリームで読む。読み取ったフレーム数と `.dtvi` の `frame_count` の一致検査 | [E14](https://github.com/fetburner/Tachikaze/issues/89) |
| `logo/score.rs` | ロゴマスク生成と相関スコア（`corr0`/`corr1`）。Amatsukaze `LogoScan.hpp` の相関方式を移植 | [E14](https://github.com/fetburner/Tachikaze/issues/89) |
| `logo/scan.rs` | `make-logo` 本体: 外周1ピクセルが単色のフレームだけを使い、画素ごとに最小二乗で `.lgd` の係数 `a`/`b` を求める。`.lgd` の書き出し（ベース部はゼロ埋め） | 上記「コマンド構成」手順20〜22、[E14](https://github.com/fetburner/Tachikaze/issues/89) |
| `logo/interval.rs` | `corr0`/`corr1` の列からロゴ表示区間を判定し logoframe 形式で出力。Amatsukaze `LogoScan.hpp` の `LogoFrame::writeResult` を移植 | [E14](https://github.com/fetburner/Tachikaze/issues/89) |

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
9. `analyze --logo` 指定時: ffmpeg から読み取ったロゴ矩形のフレーム数が `.dtvi` の `frame_count` と一致するか（`src/logo/frames.rs::stream_luma_frames`、不一致なら明示エラーで停止。ロゴ検出は `cut` とは別の ffmpeg 供給経路を使うため、上記 4 とは別に必要な検査。[E14](https://github.com/fetburner/Tachikaze/issues/89) の「静かに壊れる仕組み」と同種の防御）

## 未対応の入力（`mp4io/support.rs` が明示エラーで落とす）

**いずれも「静かに壊れる」より「止まる」を選んでいる。** 音声コーデック自体は `mp4-atom` が認識する音声 Codec 全般（Opus / AAC など）を同じ経路で扱う。ここに残すのはコーデックと無関係に未対応と判断した構成。

| 構成 | 判定関数 | 理由と回避策 |
|---|---|---|
| `elst`（edit list）あり | `check_no_edit_list` | `moov.clone()` で引き継いだ `elst` の `segment_duration` がカット後のトラック長を超え、`media_time` が新しい先頭の正当なフレームを数フレーム分スキップする——「エラーは出ないが結果が壊れる」の実例を実験で確認済み。**回避策**: `ffmpeg -i IN.mp4 -c copy -use_editlist 0 -movflags +faststart OUT.mp4`（除去後の映像・音声パケットが CRC32 でビット一致することを確認済み。**ただしこれはペイロードのみの確認であり、A/V の相対タイムスタンプは対象外**。除去が A/V 相対時刻に与える影響の実測と方針は [measurements.md](measurements.md)「elst 除去と A/V 相対時刻」） |
| `stsd` が複数エントリ | `check_single_stsd_entry` | `write.rs` が `sample_description_index: 1` 固定で `stsc` を再構築しているため。対応にはサンプルごとの index 保持（`read.rs` の `SampleInfo` と `write.rs` の両方の変更）が必要。無劣化 remux ではパラメータ差異という原因自体を解消できないので、事前除去の回避策は提示できない |
| オープン GOP | `check_closed_gop` | 「S の同期サンプルからデコード順に `E - S` パケット取る」規則が成立しない。`.dtvi` の `leading_frame_count` で判定する（`.dtvi` が無い場合も判定不能として停止） |
| 映像 2 本以上 / 音声 2 本以上 | `check_track_counts` | 映像 1 本 + 音声 1 本のみ対応。`prepare`（`src/prepare.rs::reject_multiple_video_or_audio_tracks`）も同じ制約を入口で課す。`prepare` は `-map 0:v:0 -map 0:a:0` 固定で ffmpeg に渡すため、この検査が無いと二重音声放送のような入力でも黙って1本目だけを残し、`cut` が本来拒否すべき構成を素通しさせてしまう |
| 字幕などのトラックあり | `check_track_counts` | **トラックとしては未対応のまま**（`cut` に直接渡すと明示エラーで止まる）。`prepare`（#58）が `cut` へ渡す前に字幕トラックを ASS/SRT のサイドカーへ抽出し、mp4 側からは除去する（下記「コマンド構成」手順0、以下「方式A」）。字幕トラックをそのまま `cut` に持たせてサンプル単位でコピーする方式（「方式B」: トラックごとの区間選択、字幕のようにサンプルが疎なトラックの扱いが別途要る）は採らなかった。**理由**: (1) ARIB 字幕をデコードした一部のスタイル情報（色・位置などの装飾）は `mov_text` のようなプレーンテキスト中心の字幕コーデックに変換すると失われるため、トラックとして持たせても方式Bだけでは解決しない、(2) 方式Bは対応工数が方式Aより一桁大きい（トラックごとの区間選択とサンプルが疎なトラックの扱いを `mp4io` 全体に持ち込む必要がある）。サイドカー化した字幕の cut 後タイムラインへの追従は `remap-subs`（#59）が区間マップから計算する |

## 未解決事項

| 項目 | 状況 |
|---|---|
| 複数音声トラック | **未対応**（上表のとおり明示エラー）。対応するならトラックごとの区間選択を実装する必要がある |
| 字幕のトラック対応 | `cut` にトラックとして持たせる方式（方式B）は**未対応のまま**（上表「未対応の入力」参照）。`prepare` のサイドカー方式（方式A）で実用上の字幕保持は満たしているため、方式Bを実装する優先度は無い |
| 継ぎ目の MDCT 過渡（クリックノイズ） | **許容する方針で決定済み。** 継ぎ目は残存 CM マージンの内側に来る（[lossless-cut.md](lossless-cut.md)） |
| `prepare` の elst 除去による AAC の残存ずれ | **許容する方針で決定済み（#60 実測）。** elst 除去により、保持した最初の区間がソースの先頭フレームから始まる場合に限り、音声の priming 分（実測 21.333ms、他エンコーダでは見積もりで最大43ms程度）がそのまま音声として残る。ファイルにつき高々1回・1フレーム未満・非累積のずれで、`prepare`/`auto` は elst を自動除去してよいと判断している（[measurements.md](measurements.md)「elst 除去と A/V 相対時刻」）。Opus は `dOps` がコーデックレベルで pre-skip を伝えるため影響なし |
| `--snap inward` と `--cm-output` の併用 | **拒否する。** inward では保持区間が退化（`end < start`）しうるため補集合の順序も壊れる |
| `mp4io::read` のテストが並列実行で稀に落ちる | **未対応。** `src/mp4io/read.rs::tests::ffprobe_packets` が ffprobe の**起動失敗**を `.expect("ffprobe を起動できること")` で panic させる。多数の ffprobe を同時に起動すると稀に起動に失敗し、テストが落ちる（master で8回中4回再現）。同モジュールには起動できない場合をスキップ扱いにする `skip_if_ffprobe_missing` があるので、`ffprobe_packets` の起動失敗も同じ扱いにすれば直る。テストだけの問題で、製品コードには影響しない |
| `analyze` のテストが `--include-ignored` で競合する | **未対応。** `src/analyze.rs::tests::analyze_run_produces_trim_list_with_real_tools` は `TOOL_PATH_ENV_LOCK` を取らずに実 `PATH` からツールを解決する一方、同モジュールの他3テストはそのロックの下でプロセス全体の `PATH` を差し替える。並列に走ると前者が差し替え後の `PATH` を読んで落ちうる。`--ignored` 単独では他3テストが走らないため出ず、`--include-ignored` でのみ再現する。前者にも同じロックを取らせれば直る |
| キャッシュ鍵の弱さ | `<キャッシュの根>/<入力ごと>/`（既定 `~/.cache/tachikaze`、`--cache-dir` で変更可）のディレクトリ名は入力の**絶対パスのハッシュのみ**から決まる（`workdir::cache_dir_for_input`、FNV-1a）。同じパスに別内容のファイルが後から置かれても（録画ファイルの上書き・再利用）区別できず、古い `.dtvi` / `trim.avs` / `input_prepared.mp4` を新しい入力に対して誤って再利用しうる。`auto` は `analyze`/`prepare` を毎回作り直すことでこの穴を避けているが（`src/auto.rs` の doc comment）、`cut --dtvi` を省略してキャッシュから自動解決する経路には対策が無い。size + mtime の突き合わせなどの対策は、要求されていない現時点では追加しないと判断している（理由は `src/auto.rs` の doc comment参照） |
| `-CutMrgIn` / `-CutMrgOut` を CLI から渡す口 | **追加しないと決定（E14-9、#98）。** join_logo_scp の起動オプション（`-set` 変数ではない）で、シーンチェンジからロゴ表示開始/終了までの局固有の固定遅延を補正する。実測（BS11、`docs/jls-settings.md`「`-CutMrgIn`/`-CutMrgOut`」）では `JL_標準.txt` の自動判断（`-CutMrgWI 2`/`-CutMrgWO 2`）が既に妥当な値（`CutMrgIn=5 CutMrgOut=8`）を検出しており、`0`〜`20` の範囲で手動指定に変えても `trim.avs`/`detail.jls` に変化が無かった。**やらないと決めた理由**: 実測した1局では効果が確認できず、CLI にオプションを追加する（`--jls-set` と別の口が要る、`-set` ではないため）実装コストに見合う利得が無いと判断した。局によっては固定遅延が大きく効果が出る可能性は残るため、実際に精度不足が観測された局が出たら別 issue で対応を検討する |
| 見逃し候補ヒューリスティックとロゴ | `src/report/missed.rs` は `.jls` のラベルを見ずギャップ長の一致だけで判定するため、ロゴの有無でロジックは変わらない。実測 2 本（BS11・TOKYO MX2、`docs/jls-settings.md`「ロゴありでの見逃し警告」）ではロゴあり/なしで見逃し候補の件数に変化が無かった（どちらも 0 件）。**この issue ではコードを変えない。** 検出済み CM ブロック長が揃わない番組でロゴありを試した実測が無いため、一般に誤警告が増えないとは言えない |

## 方針として作らないもの

| 項目 | 理由 |
|---|---|
| CM 検出アルゴリズム | 既存ツール（chapter_exe → join_logo_scp）が担当 |
| 映像の再エンコード | 数秒の CM 残りを許容する方針 |
| 音声の再エンコード | 継ぎ目のノイズは残存 CM の範囲内 |
| `auto` の複数入力・ディレクトリ一括処理（glob 展開） | 1プロセス1入力にすると exit code の意味が一意になる（#70）。繰り返しはシェル（`for` / `xargs -n1`）の仕事 |
| ロゴ検出の fade（フェード区間そのもの）・TOP/BTM（片フィールドロゴ）の出力 | [E14](https://github.com/fetburner/Tachikaze/issues/89) の方針。Amatsukaze も出していない。カットはキーフレーム境界（GOP 120 = 約4秒）に丸めるため、数フレーム単位の境界精度は残存 CM マージンに吸収され利得が小さい（詳細は [cm-detection.md](cm-detection.md)「出力形式の要点」） |

**ただし「境界 GOP だけ再エンコードすればフレーム精度になる」ことは覚えておく。** 実測で境界あたり 4 秒（120 フレーム）分のエンコードで済む。30 分番組・8 境界なら 32 秒分。方針が変わったときの逃げ道として安い。

## 将来的な拡張候補

- **チャプター出力**: dtvindex に `create_join_logo_scp_chapters` があるのでそれを呼ぶだけ
- **局別 JL ファイルの選択**: 現状は `JL_標準.txt` 既定（`--jl-file` で差し替えは可能）
