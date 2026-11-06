use std::collections::HashMap;
use std::env;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::Path;
use std::sync::OnceLock;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use flate2::Compression;
use flate2::write::ZlibEncoder;
#[cfg(target_os = "linux")]
use memmap2::Advice;
use memmap2::MmapOptions;
use sha2::{Digest, Sha256};

use crate::model::{DELTA_MAGIC, DeltaBlob, DeltaOp};

pub const LARGE_FILE_THRESHOLD: u64 = 10 * 1024 * 1024;
static OPENEULER_IO_HINTS: OnceLock<bool> = OnceLock::new();
static ETMEM_TIERING: OnceLock<bool> = OnceLock::new();
static IO_CHUNK_SIZE_BYTES: OnceLock<usize> = OnceLock::new();

// 当 MYVCS_DEBUG 为真值时启用详细调试日志。
pub fn is_debug_enabled() -> bool {
    match env::var("MYVCS_DEBUG") {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"),
        Err(_) => false,
    }
}

#[macro_export]
macro_rules! debug_log {
    ($($arg:tt)*) => {{
        if $crate::util::is_debug_enabled() {
            eprintln!("[myvcs:debug] {}", format_args!($($arg)*));
        }
    }};
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

// 用于命令行输出的短提交 ID。
pub fn short_id(id: &str) -> String {
    id.chars().take(8).collect()
}

// 计算对象 ID 使用的标准 SHA-256 十六进制编码。
pub fn hex_sha256(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    digest_to_hex(&hasher.finalize())
}

// 计算文件内容的 SHA-256；大文件优先 mmap，失败时回退流式读取。
pub fn hex_sha256_file(path: &Path) -> Result<String> {
    let meta = fs::metadata(path)?;
    if meta.len() < LARGE_FILE_THRESHOLD {
        return Ok(hex_sha256(&fs::read(path)?));
    }

    let mut f = File::open(path)?;
    advise_file_sequential(&f);
    if should_use_mmap_for_large_files() {
        let mapped = unsafe { MmapOptions::new().map(&f) };
        if let Ok(m) = mapped {
            advise_mmap_sequential(&m);
            let out = hex_sha256(m.as_ref());
            advise_file_dontneed(&f);
            return Ok(out);
        }
    }

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; io_chunk_size_bytes()];
    loop {
        let n = f.read(&mut buf[..])?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }

    let out = digest_to_hex(&hasher.finalize());
    advise_file_dontneed(&f);
    Ok(out)
}

/// 使用默认压缩级别，将输入字节压缩为 zlib 格式的数据。
///
/// 输入 `bytes` 是待压缩的原始字节数据。
///
/// 返回值 `Result<Vec<u8>>`：
///
/// - `Ok(data)`：压缩成功，得到压缩后的完整字节数据。
/// - `Err(...)`：写入压缩器或完成压缩时失败，携带错误信息。
///
/// 本函数只在内存中压缩，不读写文件；压缩后的数据不保证比原始数据更小。
pub fn compress_bytes(bytes: &[u8]) -> Result<Vec<u8>> {
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?)
}

/// 将差量结构编码为可供后续保存或传输的字节数据。
///
/// 编码格式为差量格式标记（魔数头）加 JSON 数据，包含基准对象 ID、
/// 重建指令和目标内容大小。
///
/// 返回值 `Result<Vec<u8>>`：
///
/// - `Ok(bytes)`：编码成功，得到编码后的完整字节数据。
/// - `Err(...)`：JSON 序列化失败，携带错误信息。
///
/// 本函数只负责编码，不压缩、不写入文件，也不还原目标内容。
pub fn encode_delta_blob(delta: &DeltaBlob) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    out.extend_from_slice(DELTA_MAGIC);
    out.extend_from_slice(&serde_json::to_vec(delta)?);
    Ok(out)
}
///判断传入的字节是不是增量格式；
/// 如果是，就解析成程序能使用的增量结构。
/// 它只解析说明，不负责还原文件
/// 判断输入字节是否为增量格式；如果是，则解析为增量结构，不负责还原文件内容。
///
/// 输入 `raw: &[u8]` 是一段已经读进内存的字节。
///
/// 返回值 `Result<Option<DeltaBlob>>` 有三种情况：
///
/// - `Ok(None)`：不是增量格式，交给调用方按普通内容处理。
/// - `Ok(Some(delta))`：是增量格式，解析成功。
/// - `Err(...)`：有增量标记，但内容无法正确解析。
///
/// 其中 `delta` 包含：基准对象的 ID、重建指令、还原后的文件大小。
pub fn decode_delta_blob(raw: &[u8]) -> Result<Option<DeltaBlob>> {
    if !raw.starts_with(DELTA_MAGIC) {
        return Ok(None);
    }
    let body = &raw[DELTA_MAGIC.len()..];
    let delta: DeltaBlob = serde_json::from_slice(body).context("invalid delta blob")?;
    Ok(Some(delta))
}

