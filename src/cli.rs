use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Parser)]
#[command(name = "tachikaze")]
pub struct Cli {
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
