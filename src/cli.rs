use std::path::PathBuf;

use clap::{Args, Parser, Subcommand, ValueEnum};

use crate::logo::frames::LogoRect;
use crate::logo::scan;

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
    /// `trim.avs` のパスと「直して `cut` する」コマンド例を表示する（`--ignore-gate` で
    /// 無視できるが、無視できるのは gate の判定だけで、自己検証や `.dtvi` 必須は
    /// 変わらない）。exit code は 0=完了 / 1=エラー / 2=引数の誤り（clap 既定） /
    /// 3=判定で停止 の4種類のみ。1プロセスにつき入力は1本
    /// （複数ファイルはシェルのループに任せる）。
    #[command(
        after_help = "複数ファイルを処理するときはシェルでループする（出力を\n\
        _CMcut.mp4 サフィックス付きに固定し、case で前回の出力を glob に\n\
        再度取り込まないようにする）:\n\
        \n    for f in *.mp4; do case \"$f\" in *_CMcut.mp4) continue;; esac; tachikaze auto \"$f\" -o \"${f%.mp4}_CMcut.mp4\"; done\n"
    )]
    Auto(AutoArgs),
    /// mp4 とロゴ矩形から `.lgd`（Amatsukaze 形式ロゴデータ）を作る最小実装。
    ///
    /// ロゴ検出には対象の mp4 と同じ解像度で作ったロゴデータが要るが、作る手段が
    /// 従来は Windows の AviUtl / Amatsukaze GUI しか無かった（E14-6、#95）。入力
    /// 全体（既定で全フレーム）を走らせて学習する。CM 区間だけを指定すると
    /// 「ロゴが無い」ロゴデータができてしまうため、区間を絞る引数は用意していない。
    MakeLogo(MakeLogoArgs),
}

#[derive(Debug, Args)]
pub struct AnalyzeArgs {
    pub input: PathBuf,

    /// `trim.avs` の出力先。`-` で標準出力に書く。省略するとキャッシュにだけ書き、
    /// その場所を stderr へ出す（`cut --trim` から辿れる。`cut --trim -` で
    /// 標準入力から読ませることもできる）。
    #[arg(short, long)]
    pub output: Option<PathBuf>,

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

    /// Trim リスト。`-` で標準入力から読む（`analyze -o -` の出力をそのまま渡せる）。
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
    /// を書く（`cut`/`auto` の `-o` は必須で既定名を持たないが、多くの場合
    /// `*_CMcut.mp4` という stem を使う運用と揃えることで、プレイヤーが
    /// 同名の字幕を自動で読み込める）。
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

/// `auto` サブコマンドの引数。
#[derive(Debug, Args)]
pub struct AutoArgs {
    /// 処理する入力 mp4。1プロセスにつき1本
    /// （複数ファイルを回すときはシェルのループに任せる、`--help` の使用例参照）。
    pub input: PathBuf,

    /// 本編の出力先。字幕サイドカーの stem もここから導出する。
    #[arg(short, long)]
    pub output: PathBuf,

    /// CM 側（保持区間の補集合）の出力先。指定したときだけ CM 側ファイルを出す
    /// （既定では出さない）。
    #[arg(long = "cm-output")]
    pub cm_output: Option<PathBuf>,

    /// gate が疑わしいと判定しても cut まで進む。gate の判定だけを無視する
    /// （自己検証1〜8や `.dtvi` 必須は緩めない）。
    #[arg(long = "ignore-gate")]
    pub ignore_gate: bool,

    /// 既存の出力（本編・CM側・字幕サイドカーのいずれか）があっても上書きする
    /// （`cp -f` / `rm -f` と同じ意味）。未指定では、既存の出力があるファイルは
    /// その実行をスキップする（再実行で成果物を黙って潰さないため）。
    #[arg(short = 'f', long)]
    pub force: bool,

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

/// `make-logo` サブコマンドの引数。
#[derive(Debug, Args)]
pub struct MakeLogoArgs {
    pub input: PathBuf,

    /// ロゴ矩形。`x,y,w,h`（カンマ区切り、映像の左上を原点とする表示ピクセル座標。
    /// `--rect` そのままを ffmpeg の `crop` フィルタに渡す）。クロマの間引きに
    /// 合わせて2の倍数に丸める（丸めた場合は stderr へ通知する。
    /// `src/logo/scan.rs::round_rect_to_even`）。矩形の外周1ピクセルが単色になる
    /// フレームだけを学習に使うため、ロゴそのものより少し広めに取ると学習しやすい。
    #[arg(long, value_parser = parse_logo_rect)]
    pub rect: LogoRect,

    /// 出力先の `.lgd`。
    #[arg(short, long)]
    pub output: PathBuf,

    /// 矩形の外周1ピクセルの最小値・最大値の差がこの値を超えるフレームは、
    /// 単色背景ではない（ロゴの外側に映像が写っている）として学習から除外する。
    #[arg(long, default_value_t = scan::DEFAULT_THRESHOLD)]
    pub threshold: u8,
}

/// `--rect x,y,w,h` を [`LogoRect`] にパースする（clap の `value_parser`）。
fn parse_logo_rect(s: &str) -> Result<LogoRect, String> {
    let parts: Vec<&str> = s.split(',').collect();
    let [x, y, w, h]: [&str; 4] = parts.as_slice().try_into().map_err(|_| {
        format!("--rect は x,y,w,h の4値をカンマ区切りで指定してください（実際: \"{s}\"）")
    })?;

    let parse_one = |label: &str, v: &str| -> Result<u32, String> {
        v.trim()
            .parse::<u32>()
            .map_err(|_| format!("--rect の{label}を非負整数として解釈できません: \"{v}\""))
    };

    Ok(LogoRect {
        x: parse_one("x", x)?,
        y: parse_one("y", y)?,
        w: parse_one("w", w)?,
        h: parse_one("h", h)?,
    })
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

    #[test]
    fn make_logo_parses_rect_and_defaults_threshold() {
        let cli = Cli::parse_from([
            "tachikaze",
            "make-logo",
            "in.mp4",
            "--rect",
            "10,20,100,40",
            "-o",
            "out.lgd",
        ]);

        match cli.command {
            Commands::MakeLogo(args) => {
                assert_eq!(
                    args.rect,
                    LogoRect {
                        x: 10,
                        y: 20,
                        w: 100,
                        h: 40
                    }
                );
                assert_eq!(args.threshold, scan::DEFAULT_THRESHOLD);
                assert_eq!(args.output, PathBuf::from("out.lgd"));
            }
            _ => panic!("expected MakeLogo command"),
        }
    }

    #[test]
    fn make_logo_rejects_rect_with_wrong_number_of_fields() {
        let err = Cli::try_parse_from([
            "tachikaze",
            "make-logo",
            "in.mp4",
            "--rect",
            "10,20,100",
            "-o",
            "out.lgd",
        ])
        .expect_err("3値しかないので失敗するはず");
        assert!(err.to_string().contains("--rect"));
    }

    #[test]
    fn make_logo_rejects_non_numeric_rect_field() {
        let err = Cli::try_parse_from([
            "tachikaze",
            "make-logo",
            "in.mp4",
            "--rect",
            "10,20,abc,40",
            "-o",
            "out.lgd",
        ])
        .expect_err("数値でないので失敗するはず");
        assert!(err.to_string().contains("abc"));
    }
}