/// 从 base 到 target 构建基于固定块的增量指令。
///
/// 比较基准内容 base 和目标内容 target，生成一组重建目标内容的指令
///
/// 返回值 `Vec<DeltaOp>` 是一个按顺序执行的指令列表，包含两种操作：
///
/// - `Copy { offset, len }`：从基准内容的指定位置复制一段数据。
/// - `Insert { data }`：直接追加这段数据。
pub fn build_delta(base: &[u8], target: &[u8]) -> Vec<DeltaOp> {
    const CHUNK: usize = 32;
    const MAX_CANDIDATES: usize = 8;
    if base.is_empty() || target.is_empty() {
        return vec![DeltaOp::Insert {
            data: target.to_vec(),
        }];
    }

    let mut index: HashMap<u64, Vec<usize>> = HashMap::new();
    if base.len() >= CHUNK {
        let mut i = 0usize;
        while i + CHUNK <= base.len() {
            let key = chunk_hash(&base[i..i + CHUNK]);
            let candidates = index.entry(key).or_default();
            if candidates.len() < MAX_CANDIDATES {
                candidates.push(i);
            }
            i += CHUNK;
        }
    }

    let mut ops = Vec::new();
    let mut insert_buf = Vec::new();
    let mut i = 0usize;
    while i < target.len() {
        if i + CHUNK <= target.len()
            && !index.is_empty()
            && let Some(candidates) = index.get(&chunk_hash(&target[i..i + CHUNK]))
        {
            let mut best_off = 0usize;
            let mut best_len = 0usize;
            for &off in candidates {
                let mut l = CHUNK;
                while off + l < base.len() && i + l < target.len() && base[off + l] == target[i + l]
                {
                    l += 1;
                }
                if l > best_len {
                    best_len = l;
                    best_off = off;
                }
            }

            if best_len >= CHUNK {
                if !insert_buf.is_empty() {
                    ops.push(DeltaOp::Insert {
                        data: std::mem::take(&mut insert_buf),
                    });
                }
                ops.push(DeltaOp::Copy {
                    offset: best_off,
                    len: best_len,
                });
                i += best_len;
                continue;
            }
        }

        insert_buf.push(target[i]);
        i += 1;
    }

    if !insert_buf.is_empty() {
        ops.push(DeltaOp::Insert { data: insert_buf });
    }
    ops
}

/// 根据基准内容和增量指令，在内存中重建目标文件的完整字节。
/// 复制范围越界时返回错误；不执行文件读写。
pub fn apply_delta(base: &[u8], ops: &[DeltaOp]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    for op in ops {
        match op {
            DeltaOp::Copy { offset, len } => {
                let end = offset.saturating_add(*len);
                if end > base.len() {
                    bail!("invalid delta copy range: {}..{}", offset, end);
                }
                out.extend_from_slice(&base[*offset..end]);
            }
            DeltaOp::Insert { data } => out.extend_from_slice(data),
        }
    }
    Ok(out)
}

