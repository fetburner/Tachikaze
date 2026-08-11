# Tachikaze

mp4 に変換済みの録画ファイルを、**再エンコードせずに CM カット**するツール。CM 検出は既存ツール（chapter_exe → join_logo_scp）に任せ、本ツールは「Trim リスト → ロスレス出力」だけを担う。

**状態**: `analyze` / `cut` / `prepare` / `remap-subs` / `auto` は**実装完了**。エピック E1〜E11 とそれぞれのサブ issue はすべてクローズ済み（経緯は `git log` の `[E1-1]`〜`[E11-7]`。E11 は字幕の保持と `auto`、#56）。言語は Rust、mp4 の読み書きは `mp4-atom` クレート。

```console
$ tachikaze analyze IN.mp4 -o trim.avs --report
$ tachikaze cut IN.mp4 --trim trim.avs -o OUT.mp4
$ tachikaze auto IN.mp4 -o OUT.mp4   # prepare → analyze → gate → cut → remap-subs を対話なしで合成
$ for f in *.mp4; do case "$f" in *_CMcut.mp4) continue;; esac; tachikaze auto "$f" -o "${f%.mp4}_CMcut.mp4"; done   # 複数ファイルはシェルのループで（出力を *_CMcut.mp4 に固定して glob の再取り込みを避ける）
```

`--cache-dir` / `--dtvi` は省略可（既定 `~/.cache/tachikaze/<入力ごと>/` から自動的に繋がる。探索順は [docs/architecture.md](docs/architecture.md)「パス解決」節）。インストールして使う場合の配置先は [docs/toolchain-macos.md](docs/toolchain-macos.md)「ビルド後の配置とインストール」。外部3ツールを自分でビルドせず使いたい場合は [docs/docker.md](docs/docker.md)。

手元のファイルを一通しで処理するときは `tachikaze auto`。gate が疑わしいと判定したら cut せず exit code 3 で停止し、直して `cut` するコマンド例を出す。`--ignore-gate` で無視できるが、無視されるのは gate の判定だけ。引数の誤りは clap の既定 exit code（2）のまま、gate 停止だけを3に分けている。判断を挟みながら進めたい場合は従来どおり `analyze` → 目視 → `cut`。1プロセスにつき入力は1本で、繰り返しはシェルのループに任せる。

現在のコマンド構成・モジュール構成・自己検証の一覧は
[docs/architecture.md](docs/architecture.md)。

## ドキュメント

**[docs/overview.md](docs/overview.md) に目的別のルーティング表がある。** 必要な文書だけを開くこと。各文書は独立して読める。

```
docs/overview.md            文書索引。スコープと重要事実（入口は README.md）
docs/tech-stack.md          技術選定の根拠（却下した選択肢も）
docs/pipeline.md            処理の流れと外部ツールの入出力形式
docs/cm-detection.md        CM 検出の仕組み（背景知識）
docs/jls-settings.md        検出が外れたときの調整と既知の失敗モード
docs/lossless-cut.md        カット処理の実装知識
docs/mp4-atom.md            mp4 読み書きの検証済みコードと落とし穴
docs/toolchain-macos.md     外部ツールのビルド手順
docs/measurements.md        実測データ
docs/architecture.md        現在の構成・自己検証・未対応の入力・未解決事項
docs/docker.md              インストールせず Docker で使う方法
```

## テスト

```console
$ cargo test                  # 単体テスト
$ cargo test -- --ignored     # E2E（要 ffmpeg / ffprobe / 外部3ツールとフィクスチャ）
$ bash tests/fixtures/gen.sh   # フィクスチャ生成（コミットされていない）
$ npm ci && npm run docs-lint  # ドキュメントの文面検査（CI の docs-lint と同じ）
```

**ドキュメント（`README.md` / このファイル / `THIRD-PARTY-NOTICES.md` / `docs/` 以下）を編集したら `npm run docs-lint` を通すこと**。規則は `.textlintrc.json`（1文120字・読点5個までなど）。textlint の版は `package-lock.json` で固定してある。

