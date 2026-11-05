use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

// 暂存区：记录待提交的新增/修改，以及待删除路径。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
//这个 Index 表示暂存区
pub struct Index {
    //第一个 String：文件在仓库里的路径
    //第二个 String：文件内容保存后得到的对象 ID，也就是 oid
    pub entries: BTreeMap<String, String>,  //BTreeMap 是一张“键 → 值”的表
    #[serde(default)]
    pub removed: BTreeSet<String>,   //BTreeSet 是一组不允许重复的值,保存准备删除的文件路径
}

// `status` 使用的缓存，避免对未变化文件重复哈希。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
///文件路径 → 这个文件的缓存信息
//它的意义是节省时间。计算内容哈希需要读取整个文件；文件很大、文件很多时
//每次执行 status 都重新读取会很慢
pub struct StatusCache {
    pub entries: BTreeMap<String, StatusCacheEntry>,
}

// 工作区文件指纹缓存项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StatusCacheEntry {
    pub mtime_secs: u64,  //文件最后修改时间，以秒为单位
    pub size: u64,  //文件大小，以字节为单位
    pub hash: String,  //上次计算出的文件内容哈希
}

// 持久化在对象库中的提交对象。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commit {  //理解为一张包含多个字段的提交记录
    pub tree: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub parents: Vec<String>, //多个父提交的 ID
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>, //单个父提交的 ID
    pub message: String, //提交说明
    pub author: String,
    pub timestamp: i64,
}

pub const DELTA_MAGIC: &[u8] = b"MYVCS_DELTA_V1\\n";

// 用于紧凑存储 blob 的增量指令序列。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op")]
pub enum DeltaOp {
    #[serde(rename = "copy")]
    Copy { offset: usize, len: usize },
    #[serde(rename = "insert")]
    Insert { data: Vec<u8> },
}

// 引用基础对象的序列化增量 blob。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeltaBlob {
    pub base: String,
    pub ops: Vec<DeltaOp>,
    pub size: usize,
}