fn chunk_hash(chunk: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for &b in chunk {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

// 读取文件字节；大文件优先内存映射，降低峰值内存波动。
pub fn read_file_for_blob(path: &Path) -> Result<Vec<u8>> {
    let meta = fs::metadata(path)?;
    if meta.len() < LARGE_FILE_THRESHOLD {
        return Ok(fs::read(path)?);
    }

    let mut f = File::open(path)?;
    advise_file_sequential(&f);
    if should_use_mmap_for_large_files() {
        let mapped = unsafe { MmapOptions::new().map(&f) };
        if let Ok(m) = mapped {
            advise_mmap_sequential(&m);
            let out = m.as_ref().to_vec();
            advise_file_dontneed(&f);
            return Ok(out);
        }
    }

    let file_len = capacity_from_u64(meta.len());
    let mut out = Vec::with_capacity(file_len);
    let mut buf = vec![0u8; io_chunk_size_bytes()];
    loop {
        let n = f.read(&mut buf[..])?;
        if n == 0 {
            break;
        }
        out.extend_from_slice(&buf[..n]);
    }

    advise_file_dontneed(&f);
    Ok(out)
}

pub fn digest_to_hex(digest: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for &b in digest {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn advise_mmap_sequential(m: &memmap2::Mmap) {
    #[cfg(target_os = "linux")]
    {
        let _ = m.advise(Advice::Sequential);
        if should_enable_openeuler_io_hints() {
            let _ = m.advise(Advice::WillNeed);
        }
    }
}

pub fn advise_file_sequential(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        if should_enable_openeuler_io_hints() {
            let _ =
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_SEQUENTIAL) };
        }
    }
}

pub fn advise_file_dontneed(file: &File) {
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;

        if should_enable_openeuler_io_hints() {
            let _ =
                unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED) };
        }
    }
}
///尝试降低当前进程的 CPU 调度优先级，让 GC、校验等工作少与其他程序争抢 CPU
pub fn maybe_set_background_nice() {
    #[cfg(target_os = "linux")]
    {
        if !should_enable_openeuler_io_hints() || env_truthy("MYVCS_DISABLE_BG_NICE") {
            return;
        }

        let nice = env::var("MYVCS_BG_NICE")
            .ok()
            .and_then(|v| v.parse::<i32>().ok())
            .unwrap_or(10)
            .clamp(0, 19);

        let _ = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice as libc::c_int) };
    }
}
/// 判断是否应启用深度增量压缩优化
/// 也就是通过保存文件版本之间的差异来减少仓库占用。它本身不执行优化
pub fn should_run_deep_delta_gc() -> bool {
    if env_truthy("MYVCS_FORCE_DEEP_DELTA") {
        return true;
    }
    if env_truthy("MYVCS_DISABLE_DEEP_DELTA") {
        return false;
    }
    if !should_enable_openeuler_io_hints() {
        return true;
    }

    let Some(load1) = current_loadavg_1() else {
        return true;
    };

    let cpus = std::thread::available_parallelism()
        .map(|n| n.get() as f64)
        .unwrap_or(1.0)
        .max(1.0);

    let threshold = env::var("MYVCS_DEEP_DELTA_LOAD_RATIO")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .filter(|v| *v > 0.0)
        .unwrap_or(0.8);

    (load1 / cpus) < threshold
}

/// 检测当前操作系统是否为 openEuler。
///
/// 读取 `/etc/os-release`，其中 `ID` 为 `openeuler` 时返回 `true`；
/// 其他系统、读取失败或无法解析系统标识时返回 `false`。
/// 检测结果在首次调用后缓存，后续调用复用该结果。
/// 本函数仅识别系统，不修改系统配置；调用方根据返回值决定执行哪些操作。
pub(crate) fn should_enable_openeuler_io_hints() -> bool {
    *OPENEULER_IO_HINTS.get_or_init(|| {
        let raw = fs::read_to_string("/etc/os-release").unwrap_or_default();
        parse_os_release_id(&raw)
            .map(|id| id == "openeuler")
            .unwrap_or(false)
    })
}

pub(crate) fn should_enable_etmem_tiering() -> bool {
    *ETMEM_TIERING.get_or_init(|| {
        if env_truthy("MYVCS_DISABLE_ETMEM") {
            return false;
        }
        if env_truthy("MYVCS_ENABLE_ETMEM") {
            return true;
        }
        if !should_enable_openeuler_io_hints() {
            return false;
        }
        detect_etmem_support()
    })
}

fn detect_etmem_support() -> bool {
    Path::new("/sys/kernel/mm/etmem").exists()
        || Path::new("/sys/kernel/mm/memory_tiering").exists()
        || Path::new("/etc/etmemd/etmemd.conf").exists()
}