外部3ツール（chapter_exe / join_logo_scp / dtvindex）の用意は [docs/toolchain-macos.md](docs/toolchain-macos.md)。**テストは3ツールが有る環境でも無い環境でも通る**（どちらか一方でしか通らない状態にしないこと）。「ツールが見つからない」ことを期待するテストは、ツールを `PATH` に入れた環境では前提が崩れる。そのため子プロセスの `PATH` を空にして自分で条件を作る。例は `tests/auto_e2e.rs` の `run_auto_without_tools`。`src/analyze.rs` の `run_propagates_tool_resolution_failure_with_searched_locations` も同様。

フィクスチャの**音声は時間変化する信号**にしてある（定常サイン波ではコーデックによって音声パケットが同一バイト列になり、罠 4 を検出できない）。`gen.sh` は Opus 版 `sample.mp4` と AAC 版 `sample_aac.mp4` を同じ映像条件で生成する。映像側のパラメータを変えると `tests/data/sample.dtvi` が使えなくなるので変えないこと。

## 静かに壊れる 4 つの罠

いずれも**エラーを出さずに間違った結果を生む**。コードを書く前に必ず確認すること。

1. **切り出しはパケット数で行う。時間指定（`ffmpeg -t`）は使わない。** B フレームの並べ替え深度ぶん余分に取り込み、表示順に穴が空くのにフレーム数は一見合ってしまう。正しい規則は「S の同期サンプルからデコード順に `E - S` パケット取る」。→ [docs/lossless-cut.md](docs/lossless-cut.md)

2. **無劣化の検証に md5 を使わない。** `h264_mp4toannexb` が IDR ごとに SPS/PPS を再挿入するため、バイト数が一致してもハッシュがずれる。ffprobe の `-show_data_hash CRC32` でパケット単位に比較する。→ [docs/lossless-cut.md](docs/lossless-cut.md)

3. **表示順とデコード順を型で分ける。** 混同が唯一の重大バグ源で、間違った位置で切っても例外は飛ばない。`.dtvi` のフレーム番号と自前導出の一致を assert する。→ [docs/architecture.md](docs/architecture.md)

4. **音声パケットは「区間先頭サンプルのソース上の DTS」から引き当てる。** 出力側の累積時間を起点に詰めると**長さは合うのに中身が別の位置の音声になる**。合成時刻（pts）を使うと `cts_offset`（並べ替え深度 = 実測 67ms）ぶん**音声が映像より先行する**。どちらも実際に起きた。パケットの集合比較も区間ごとの長さ比較も素通りするので、「元ファイルと出力で『映像 pts − 音声 pts』が保たれているか」を見る検査が必要。→ [docs/lossless-cut.md](docs/lossless-cut.md)

## タスク分解と issue

**タスク分解を頼まれたら、エピック 1 本 + サブ issue に分ける。** 形式は `.github/ISSUE_TEMPLATE/` の 2 つのテンプレート（`epic` / `sub-issue`）に従う。既存の issue を読んで形式を真似る必要はない。

```console
$ gh issue create --template epic --title '[E11] ...' --label epic
$ gh issue create --template sub-issue --title '[E11-1] ...' --parent 56 --label area:cli --label lane:mp4
```

- **サブ issue は自己完結させる。** 分割の目的は「実装する人が余計な文書を読まずに済むこと」であって、進捗管理ではない。**エピックや他の issue を読まないと実装できないサブ issue は分割が失敗している。** エピックの「方針」で決めた共通事項は、参照させずに必要な行を転記する
- **「読む文書」は節名・関数名まで絞る。** 挙げなかった文書は開かせない
- **「触るファイル」を必ず列挙する。** 並行して進めるときの衝突を防ぐ。1 ブランチで終わる粒度（触るファイルを列挙できる大きさ）に切る
- 本文の先頭に `親: #<エピック番号>` を書き、`--parent` でも紐付ける（本文だけ読んでも親が分かるようにする）
- ラベルはエピックが `epic` + `area:*` + `lane:*`、サブ issue が `area:*` + `lane:*`（ドキュメントのみなら `documentation`）

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
