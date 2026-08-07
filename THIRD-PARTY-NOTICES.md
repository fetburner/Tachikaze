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

これらは `src/tools.rs::resolve_tool` が `PATH` から実体（バイナリのパス）を見つけ、`src/external.rs::run` が子プロセスとして起動する。受け渡しはファイルとコマンドライン引数のみで、リンクも同一アドレス空間の共有もない。JL コマンドファイル（`join_logo_scp` の判定ルール）も `src/tools.rs::default_jl_command_file` が `join_logo_scp` のインストール先から実行時に解決するだけで、リポジトリには同梱していない。したがって GPL の派生物条件は Tachikaze 本体には及ばない。

Cargo 依存（`cargo metadata` で確認した tachikaze 自身を除く全 51 パッケージ）はすべて permissive で、GPL のクレートは無い（`MIT OR Apache-2.0` 39 / `MIT` 7 / `Apache-2.0 OR MIT` 3 / `Unlicense OR MIT` 1 / `(MIT OR Apache-2.0) AND Unicode-3.0` 1）。

**Docker イメージを配布する場合は条件が付く。** 詳細と対応（commit SHA 固定の必要性）は [docs/docker.md](docs/docker.md)「既知の制約」。

## 移植したコード

現時点では無い。

将来 Amatsukaze（[nekopanda/Amatsukaze](https://github.com/nekopanda/Amatsukaze)、`LogoScan.hpp`、MIT、Copyright (c) 2017-2019 Nekopanda）からロゴ検出等のアルゴリズムを移植した場合、ここに著作権表示を追記する。MIT ライセンスの義務は著作権表示とライセンス文の保持だけなので、追記はここで足りる。