fn should_use_mmap_for_large_files() -> bool {
    if env_truthy("MYVCS_DISABLE_MMAP") {
        return false;
    }

    // 在 openEuler 低内存边缘环境下默认关闭 mmap，优先稳定性。
    if should_enable_openeuler_io_hints()
        && let Some(total_kb) = detect_total_mem_kb()
    {
        return total_kb > 1024 * 1024;
    }

    true
}

fn detect_total_mem_kb() -> Option<u64> {
    let raw = fs::read_to_string("/proc/meminfo").ok()?;
    parse_memtotal_kb(&raw)
}

fn parse_memtotal_kb(raw: &str) -> Option<u64> {
    for line in raw.lines() {
        let Some(rest) = line.strip_prefix("MemTotal:") else {
            continue;
        };
        let val = rest.split_whitespace().next()?.parse::<u64>().ok()?;
        return Some(val);
    }
    None
}

fn current_loadavg_1() -> Option<f64> {
    let raw = fs::read_to_string("/proc/loadavg").ok()?;
    parse_loadavg_1(&raw)
}

fn parse_loadavg_1(raw: &str) -> Option<f64> {
    raw.split_whitespace().next()?.parse::<f64>().ok()
}

fn env_truthy(key: &str) -> bool {
    match env::var(key) {
        Ok(v) => matches!(v.as_str(), "1" | "true" | "TRUE" | "yes" | "on"),
        Err(_) => false,
    }
}

fn env_kb(key: &str) -> Option<u64> {
    env::var(key).ok()?.parse::<u64>().ok()
}

fn capacity_from_u64(len: u64) -> usize {
    if len > usize::MAX as u64 {
        0
    } else {
        len as usize
    }
}

fn parse_os_release_id(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let Some(v) = line.strip_prefix("ID=") else {
            continue;
        };
        let id = v
            .trim()
            .trim_matches('"')
            .trim_matches('\'')
            .to_ascii_lowercase();
        if !id.is_empty() {
            return Some(id);
        }
    }
    None
}

// ---------------------------------------------------------------------------
// openEuler 深度优化 — 文件系统感知层
// ---------------------------------------------------------------------------

/// 文件系统类型枚举，用于自适应 I/O 策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsType {
    Ext4,
    Xfs,
    Btrfs,
    EulerFs,
    Other,
}

impl FsType {
    /// 返回该文件系统的最优写缓冲区大小（字节）。
    /// 对齐到文件系统 extent/stripe 边界可减少 RMW（Read-Modify-Write）。
    pub fn optimal_write_buffer(&self) -> usize {
        match self {
            // ext4 默认 extent 大小 128KB，4KB 块 × 32 = 128KB 对齐
            FsType::Ext4 => 128 * 1024,
            // XFS 默认 stripe 宽度通常为 64KB-256KB，取 256KB
            FsType::Xfs => 256 * 1024,
            // Btrfs 节点大小 16KB，取 64KB 对齐
            FsType::Btrfs => 64 * 1024,
            // EulerFS 大块分配器优化 64KB-256KB，取 256KB
            FsType::EulerFs => 256 * 1024,
            FsType::Other => 64 * 1024,
        }
    }

    /// 是否属于 openEuler 生态文件系统。
    #[allow(dead_code)]
    pub fn is_openeuler_native(&self) -> bool {
        matches!(self, FsType::EulerFs | FsType::Ext4)
    }
}

/// 检测指定路径所在文件系统类型。
/// 读取 `/proc/mounts` 按最长挂载点前缀匹配。
pub fn detect_fs_type(path: &Path) -> FsType {
    static CACHED_FS_TYPE: OnceLock<FsType> = OnceLock::new();
    *CACHED_FS_TYPE.get_or_init(|| {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
        let target = canonical.to_string_lossy();

        let mounts = fs::read_to_string("/proc/mounts").unwrap_or_default();
        let mut best_match: Option<(String, usize)> = None;

        for line in mounts.lines() {
            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                continue;
            }
            let mount_point = parts[1];
            if target.starts_with(mount_point) && mount_point.len() > best_match.as_ref().map_or(0, |(_, l)| *l)
            {
                best_match = Some((parts[2].to_string(), mount_point.len()));
            }
        }

        match best_match.as_ref().map(|(fs, _)| fs.as_str()) {
            Some("ext4") => FsType::Ext4,
            Some("xfs") => FsType::Xfs,
            Some("btrfs") => FsType::Btrfs,
            Some("eulerfs") => FsType::EulerFs,
            _ => FsType::Other,
        }
    })
}

