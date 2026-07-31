use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "tachikaze")]
pub struct Cli {
    /// 外部ツール（chapter_exe / join_logo_scp / dtvindex / ffprobe）を探す
    /// ディレクトリ。指定すると他の探索方法より優先される。
    #[arg(long, global = true)]
    pub tool_dir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    Analyze {
        input: PathBuf,

        #[arg(short, long)]
        output: PathBuf,

        #[arg(long)]
        report: bool,

        /// 中間ファイル（`.dtvi` / `trim.avs` / `detail.jls`）の置き場所。未指定
        /// なら入力ごとに決まる `${XDG_CACHE_HOME:-~/.cache}/tachikaze/` 配下の
        /// ディレクトリを使い、削除しない（`cut --dtvi` は同じ規則で自動的に
        /// `work.mp4.dtvi` を見つけられる）。
        #[arg(long)]
        work_dir: Option<PathBuf>,

        /// 既定のキャッシュディレクトリを使わず、従来どおり一時ディレクトリに
        /// 中間ファイルを作り、成功時に削除する（`--work-dir` とは併用しない）。
        #[arg(long)]
        no_keep_work: bool,

        /// join_logo_scp の `-set KEY VALUE` を上書き・追加する（`KEY=VALUE` 形式、繰り返し可）。
        /// 同じキーを指定すると既定値を置き換える。
        #[arg(long = "jls-set")]
        jls_set: Vec<String>,

        /// JL コマンドファイル（既定は `JL_標準.txt`）を差し替える。
        #[arg(long = "jl-file")]
        jl_file: Option<PathBuf>,
    },
    Cut {
        input: PathBuf,

        #[arg(long)]
        trim: PathBuf,

        #[arg(short, long)]
        output: PathBuf,

        #[arg(long, value_enum, default_value_t = Snap::Outward)]
        snap: Snap,

        #[arg(long)]
        video_only: bool,

        #[arg(long)]
        verify: bool,

        /// `dtvindex build` が生成した `.dtvi`（オープン GOP 判定と自己検証に必須）。
        /// 未指定なら、直前に同じ入力へ `analyze` を実行していればキャッシュ
        /// （`${XDG_CACHE_HOME:-~/.cache}/tachikaze/`）から自動的に見つかる。
        /// 見つからない場合は `analyze` を実行するコマンド例を添えて停止する。
        #[arg(long)]
        dtvi: Option<PathBuf>,

        /// 保持区間の補集合（CM として除去した区間）を、指定したパスへ別ファイルとして
        /// 出力する。検出が当たっているか（CM 側に本編が映り込んでいないか）を目視で
        /// 確認するための機能（docs/lossless-cut.md「CM 側（除去した区間）を別ファイルに
        /// 出す」節）。`--snap inward` とは併用できない（保持区間が退化しうるため）。
        #[arg(long = "cm-output")]
        cm_output: Option<PathBuf>,

        /// 区間マップ（snap 後の境界と出力タイムライン上の開始時刻。字幕やチャプターを
        /// cut 後のタイムラインに合わせるための中間データ）を、指定したパスにも書き出す。
        /// 既定でも入力ごとのキャッシュ（`work.mp4.segmap.json`）に書かれるため、この
        /// オプションは「任意の場所にも欲しい」場合に使う。`--cm-output` 指定時は保持側
        /// のマップだけを対象にする（CM 側は検出確認用で、字幕を付ける対象ではない）。
        #[arg(long = "segment-map")]
        segment_map: Option<PathBuf>,
    },
    /// `cut` に渡す前に、elst(edit list) 除去と字幕抽出を1か所にまとめて行う。
    ///
    /// `cut` は elst 付き入力と字幕トラック付き入力を明示エラーで拒否する(#41)。
    /// このコマンドは両方の回避策(ffmpeg での elst 除去、字幕トラックの
    /// サイドカーへの抽出)を1回の ffmpeg 呼び出しにまとめて行う。出力は入力の
    /// 隣ではなく、入力ごとの XDG キャッシュディレクトリに書く
    /// (`docs/architecture.md`「パス解決」節と同じ規則)。
    Prepare {
        input: PathBuf,

        /// mp4 内蔵の字幕トラックから抽出する代わりに、指定したファイルを
        /// そのまま字幕サイドカーとして使う。元が ARIB 字幕由来の外部 `.ass`
        /// はスタイル情報が mp4 内 `mov_text` より豊富な場合があるための
        /// 差し替え口。指定時も、mp4 内蔵の字幕トラック自体の除去(elst 除去と
        /// 同じ ffmpeg 呼び出し)は引き続き行う。
        #[arg(long)]
        subs: Option<PathBuf>,
    },
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
            Commands::Cut { snap, .. } => assert_eq!(snap, Snap::Outward),
            _ => panic!("expected Cut command"),
        }
    }
}
