use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "tachikaze", version)]
pub struct Cli {
    /// キャッシュ（`.dtvi` / `trim.avs` / `detail.jls` / 前処理済み入力 / 字幕
    /// サイドカーなど、再生成できる中間物）の置き場所の根。未指定なら
    /// `~/.cache/tachikaze`（ホームディレクトリが特定できない場合は
    /// このオプションを促すエラーで停止する）。入力ごとのサブディレクトリ規則
    /// （`<入力絶対パスのハッシュ>-<stem>/`）自体は変わらない。使い捨てにしたい
    /// 場合は `--cache-dir "$(mktemp -d)"` を使う。
    #[arg(long, global = true)]
    pub cache_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Analyze(AnalyzeArgs),
    Cut(CutArgs),
    /// `cut` に渡す前に、elst(edit list) 除去と字幕抽出を1か所にまとめて行う。
    ///
    /// `cut` は elst 付き入力と字幕トラック付き入力を明示エラーで拒否する。
    /// このコマンドは両方の回避策(ffmpeg での elst 除去、字幕トラックの
    /// サイドカーへの抽出)を1回の ffmpeg 呼び出しにまとめて行う。出力は入力の
    /// 隣ではなく、入力ごとの XDG キャッシュディレクトリに書く
    /// (`docs/architecture.md`「パス解決」節と同じ規則)。
    Prepare(PrepareArgs),
    /// 字幕サイドカー（ASS/SRT）のタイムスタンプを、`cut` が書いた区間マップ
    /// （`--segment-map`）で cut 後のタイムラインへ張り替える。
    ///
    /// 区間マップ・字幕はどちらもキャッシュから自動解決できる（`cut --dtvi` と
    /// 同じ規則）。区間マップは `workdir::cached_segment_map_path`、字幕は
    /// `workdir::subs_path(cache_dir, input, "ass")` / `subs_path(cache_dir, input, "srt")`
    /// の順に探す。`--segment-map` / `--subs` を指定すればそちらを最優先で使う。
    RemapSubs(RemapSubsArgs),
    /// `prepare` → `analyze` → gate 判定 → `cut`(区間マップ込み) → `remap-subs`
    /// を対話なしで合成する。
    ///
    /// アルゴリズムは持たない（`analyze` / `cut` を複製せず呼ぶだけ）。対話プロンプトは
    /// 出さない: gate が疑わしいと判定したら cut を実行せず exit code 3 で停止し、
    /// `trim.avs` のパスと「直して `cut` する」コマンド例を表示する（`--force` で
    /// 無視できるが、無視できるのは gate の判定だけで、自己検証や `.dtvi` 必須は
    /// 変わらない）。exit code は 0=完了 / 1=エラー / 2=引数の誤り（clap 既定） /
    /// 3=判定で停止 の4種類のみ。1プロセスにつき入力は1本
    /// （複数ファイルはシェルのループに任せる）。
    #[command(after_help = "複数ファイルを処理するときはシェルでループする:\n\
        \n    for f in *.mp4; do tachikaze auto \"$f\"; done\n")]
    Auto(AutoArgs),
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    pub input: PathBuf,

    #[arg(short, long)]
    pub output: PathBuf,

    #[arg(long)]
    pub report: bool,

    /// join_logo_scp の `-set KEY VALUE` を上書き・追加する（`KEY=VALUE` 形式、繰り返し可）。
    /// 同じキーを指定すると既定値を置き換える。
    #[arg(long = "jls-set")]
    pub jls_set: Vec<String>,

    /// JL コマンドファイル（既定は `JL_標準.txt`）を差し替える。
    #[arg(long = "jl-file")]
    pub jl_file: Option<PathBuf>,
}

#[derive(Debug, Args)]
pub struct CutArgs {
    pub input: PathBuf,

    #[arg(long)]
    pub trim: PathBuf,

    #[arg(short, long)]
    pub output: PathBuf,

    #[arg(long, value_enum, default_value_t = Snap::Outward)]
    pub snap: Snap,

    #[arg(long)]
    pub video_only: bool,

    #[arg(long)]
    pub verify: bool,

    /// `dtvindex build` が生成した `.dtvi`（オープン GOP 判定と自己検証に必須）。
    /// 未指定なら、直前に同じ入力へ `analyze` を実行していればキャッシュ
    /// （既定 `~/.cache/tachikaze/`、`--cache-dir` で根を変えられる）から
    /// 自動的に見つかる。見つからない場合は `analyze` を実行するコマンド例を
    /// 添えて停止する。
    #[arg(long)]
    pub dtvi: Option<PathBuf>,

    /// 保持区間の補集合（CM として除去した区間）を、指定したパスへ別ファイルとして
    /// 出力する。検出が当たっているか（CM 側に本編が映り込んでいないか）を目視で
    /// 確認するための機能（docs/lossless-cut.md「CM 側（除去した区間）を別ファイルに
    /// 出す」節）。`--snap inward` とは併用できない（保持区間が退化しうるため）。
    #[arg(long = "cm-output")]
    pub cm_output: Option<PathBuf>,