const EULERFS_PREALLOC_MIN_BYTES: u64 = 256 * 1024;

pub fn eulerfs_prealloc_len(path: &Path, data_len: u64) -> Option<u64> {
    if data_len < EULERFS_PREALLOC_MIN_BYTES {
        return None;
    }
    if env_truthy("MYVCS_DISABLE_EULERFS_PREALLOC") {
        return None;
    }
    if !should_enable_openeuler_io_hints() {
        return None;
    }
    if detect_fs_type(path) != FsType::EulerFs {
        return None;
    }
    let align = FsType::EulerFs.optimal_write_buffer() as u64;
    let aligned = ((data_len + align - 1) / align) * align;
    Some(aligned.max(align))
}

#[cfg(target_os = "linux")]
pub fn preallocate_file(file: &File, len: u64) {
    if len == 0 {
        return;
    }
    use std::os::fd::AsRawFd;
    let rc = unsafe { libc::posix_fallocate(file.as_raw_fd(), 0, len as libc::off_t) };
    if rc != 0 {
        crate::debug_log!("posix_fallocate failed rc={} len={}", rc, len);
    }
}

#[cfg(not(target_os = "linux"))]
pub fn preallocate_file(_file: &File, _len: u64) {}


#[derive(Debug, Clone)]
pub struct NumaTopology {
    pub node_count: usize,
    #[allow(dead_code)]
    pub cpu_to_node: Vec<usize>,
}

pub fn detect_numa_topology() -> NumaTopology {
    static CACHED_NUMA: OnceLock<NumaTopology> = OnceLock::new();
    CACHED_NUMA.get_or_init(|| {
        let node_dir = Path::new("/sys/devices/system/node");
        let mut node_count = 0usize;
        if let Ok(entries) = fs::read_dir(node_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with("node") && name[4..].parse::<u64>().is_ok() {
                    node_count += 1;
                }
            }
        }
        if node_count == 0 {
            node_count = 1;
        }

        let mut cpu_to_node = Vec::new();
        let cpu_dir = Path::new("/sys/devices/system/cpu");
        if let Ok(entries) = fs::read_dir(cpu_dir) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if let Some(suffix) = name.strip_prefix("cpu") {
                    if suffix.parse::<u64>().is_ok() {
                        let node_path = e.path().join("topology").join("physical_package_id");
                        if let Ok(raw) = fs::read_to_string(&node_path) {
                            if let Ok(n) = raw.trim().parse::<usize>() {
                                if cpu_to_node.len() <= n {
                                    cpu_to_node.resize(n + 1, 0);
                                }
                                cpu_to_node[n] = n;
                            }
                        }
                    }
                }
            }
        }
        if cpu_to_node.is_empty() {
            cpu_to_node.push(0);
        }

        NumaTopology {
            node_count,
            cpu_to_node,
        }
    })
    .clone()
}

