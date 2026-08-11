# サードパーティ表記

Tachikaze 本体（このリポジトリのコード）のライセンスは [LICENSE](LICENSE)（MIT）。ここでは、本体が依存・利用する外部コードのライセンスと、それが Tachikaze 本体のライセンスに影響しない理由を記録する。

## 外部プロセスとして呼ぶツール

Tachikaze は次の外部ツールを**別プロセスとして起動するだけ**で利用する。

| ツール | ライセンス |
|---|---|
| [join_logo_scp](https://github.com/tobitti0/join_logo_scp) | GPLv2 |
| [chapter_exe](https://github.com/tobitti0/chapter_exe) | GPL 系 |
| [dtvindex](https://github.com/tobitti0/dtvindex) | GPL 系 |
| [ffmpeg](https://ffmpeg.org/) | GPL 系（ビルド構成による） |

これらは `src/tools.rs::resolve_tool` が `PATH` から実体（バイナリのパス）を見つけ、`src/external.rs::run` が子プロセスとして起動する。受け渡しはファイルとコマンドライン引数のみで、リンクも同一アドレス空間の共有もない。JL コマンドファイル（`join_logo_scp` の判定ルール）もリポジトリには同梱していない。`src/tools.rs::default_jl_command_file` が `join_logo_scp` のインストール先から実行時に解決するだけである。したがって GPL の派生物条件は Tachikaze 本体には及ばない。

Cargo 依存（`cargo metadata` で確認した tachikaze 自身を除く全 51 パッケージ）はすべて permissive で、GPL のクレートは無い。内訳は `MIT OR Apache-2.0` 39 / `MIT` 7 / `Apache-2.0 OR MIT` 3 / `Unlicense OR MIT` 1 / `(MIT OR Apache-2.0) AND Unicode-3.0` 1。

**Docker イメージを配布する場合は条件が付く。** 詳細と対応（commit SHA 固定の必要性）は [docs/docker.md](docs/docker.md)「既知の制約」。

## 移植したコード

次の 2 ファイルは、Amatsukaze（[nekopanda/Amatsukaze](https://github.com/nekopanda/Amatsukaze)）`LogoScan.hpp` の相関方式・`LogoFrame::writeResult` を移植したもの。`src/logo/score.rs` はロゴ相関スコア `corr0`/`corr1` の計算。`src/logo/interval.rs` は `corr0`/`corr1` からロゴ表示区間を判定し logoframe 形式で出力する処理。

- ライセンス: MIT
- 著作権表示: Copyright (c) 2017-2019 Nekopanda
- ライセンス全文: <http://opensource.org/licenses/mit-license.php>（原典のファイルヘッダに記載のリンク）

MIT ライセンスの義務は著作権表示とライセンス文の保持だけなので、上記と各ファイル冒頭の doc comment（同じ表示）で足りる。原典 `LogoScan.hpp` 内の `approxim_line()` / `GetAB()` / `med_average()`（MakKi 氏の delogo 由来でライセンス不明）は参照していない。

`src/logo/scan.rs`（E14-6、issue #95）が実装するロゴ学習アルゴリズムでも、Amatsukaze の `LogoScan::AddFrame` 系は同じ理由で参照していない。対象は `GetAB()` / `med_average()` / `approxim_line()` / `ToOutLGP()`。いずれも MakKi 氏の delogo 由来でライセンス不明（配布物・GitHub のいずれにも LICENSE 表記が無い）。**これらのコードは移植せず、issue #95 本文に書かれた数式（最小二乗の回帰直線、外周値の中央半分の平均）から自分で書き下ろした**。判断の詳細は `src/logo/scan.rs` のモジュール doc comment「重要: MakKi 氏 delogo 由来のコードは訳さない」節。