    /// 区間マップ（snap 後の境界と出力タイムライン上の開始時刻。字幕やチャプターを
    /// cut 後のタイムラインに合わせるための中間データ）を、指定したパスにも書き出す。
    /// 既定でも入力ごとのキャッシュ（`work.mp4.segmap.json`）に書かれるため、この
    /// オプションは「任意の場所にも欲しい」場合に使う。`--cm-output` 指定時は保持側
    /// のマップだけを対象にする（CM 側は検出確認用で、字幕を付ける対象ではない）。
    #[arg(long = "segment-map")]
    pub segment_map: Option<PathBuf>,
}

/// `prepare` サブコマンドの引数。
#[derive(Debug, Args)]
pub struct PrepareArgs {
    pub input: PathBuf,

    /// mp4 内蔵の字幕トラックから抽出する代わりに、指定したファイルを
    /// そのまま字幕サイドカーとして使う。元が ARIB 字幕由来の外部 `.ass`
    /// はスタイル情報が mp4 内 `mov_text` より豊富な場合があるための
    /// 差し替え口。指定時も、mp4 内蔵の字幕トラック自体の除去(elst 除去と
    /// 同じ ffmpeg 呼び出し)は引き続き行う。
    #[arg(long)]
    pub subs: Option<PathBuf>,
}

/// `remap-subs` サブコマンドの引数。
#[derive(Debug, Args)]
pub struct RemapSubsArgs {
    pub input: PathBuf,

    /// `cut --segment-map` が書き出した JSON。未指定ならキャッシュ
    /// （`work.mp4.segmap.json`）から自動的に探す。見つからない場合は
    /// `cut` を実行するコマンド例を添えて停止する。
    #[arg(long = "segment-map")]
    pub segment_map: Option<PathBuf>,

    /// 張り替える字幕ファイル（`.ass`/`.ssa`/`.srt`）。未指定なら
    /// キャッシュ（`prepare` が書いた `subs.ass` / `subs.srt`）から順に探す。
    #[arg(long)]
    pub subs: Option<PathBuf>,

    /// 出力先。未指定なら入力の隣に `<入力のstem>_CMcut.<字幕の拡張子>`
    /// を書く（`cut` の既定の出力名 `*_CMcut.mp4` と同じ stem にすることで、
    /// プレイヤーが同名の字幕を自動で読み込める）。
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

/// `auto` サブコマンドの引数。
#[derive(Debug, Args)]
pub struct AutoArgs {
    /// 処理する入力 mp4。1プロセスにつき1本
    /// （複数ファイルを回すときはシェルのループに任せる、`--help` の使用例参照）。
    pub input: PathBuf,

    /// 本編の出力先。既定は入力の隣の `<stem>_CMcut.mp4`。
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// CM 側（保持区間の補集合）の出力先。既定は入力の隣の `<stem>_CM.mp4`。
    /// `--no-cm` とは併用できない。
    #[arg(long = "cm-output")]
    pub cm_output: Option<PathBuf>,

    /// CM 側ファイルを出さない（既定では `<stem>_CM.mp4` を出す）。
    #[arg(long)]
    pub no_cm: bool,

    /// gate が疑わしいと判定しても cut まで進む。gate の判定だけを無視する
    /// （自己検証1〜8や `.dtvi` 必須は緩めない）。
    #[arg(long)]
    pub force: bool,

    /// 既存の出力（本編・CM側・字幕サイドカーのいずれか）があっても上書きする。
    /// 未指定では、既存の出力があるファイルはその実行をスキップする
    /// （再実行で成果物を黙って潰さないため）。
    #[arg(long)]
    pub overwrite: bool,

    /// `prepare` + `analyze` + gate 判定までで止め、`cut` / `remap-subs` を
    /// 実行しない。止めたあとに `trim.avs` を人手で直して `cut` を直接実行できる
    /// （`--dtvi` も明示のパスを表示するので `cut` 単体で完結する）。
    #[arg(long = "analyze-only")]
    pub analyze_only: bool,

    /// 字幕の抽出・張り替えを行わない。未指定の場合、字幕の張り替えに失敗すると
    /// 既定でハードエラーになる（本編だけ出して字幕を黙って落とさないため）。
    #[arg(long = "no-subtitles")]
    pub no_subtitles: bool,

    #[arg(long, value_enum, default_value_t = Snap::Outward)]
    pub snap: Snap,

    /// `cut` に `--verify` を付ける（ffprobe のパケット単位 CRC32 比較）。
    #[arg(long)]
    pub verify: bool,

    /// join_logo_scp の JL コマンドファイルを差し替える（`analyze --jl-file` と同じ）。
    #[arg(long = "jl-file")]
    pub jl_file: Option<PathBuf>,

    /// join_logo_scp の `-set KEY VALUE` を上書き・追加する
    /// （`analyze --jls-set` と同じ、`KEY=VALUE` 形式、繰り返し可）。
    #[arg(long = "jls-set")]
    pub jls_set: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Snap {
    Outward,
    Inward,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snap_defaults_to_outward() {
        let cli = Cli::parse_from([
            "tachikaze",
            "cut",
            "in.mp4",
            "--trim",
            "trim.avs",
            "-o",
            "out.mp4",
        ]);

        match cli.command {
            Commands::Cut(args) => assert_eq!(args.snap, Snap::Outward),
            _ => panic!("expected Cut command"),
        }
    }
}
