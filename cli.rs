use clap::{Parser, Subcommand};

// 顶层命令行解析器。
#[derive(Parser)]
#[command(name = "myvcs")]
#[command(about = "A lightweight local VCS implemented in Rust")]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Commands,
}

// 面向用户的子命令集合。
#[derive(Subcommand, Debug)]
pub enum Commands {
    Init,
    Add {
        paths: Vec<String>,
    },
    Rm {
        paths: Vec<String>,
    },
    Status,
    Diff {
        path: Option<String>,
    },
    Commit {
        #[arg(short, long)]
        message: String,
    },
    Log,
    Reset {
        commit: String,
        #[arg(long)]
        hard: bool,
    },
    Branch {
        name: Option<String>,
        #[arg(short = 'd', long)]
        delete: Option<String>,
    },
    Checkout {
        target: String,
    },
    Merge {
        branch: String,
    },
    Gc,
    Verify,
}
