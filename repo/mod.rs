use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{Context, Result, anyhow, bail};
use flate2::Compression;
use flate2::read::ZlibDecoder;
use flate2::write::ZlibEncoder;
use fs2::FileExt;
use serde::Serialize;
use similar::TextDiff;
use walkdir::WalkDir;

use crate::model::{Commit, DeltaBlob, Index, StatusCache, StatusCacheEntry};
use crate::util::{
    apply_delta, build_delta, compress_bytes,
    decode_delta_blob, encode_delta_blob, hex_sha256, hex_sha256_file, now_unix,
    read_file_for_blob, short_id,
};

use self::commit_graph::CommitGraph;

mod branch_merge;
mod commit_graph;
mod core;
mod gc_verify;
mod history;
mod workspace;

const META_DIR: &str = ".myvcs";

// 仓库句柄（工作区根目录 + 元数据目录）。
#[derive(Clone)]
pub struct Repo {
    root: PathBuf,
    meta: PathBuf,
    /// ```text
    /// Arc：大家共同持有的钥匙
    /// └─ Mutex：带锁的柜子
    ///     └─ Option：柜子可能是空的
    ///         └─ CommitGraph：柜子里实际存放的东西
    /// ```
    commit_graph: Arc<Mutex<Option<CommitGraph>>>,
}

// 若内容为 UTF-8 文本则输出 unified diff，否则提示为二进制差异。
fn print_text_diff_or_binary(path: &str, old: Option<&[u8]>, new: Option<&[u8]>) {
    let old_label = if old.is_some() {
        format!("a/{}", path)
    } else {
        "/dev/null".to_string()
    };
    let new_label = if new.is_some() {
        format!("b/{}", path)
    } else {
        "/dev/null".to_string()
    };

    let old_text = old.and_then(|b| std::str::from_utf8(b).ok());
    let new_text = new.and_then(|b| std::str::from_utf8(b).ok());
    let old_is_binary = old.is_some() && old_text.is_none();
    let new_is_binary = new.is_some() && new_text.is_none();
    if old_is_binary || new_is_binary {
        println!("  binary files differ");
        return;
    }
    match (old_text, new_text) {
        (Some(a), Some(b)) => {
            let diff = TextDiff::from_lines(a, b);
            let mut udiff = diff.unified_diff();
            udiff.header(&old_label, &new_label);
            print!("{}", udiff);
        }
        (Some(a), None) => {
            let diff = TextDiff::from_lines(a, "");
            let mut udiff = diff.unified_diff();
            udiff.header(&old_label, &new_label);
            print!("{}", udiff);
        }
        (None, Some(b)) => {
            let diff = TextDiff::from_lines("", b);
            let mut udiff = diff.unified_diff();
            udiff.header(&old_label, &new_label);
            print!("{}", udiff);
        }
        _ => {
            println!("  binary files differ");
        }
    }
}

// 由 HEAD 与索引共同确定的“已跟踪路径”并集。
fn tracked_working_set(head_tree: &BTreeMap<String, String>, index: &Index) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    for k in head_tree.keys() {
        set.insert(k.clone());
    }
    for k in index.entries.keys() {
        set.insert(k.clone());
    }
    set
}
