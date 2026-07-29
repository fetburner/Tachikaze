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

        #[arg(long)]
        work_dir: Option<PathBuf>,

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
        /// `analyze --work-dir <DIR>` を使うと `<DIR>/work.mp4.dtvi` に残る。
        #[arg(long)]
        dtvi: Option<PathBuf>,
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