/// 获取最优 I/O 块大小：综合考虑 FS 类型、内存容量、NUMA 拓扑。
/// openEuler 场景下自动使用 FS 对齐值。
pub fn io_chunk_size_bytes() -> usize {
    *IO_CHUNK_SIZE_BYTES.get_or_init(|| {
        if let Some(kb) = env_kb("MYVCS_IO_CHUNK_KB") {
            return (kb.clamp(64, 4096) * 1024) as usize;
        }

        // openEuler 优化：优先使用 FS 对齐值
        if should_enable_openeuler_io_hints() {
            // 以 objects 目录所在 FS 为准
            let fs_type = detect_fs_type(Path::new("."));
            let fs_opt = fs_type.optimal_write_buffer();

            // 若内存紧张，折半以降低单次 I/O 压力
            if let Some(total_kb) = detect_total_mem_kb() {
                if total_kb <= 1024 * 1024 {
                    return (fs_opt / 2).max(64 * 1024);
                }
                if total_kb <= 2 * 1024 * 1024 {
                    return (fs_opt * 2 / 3).max(128 * 1024);
                }
            }
            return fs_opt;
        }

        1024 * 1024
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlobTemperature {
    Hot,
    Warm,
    Cold,
}

/// 根据提交时间距当前时间的长短，将文件内容对象划分为热、温、冷三类。
///
/// 输入 `commit_timestamp` 是 Unix 时间戳，单位为秒。
/// 调用方传入引用该对象的最新提交时间时，分类反映该对象最近出现于提交中的时间。
///
/// 返回值 `BlobTemperature`：
///
/// - `Hot`：距现在不足 1 小时。
/// - `Warm`：距现在满 1 小时、不足 7 天。
/// - `Cold`：距现在满 7 天及以上。
///
/// 未来时间按距现在 0 秒处理，归为 `Hot`。
/// 此分类仅依据提交时间，不反映实际读取频率，也不修改对象或系统配置。
pub fn categorize_blob_temperature(commit_timestamp: i64) -> BlobTemperature {
    let now = now_unix() as i64;
    let age_secs = (now - commit_timestamp).max(0) as u64;

    if age_secs < 3600 {
        BlobTemperature::Hot
    } else if age_secs < 604800 {
        BlobTemperature::Warm
    } else {
        BlobTemperature::Cold
    }
}

#[cfg(target_os = "linux")]
const DEFAULT_MADVISE_LIMIT_BYTES: u64 = 32 * 1024 * 1024;

#[cfg(target_os = "linux")]
const ETMEM_MADVISE_LIMIT_BYTES: u64 = 256 * 1024 * 1024;

#[cfg(target_os = "linux")]
fn etmem_madvise_limit_bytes() -> u64 {
    if let Ok(raw) = env::var("MYVCS_ETMEM_MADVISE_LIMIT_MB") {
        if let Ok(mb) = raw.parse::<u64>() {
            return mb.max(1) * 1024 * 1024;
        }
    }
    if should_enable_etmem_tiering() {
        return ETMEM_MADVISE_LIMIT_BYTES;
    }
    DEFAULT_MADVISE_LIMIT_BYTES
}

#[cfg(target_os = "linux")]
fn try_madvise_file(file: &File, advice: libc::c_int) {
    let len = match file.metadata() {
        Ok(m) => m.len(),
        Err(_) => return,
    };
    let limit = etmem_madvise_limit_bytes();
    if len == 0 || len > limit {
        return;
    }

    let mapped = unsafe { MmapOptions::new().map(file) };
    if let Ok(m) = mapped {
        let _ = unsafe { libc::madvise(m.as_ptr() as *mut libc::c_void, m.len(), advice) };
    }
}

pub fn advise_hot_blob(file: &File) {
    #[cfg(target_os = "linux")]
    {
        if !should_enable_openeuler_io_hints() {
            return;
        }
        use std::os::fd::AsRawFd;
        let _ = unsafe { libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_WILLNEED) };
        try_madvise_file(file, libc::MADV_WILLNEED);
    }
}

pub fn advise_cold_blob(file: &File) {
    #[cfg(target_os = "linux")]
    {
        if !should_enable_openeuler_io_hints() {
            return;
        }
        use std::os::fd::AsRawFd;
        let _ = unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_DONTNEED)
        };
        let _ = unsafe {
            libc::posix_fadvise(file.as_raw_fd(), 0, 0, libc::POSIX_FADV_NOREUSE)
        };
        try_madvise_file(file, libc::MADV_COLD);
    }
}

impl std::fmt::Display for FsType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FsType::Ext4 => write!(f, "ext4"),
            FsType::Xfs => write!(f, "xfs"),
            FsType::Btrfs => write!(f, "btrfs"),
            FsType::EulerFs => write!(f, "eulerfs"),
            FsType::Other => write!(f, "other"),
        }
    }
}


pub fn adaptive_compression(temp: BlobTemperature) -> Compression {
    match temp {
        BlobTemperature::Hot => Compression::fast(),
        BlobTemperature::Warm => Compression::default(),
        BlobTemperature::Cold => Compression::best(),
    }
}

