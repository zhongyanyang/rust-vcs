mod cli;
mod model;
mod repo;
mod util;

use anyhow::{Result, bail};
use clap::Parser;

use crate::cli::{Cli, Commands};
use crate::repo::Repo;

// 供调试日志使用的人类可读命令标签。
fn command_label(cmd: &Commands) -> &'static str {
    match cmd {
        Commands::Init => "init",
        Commands::Add { .. } => "add",
        Commands::Rm { .. } => "rm",
        Commands::Status => "status",
        Commands::Diff { .. } => "diff",
        Commands::Commit { .. } => "commit",
        Commands::Log => "log",
        Commands::Reset { .. } => "reset",
        Commands::Branch { .. } => "branch",
        Commands::Checkout { .. } => "checkout",
        Commands::Merge { .. } => "merge",
        Commands::Gc => "gc",
        Commands::Verify => "verify",
    }
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    crate::debug_log!("command={} args={:?}", command_label(&cli.cmd), &cli.cmd);

    match cli.cmd {
        Commands::Init => Repo::init_here(),
        Commands::Add { paths } => {
            let repo = Repo::discover()?;
            // 修改类操作在仓库锁保护下执行。
            repo.with_repo_lock(|r| r.add_paths(&paths))
        }
        Commands::Rm { paths } => {
            let repo = Repo::discover()?;
            repo.with_repo_lock(|r| r.rm_paths(&paths))
        }
        Commands::Status => {
            let repo = Repo::discover()?;
            repo.with_repo_lock(|r| r.status())
        }
        Commands::Diff { path } => Repo::discover()?.diff(path.as_deref()),
        Commands::Commit { message } => {
            let repo = Repo::discover()?;
            repo.with_repo_lock(|r| r.commit(&message))
        }
        Commands::Log => Repo::discover()?.log(),
        Commands::Reset { commit, hard } => {
            if !hard {
                // 当前仅实现 --hard 语义，避免行为歧义。
                bail!("only --hard reset is currently supported")
            }
            let repo = Repo::discover()?;
            let target = repo.resolve_commit(&commit)?;
            repo.with_repo_lock(|r| r.reset_hard(&target))
        }
        Commands::Branch { name, delete } => {
            let repo = Repo::discover()?;
            if name.is_some() || delete.is_some() {//创建或删除分支
                repo.with_repo_lock(|r| r.branch(name.as_deref(), delete.as_deref()))
            } else {//只是查看分支
                repo.branch(name.as_deref(), delete.as_deref())
            }
        }
        Commands::Checkout { target } => {
            let repo = Repo::discover()?;
            repo.with_repo_lock(|r| r.checkout(&target))
        }
        Commands::Merge { branch } => {
            let repo = Repo::discover()?;
            repo.with_repo_lock(|r| r.merge_fast_forward_only(&branch))
        }
        Commands::Gc => {
            let repo = Repo::discover()?;
            repo.with_repo_lock(|r| r.gc())
        }
        Commands::Verify => Repo::discover()?.verify(),
    }
}