// ---------------------------------------------------------------------------
// 单元测试
// ---------------------------------------------------------------------------
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_delta_reconstructs_target() {
        let base = b"hello-AAAA-world";
        let target = b"hello-BBBB-world!!!";
        let ops = build_delta(base, target);
        let rebuilt = apply_delta(base, &ops).expect("apply delta should succeed");
        assert_eq!(rebuilt, target);
    }

    #[test]
    fn delta_encode_decode_roundtrip() {
        let delta = DeltaBlob {
            base: "abcd".to_string(),
            ops: vec![DeltaOp::Insert {
                data: b"payload".to_vec(),
            }],
            size: 7,
        };
        let encoded = encode_delta_blob(&delta).expect("encode should succeed");
        let decoded = decode_delta_blob(&encoded)
            .expect("decode should succeed")
            .expect("must be delta");
        assert_eq!(decoded.base, delta.base);
        assert_eq!(decoded.size, delta.size);
        assert_eq!(decoded.ops.len(), delta.ops.len());
    }

    #[test]
    fn hash_output_is_64_hex_chars() {
        let out = hex_sha256(b"abc");
        assert_eq!(out.len(), 64);
        assert!(out.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn parse_os_release_id_handles_quotes_and_case() {
        let raw = "NAME=openEuler\nID=\"openEuler\"\nVERSION=22.03\n";
        assert_eq!(parse_os_release_id(raw).as_deref(), Some("openeuler"));
    }

    #[test]
    fn parse_memtotal_kb_works() {
        let raw = "MemTotal:       2048000 kB\nMemFree:         512000 kB\n";
        assert_eq!(parse_memtotal_kb(raw), Some(2_048_000));
    }

    #[test]
    fn parse_loadavg_1_works() {
        let raw = "0.75 0.42 0.30 1/500 12345\n";
        assert_eq!(parse_loadavg_1(raw), Some(0.75));
    }
    #[test]
    fn fs_type_optimal_buffer_sizes() {
        assert_eq!(FsType::Ext4.optimal_write_buffer(), 128 * 1024);
        assert_eq!(FsType::Xfs.optimal_write_buffer(), 256 * 1024);
        assert_eq!(FsType::EulerFs.optimal_write_buffer(), 256 * 1024);
        assert_eq!(FsType::Btrfs.optimal_write_buffer(), 64 * 1024);
        assert!(FsType::Other.optimal_write_buffer() > 0);
    }

    #[test]
    fn fs_type_openeuler_native() {
        assert!(FsType::EulerFs.is_openeuler_native());
        assert!(FsType::Ext4.is_openeuler_native());
        assert!(!FsType::Xfs.is_openeuler_native());
        assert!(!FsType::Btrfs.is_openeuler_native());
    }

    #[test]
    fn blob_temperature_categorization() {
        let now = now_unix() as i64;
        assert_eq!(categorize_blob_temperature(now), BlobTemperature::Hot);
        assert_eq!(
            categorize_blob_temperature(now - 1800),
            BlobTemperature::Hot
        );
        assert_eq!(
            categorize_blob_temperature(now - 7200),
            BlobTemperature::Warm
        );
        assert_eq!(
            categorize_blob_temperature(now - 691200),
            BlobTemperature::Cold
        );
    }

    #[test]
    fn numa_topology_has_at_least_one_node() {
        let topo = detect_numa_topology();
        assert!(topo.node_count >= 1);
        assert!(!topo.cpu_to_node.is_empty());
    }

    #[test]
    fn fs_detect_returns_valid_type() {
        let fs = detect_fs_type(Path::new("."));
        let _ = fs.to_string();
        assert!(fs.optimal_write_buffer() > 0);
    }

    #[test]
    fn chunk_size_is_reasonable() {
        let sz = io_chunk_size_bytes();
        assert!(sz >= 64 * 1024);
        assert!(sz <= 4 * 1024 * 1024);
    }

    #[test]
    fn adaptive_compression_hot_is_fast() {
        // Compression::fast() 的 level 低于 default
        let hot = adaptive_compression(BlobTemperature::Hot);
        let default = adaptive_compression(BlobTemperature::Warm);
        assert!(hot.level() <= default.level());
    }

    #[test]
    fn adaptive_compression_cold_is_best() {
        let cold = adaptive_compression(BlobTemperature::Cold);
        let default = adaptive_compression(BlobTemperature::Warm);
        assert!(cold.level() >= default.level());
    }
}
