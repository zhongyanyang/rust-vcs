use super::*;
use sha2::{Digest, Sha256};
use std::sync::{Arc, Mutex};

use crate::util::{
    self, advise_file_dontneed, advise_file_sequential, detect_fs_type, detect_numa_topology,
};

impl Repo {
    const OBJECT_IO_HINT_THRESHOLD: u64 = 1024 * 1024;

    // 从当前目录向上查找 `.myvcs`，定位仓库根目录。
    //Path 和 PathBuf 的关系，类似 str 和 String
    pub fn discover() -> Result<Self> {
        let cwd = std::env::current_dir().context("failed to get current dir")?;
        crate::debug_log!("discover repo from cwd={}", cwd.display());
        let mut cur = cwd.as_path();
        //只要文件管理区（.meta文件夹）在当前目录的任一级目录里，就认为找到了仓库根目录
        loop {
            let meta = cur.join(META_DIR);
            if meta.is_dir() {
                return Ok(Self {
                    root: cur.to_path_buf(),
                    meta,
                    commit_graph: Arc::new(Mutex::new(None)),
                });
            }
            //让 cur 指向上一级路径；如果没有上一级，就退出循环
            cur = match cur.parent() {
                Some(p) => p,  //有父路径，把父路径赋给 cur
                None => break,  // 没有父路径，退出外层循环
            };
        }
        bail!("not a myvcs repository (run `myvcs init` first)")
    }
    //在持锁状态下执行闭包f,执行完后解锁
    pub fn with_repo_lock<T, F>(&self, f: F) -> Result<T>
    where
        F: FnOnce(&Self) -> Result<T>,//f必须是个具有可调用能力的东西，可以是函数或闭包
    {
        // 仓库级单锁保证关键元数据写入串行化。
        let lock_path = self.meta.join("repo.lock");
        crate::debug_log!("acquire lock {}", lock_path.display());
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open lock file {}", lock_path.display()))?;
        Self::lock_repo_file(&lock)?;
        let result = f(self);
        let unlock_result = Self::unlock_repo_file(&lock);
        crate::debug_log!("release lock {}", lock_path.display());
        if let Err(e) = unlock_result {
            eprintln!("warning: failed to release lock: {}", e);
        }
        result
    }
    //给仓库文件加独占锁
    fn lock_repo_file(lock: &File) -> Result<()> {
        #[cfg(unix)]
        {
            Self::lock_repo_file_fcntl(lock)?;
        }
        //尝试获取独占锁。通常其他进程已经持有冲突锁时，这里会阻塞等待
        lock.lock_exclusive()
            .context("failed to acquire repository lock")?;
        Ok(())  //表示所有加锁步骤都成功了
        //函数返回并不表示解锁。锁通常会继续保持，直到显式解锁或持锁文件被关闭
    }
    //给仓库文件解锁
    fn unlock_repo_file(lock: &File) -> Result<()> {
        #[cfg(unix)]
        {
            Self::unlock_repo_file_fcntl(lock)?;
        }
        lock.unlock().context("failed to release repository lock")?;
        Ok(())
    }

    #[cfg(unix)]
    fn lock_repo_file_fcntl(lock: &File) -> Result<()> {
        use std::os::fd::AsRawFd;

        let mut fl = libc::flock {
            l_type: libc::F_WRLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };

        let rc = unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETLKW, &mut fl) };
        if rc == -1 {
            return Err(anyhow!(
                "failed to acquire repository lock (fcntl): {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }

    #[cfg(unix)]
    fn unlock_repo_file_fcntl(lock: &File) -> Result<()> {
        use std::os::fd::AsRawFd;

        let mut fl = libc::flock {
            l_type: libc::F_UNLCK as libc::c_short,
            l_whence: libc::SEEK_SET as libc::c_short,
            l_start: 0,
            l_len: 0,
            l_pid: 0,
        };

        let rc = unsafe { libc::fcntl(lock.as_raw_fd(), libc::F_SETLK, &mut fl) };
        if rc == -1 {
            return Err(anyhow!(
                "failed to release repository lock (fcntl): {}",
                std::io::Error::last_os_error()
            ));
        }
        Ok(())
    }
//在当前目录里初始化一个仓库
    pub fn init_here() -> Result<()> {
        //准备仓库路径
        let root = std::env::current_dir().context("failed to get current dir")?;
        let meta = root.join(META_DIR);
        //检查有没有错建同名文件
        if meta.exists() && !meta.is_dir() {
            bail!(
                "metadata path exists but is not a directory: {}",
                meta.display()
            );
        }
        //没有则创建.myvcs文件夹
        fs::create_dir_all(&meta)
            .with_context(|| format!("failed to create meta directory {}", meta.display()))?;

        // init 使用同一仓库锁，避免并发初始化写入冲突。
        //准备锁路径
        let lock_path = meta.join("repo.lock");
        crate::debug_log!("acquire init lock {}", lock_path.display());
        //创建锁文件
        let lock = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&lock_path)
            .with_context(|| format!("failed to open init lock {}", lock_path.display()))?;
        //上锁，防止并发初始化
        Self::lock_repo_file(&lock)?;

        //result等于右边这个闭包的执行结果
        let result = (|| -> Result<()> {
            //准备仓库的各个目录和文件的路径
            let objects = meta.join("objects");
            let heads_dir = meta.join("refs").join("heads");
            let head_file = meta.join("HEAD");
            let main_ref = heads_dir.join("main");
            let index_file = meta.join("index.json");
            //检查是否已经初始化过了
            let already_initialized =
                head_file.is_file() && main_ref.is_file() && index_file.is_file();
            //补全仓库结构。防止重复运行初始化，创建文件时先检查一遍当前有没有
            fs::create_dir_all(&objects)?;//缺失才创建对象库
            fs::create_dir_all(&heads_dir)?;//缺失才创建分支列表            
            if !head_file.exists() {
                fs::write(&head_file, "ref: refs/heads/main\n")?;
            }
            if !main_ref.exists() {
                fs::write(&main_ref, "")?;
            }
            if !index_file.exists() {
                fs::write(&index_file, "{\"entries\":{}}\n")?;
            }

            if already_initialized {
                // 输出 FS/NUMA 诊断信息（openEuler 优化可观测性）
                if util::should_enable_openeuler_io_hints() {
                    let fs = detect_fs_type(&meta);
                    let numa = detect_numa_topology();
                    crate::debug_log!(
                        "repo fs={} io_chunk={} numa_nodes={}",
                        fs,
                        util::io_chunk_size_bytes(),
                        numa.node_count
                    );
                }
                println!("repository already initialized: {}", meta.display());
                Ok(())
            } else {
                let repo = Repo {
                    root: root.clone(),
                    meta: meta.clone(),
                    commit_graph: Arc::new(Mutex::new(None)),
                };

                let author = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
                let commit = Commit {
                    tree: BTreeMap::new(),
                    parents: Vec::new(),
                    parent: None,
                    message: "Initial commit".to_string(),
                    author,
                    timestamp: now_unix() as i64,
                };
                //写入到硬盘
                let raw = serde_json::to_vec(&commit)?;//序列化成字节
                //计算SHA-256哈希
                let id = repo.write_object_with_temp(&raw, crate::util::BlobTemperature::Hot)?;//按热对象压缩内容
                //main 文件只保存当前最新提交的对象 ID
                //旧提交仍然留在对象库里，通过新提交的 parent 或 parents 可以继续找到
                repo.atomic_write(&main_ref, format!("{}\n", id).as_bytes())?;

                let fs = detect_fs_type(&meta);
                let numa = detect_numa_topology();
                println!("initialized empty myvcs repository in {}", meta.display());
                crate::debug_log!(
                    "init fs={} io_chunk={} numa_nodes={}",
                    fs,
                    util::io_chunk_size_bytes(),
                    numa.node_count
                );
                Ok(())
            }
        })();

        let unlock_result = Self::unlock_repo_file(&lock);
        crate::debug_log!("release init lock {}", lock_path.display());
        if let Err(e) = unlock_result {
            eprintln!("warning: failed to release init lock: {}", e);
        }

        result
    }

    pub(super) fn objects_dir(&self) -> PathBuf {
        self.meta.join("objects")
    }

    pub(super) fn index_path(&self) -> PathBuf {
        self.meta.join("index.json")
    }

    pub(super) fn status_cache_path(&self) -> PathBuf {
        self.meta.join("status-cache.json")
    }

    pub(super) fn merge_conflicts_path(&self) -> PathBuf {
        self.meta.join("merge-conflicts.txt")
    }

    pub(super) fn read_index(&self) -> Result<Index> {
        let p = self.index_path();
        if !p.exists() {
            return Ok(Index::default());
        }
        //从文件index.json里读取index数据,返回json字符串
        let data = fs::read_to_string(&p)
            .with_context(|| format!("failed to read index {}", p.display()))?;
        Ok(serde_json::from_str(&data).context("invalid index format")?)  //解析 JSON 字符串，转换成 Index 类型
        //可见，Index的内容为了持久化，存进了index.json文件里
    }

    pub(super) fn write_index(&self, index: &Index) -> Result<()> {
        self.atomic_write_json(&self.index_path(), index)
    }
    /// 没有缓存文件 → 使用默认缓存；有缓存文件 → 读取并解析
    pub(super) fn read_status_cache(&self) -> Result<StatusCache> {
        let p = self.status_cache_path();
        if !p.exists() {
            return Ok(StatusCache::default());
        }
        let data = fs::read_to_string(&p)
            .with_context(|| format!("failed to read status cache {}", p.display()))?;
        Ok(serde_json::from_str(&data).context("invalid status cache format")?)
    }

    pub(super) fn write_status_cache(&self, cache: &StatusCache) -> Result<()> {
        self.atomic_write_json(&self.status_cache_path(), cache)
    }
    ///读取仓库记录的“尚未解决的合并冲突文件列表”
    //例如，合并两个分支时，双方都修改了同一处内容，程序无法自动决定保留哪一份，就会产生冲突。
    //仓库可以用一个文件记录这些路径，每行一个
    pub(super) fn read_merge_conflicts(&self) -> Result<Vec<String>> {
        let p = self.merge_conflicts_path();
        //记录文件不存在：返回空列表，表示没有记录中的冲突。
        if !p.exists() {
            return Ok(Vec::new());
        }
        //记录文件存在：读取每一行，去掉首尾空白和空行，返回文件路径列表
        let raw = fs::read_to_string(&p)
            .with_context(|| format!("failed to read merge conflict file {}", p.display()))?;
        let mut out = Vec::new();
        for line in raw.lines() {
            let v = line.trim();
            if !v.is_empty() {
                out.push(v.to_string());
            }
        }
        Ok(out)
    }

    /// 将合并冲突的文件路径持久化为清单（每行一个），供后续 status 等命令读取。
    /// 合并整合两个分支的修改；每个提交仍记录完整文件快照，文件路径重叠不代表冲突。
    /// 本项目按文件对象 ID 做三方比较：以共同祖先为基准，只有一方修改则采用该方，
    /// 双方内容相同也可直接采用；双方都修改且结果不同（含修改与删除）才标记冲突。
    /// 此处只保存路径，不检测或解决冲突，也不保存冲突内容；空列表表示删除清单。
    pub(super) fn write_merge_conflicts(&self, conflicts: &[String]) -> Result<()> {
        let p = self.merge_conflicts_path();
        if conflicts.is_empty() {
            if p.exists() {
                fs::remove_file(&p).with_context(|| {
                    format!("failed to remove merge conflict file {}", p.display())
                })?;
            }
            return Ok(());
        }

        let mut body = String::new();
        for c in conflicts {
            body.push_str(c);
            body.push('\n');
        }
        self.atomic_write(&p, body.as_bytes())
            .with_context(|| format!("failed to write merge conflict file {}", p.display()))
    }

    pub(super) fn clear_merge_conflicts(&self) -> Result<()> {
        self.write_merge_conflicts(&[])
    }
    // 'refs/heads/'是分支池，'refs/heads/分支名' 记录某个分支最新的提交
    // 'HEAD' 表示当前所在的分支
    /// 读取 HEAD 文件内容，返回分支引用
    pub(super) fn head_raw(&self) -> Result<String> {
        let head_path = self.meta.join("HEAD");
        let s = fs::read_to_string(&head_path)
            .with_context(|| format!("failed to read HEAD {}", head_path.display()))?;
        Ok(s.trim().to_string()) //s.trim()：去掉首尾的空格、换行、制表符等，返回 &str
        //可见，HEAD为了持久化，放在了.meta/HEAD文件里，内容是当前分支的引用，比如 refs/heads/main
    }
    ///获取当前分支。从HEAD文件里读取分支引用，返回分支文件路径
    pub(super) fn current_ref(&self) -> Result<Option<String>> {
        let raw = self.head_raw()?; //"ref: refs/heads/main"
        //raw 以 "ref:" 开头 → 返回 Some(去掉前缀后的内容)
        //不以 "ref:" 开头 → 返回 None。
        if let Some(rest) = raw.strip_prefix("ref:") { //" refs/heads/main"
            return Ok(Some(rest.trim().to_string()));  //"refs/heads/main"
        }
        Ok(None)
    }
    ///返回当前分支的最新提交id
    pub(super) fn head_commit_id(&self) -> Result<Option<String>> {
        //从HEAD文件里有没有读取到分支引用
        match self.current_ref()? {
            Some(r) => {
                let p = self.meta.join(r);
                if !p.exists() {
                    return Ok(None);
                }
                //读取分支文件里的最新提交id
                let s = fs::read_to_string(&p)
                    .with_context(|| format!("failed to read ref {}", p.display()))?;
                let id = s.trim().to_string();
                if id.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(id))
                }
            }
            None => {
                //没有就自己读取再处理(分离头情况)
                let raw = self.head_raw()?;
                if raw.is_empty() {
                    Ok(None)
                } else {
                    Ok(Some(raw))
                }
            }
        }
    }

    /// 将当前位置更新为指定提交，不修改提交对象本身。
    /// 在分支上时，将提交 ID 写入当前分支的引用文件，HEAD 仍指向该分支；
    /// 分离 HEAD 时，将提交 ID 直接写入 HEAD 文件。
    /// 使用原子写入；读取引用、创建目录或写入失败时返回错误。
    pub(super) fn update_head_to_commit(&self, commit_id: &str) -> Result<()> {
        if let Some(r) = self.current_ref()? {
            let p = self.meta.join(r);
            if let Some(parent) = p.parent() {
                fs::create_dir_all(parent)?;
            }
            self.atomic_write(&p, format!("{}\n", commit_id).as_bytes())?;
        } else {
            self.atomic_write(
                &self.meta.join("HEAD"),
                format!("{}\n", commit_id).as_bytes(),
            )?;
        }
        Ok(())
    }

    ///对输入参数name进行解析，如果name是commitid就直接返回
    ///如果是分支名就读取分支文件里的最新提交id返回
    pub fn resolve_commit(&self, name: &str) -> Result<String> {
        if self.has_object(name)? {
            return Ok(name.to_string());
        }

        let branch_path = self.meta.join("refs").join("heads").join(name);
        if branch_path.exists() {
            let id = fs::read_to_string(&branch_path)
                .with_context(|| format!("failed to read branch ref {}", branch_path.display()))?
                .trim()
                .to_string();
            if id.is_empty() {
                bail!("branch `{}` has no commits", name);
            }
            return Ok(id);
        }

        bail!("unknown commit or branch: {}", name)
    }

    pub(super) fn has_object(&self, hash: &str) -> Result<bool> {
        let p = self.object_path(hash)?;
        Ok(p.exists())
    }
    /// 根据对象的哈希字符串，计算它应该存放的文件路径
    pub(super) fn object_path(&self, hash: &str) -> Result<PathBuf> {
        if hash.len() < 3 {
            bail!("invalid object hash")
        }
        //把字符串从第 2 个字节处切开，并分别命名为 dir 和 file
        let (dir, file) = hash.split_at(2);
        Ok(self.objects_dir().join(dir).join(file))
    }

    /// 写入对象（默认压缩级别）。
    #[allow(dead_code)]
    pub(super) fn write_object(&self, bytes: &[u8]) -> Result<String> {
        self.write_object_with_temp(bytes, crate::util::BlobTemperature::Warm)
    }

    /// 写入对象，按温度自适应选择 zlib 压缩级别。
    /// Hot→fast, Warm→default, Cold→best。
    pub(super) fn write_object_with_temp(
        &self,
        bytes: &[u8],
        temp: crate::util::BlobTemperature,
    ) -> Result<String> {
        let hash = hex_sha256(bytes);
        let p = self.object_path(&hash)?;
        if p.exists() {
            return Ok(hash);
        }

        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }

        let level = crate::util::adaptive_compression(temp);
        // 预分配压缩缓冲区：zlib 典型压缩率 30%-70%，取 2/3 容量
        let estimated = bytes.len() * 2 / 3 + 64;
        let mut encoder = ZlibEncoder::new(Vec::with_capacity(estimated), level);
        encoder.write_all(bytes)?;
        let compressed = encoder.finish()?;
        self.atomic_write_object(&p, &compressed)?;

        Ok(hash)
    }

    pub(super) fn write_object_from_path(&self, path: &Path) -> Result<String> {
        let meta = fs::metadata(path)
            .with_context(|| format!("failed to stat file {}", path.display()))?;
        if meta.len() < crate::util::LARGE_FILE_THRESHOLD {
            let bytes = read_file_for_blob(path)
                .with_context(|| format!("failed to read file {}", path.display()))?;
            // 新暂存文件为 Hot，使用快速压缩
            return self.write_object_with_temp(&bytes, crate::util::BlobTemperature::Hot);
        }

        let mut f =
            File::open(path).with_context(|| format!("failed to open file {}", path.display()))?;
        advise_file_sequential(&f);

        // openEuler 优化：流式写入使用对象库所在 FS 对齐缓冲区
        let chunk = if crate::util::should_enable_openeuler_io_hints() {
            let objects_dir = self.objects_dir();
            detect_fs_type(&objects_dir).optimal_write_buffer()
        } else {
            crate::util::io_chunk_size_bytes()
        };

        let tmp_dir = self.objects_dir().join("tmp");
        fs::create_dir_all(&tmp_dir)?;
        let tmp_path = tmp_dir.join(format!("stream-{}-{}", now_unix(), std::process::id()));
        let tmp_file = File::create(&tmp_path)?;
        let estimated = meta.len().saturating_mul(2).saturating_div(3).saturating_add(64);
        if let Some(prealloc) = crate::util::eulerfs_prealloc_len(&self.objects_dir(), estimated)
        {
            crate::util::preallocate_file(&tmp_file, prealloc);
        }
        let mut encoder = ZlibEncoder::new(tmp_file, Compression::default());

        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; chunk];
        loop {
            let n = f.read(&mut buf[..])?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
            encoder.write_all(&buf[..n])?;
        }

        let mut tmp_file = encoder.finish()?;
        tmp_file.flush()?;
        advise_file_dontneed(&f);

        let hash = crate::util::digest_to_hex(&hasher.finalize());
        let p = self.object_path(&hash)?;
        if p.exists() {
            let _ = fs::remove_file(&tmp_path);
            return Ok(hash);
        }

        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }

        if let Err(e) = fs::rename(&tmp_path, &p) {
            if p.exists() {
                let _ = fs::remove_file(&tmp_path);
            } else {
                return Err(e.into());
            }
        }

        Ok(hash)
    }
    /// 找到对象文件，读取并解压，返回解压后的字节数据
    pub(super) fn read_object(&self, hash: &str) -> Result<Vec<u8>> { //Vec<u8> 是一堆字节组成的可变数组
        let p = self.object_path(hash)?;
        let mut f = File::open(&p).with_context(|| format!("object not found: {}", hash))?;
        //获取文件信息,如果获取成功，就把文件信息转换成文件大小，单位是字节
        //如果获取失败，就用 0
        let len = f.metadata().map(|m| m.len()).unwrap_or(0);
        //大文件读取优化
        if len >= Self::OBJECT_IO_HINT_THRESHOLD { //如果文件达到指定大小，就给操作系统一些提示
            advise_file_sequential(&f); //接下来会顺序读取这个文件
            // openEuler 优化：对大对象预读（WILLNEED）以减少缺页抖动
            crate::util::advise_hot_blob(&f); //希望提前把文件数据读入缓存
        }
        //创建一个解压读取器，底层数据来自文件 f
        let mut decoder = ZlibDecoder::new(&mut f);
        //创建一个空的字节数组 out
        let mut out = Vec::new();
        //不断读取、解压，把结果追加到 out，直到读完
        decoder.read_to_end(&mut out)?;

        //大文件读完后，提示操作系统可以回收相关文件缓存。最后返回解压后的 out
        if len >= Self::OBJECT_IO_HINT_THRESHOLD {
            advise_file_dontneed(&f);
        }
        Ok(out)
    }

    // 读取 blob；若为 delta 则递归展开整条增量链。
    ///根据对象的 hash，读取并还原出文件的完整内容
    pub(super) fn read_blob_resolved(&self, hash: &str) -> Result<Vec<u8>> {
        let mut seen = BTreeSet::new();
        self.read_blob_resolved_inner(hash, &mut seen)
    }

    pub(super) fn read_blob_resolved_inner(
        &self,
        hash: &str,
        seen: &mut BTreeSet<String>,
    ) -> Result<Vec<u8>> {
        if !seen.insert(hash.to_string()) {
            bail!("delta chain cycle detected at {}", hash);
        }
        //增量存储指“基准 ID + 重建指令”。
        let raw = self.read_object(hash)?;
        if let Some(delta) = decode_delta_blob(&raw)? {
            let base = self.read_blob_resolved_inner(&delta.base, seen)?;
            let out = apply_delta(&base, &delta.ops)?;
            if out.len() != delta.size {
                bail!(
                    "delta reconstructed size mismatch for {}: expected {}, got {}",
                    hash,
                    delta.size,
                    out.len()
                );
            }
            return Ok(out);
        }

        Ok(raw)
    }

    // 以既有对象 ID 覆写存储负载（用于 gc 重打包）。
    pub(super) fn write_object_for_existing_oid(&self, oid: &str, payload: &[u8]) -> Result<()> {
        let p = self.object_path(oid)?;
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent)?;
        }

        // GC 重打包使用 Cold 级别，最大化压缩率
        let level = crate::util::adaptive_compression(crate::util::BlobTemperature::Cold);
        let estimated = payload.len() * 2 / 3 + 64;
        let mut encoder = ZlibEncoder::new(Vec::with_capacity(estimated), level);
        encoder.write_all(payload)?;
        let compressed = encoder.finish()?;
        self.atomic_write_object(&p, &compressed)
    }
    ///读取指定提交对象，返回 Commit 类型
    pub(super) fn read_commit(&self, commit_id: &str) -> Result<Commit> {
        let raw = self.read_object(commit_id)?;
        //把 raw 中的 JSON 字节数据解析成Commit类型，返回 Commit 对象
        Ok(serde_json::from_slice(&raw).context("invalid commit object")?)
    }

    pub(super) fn commit_parents(commit: &Commit) -> Vec<String> {
        if !commit.parents.is_empty() {
            return commit.parents.clone();
        }
        match &commit.parent {
            Some(p) => vec![p.clone()],
            None => Vec::new(),
        }
    }
    ///获取 HEAD 所指提交的文件快照 tree
    pub(super) fn head_tree(&self) -> Result<BTreeMap<String, String>> {
        match self.head_commit_id()? {
            Some(id) => Ok(self.read_commit(&id)?.tree),//返回的tree描述了该提交包含哪些文件
            None => Ok(BTreeMap::new()),
        }
    }

    pub(super) fn current_branch_name(&self) -> Option<String> {
        let raw = self.head_raw().ok()?;
        let r = raw.strip_prefix("ref:")?.trim();
        Some(r.strip_prefix("refs/heads/")?.to_string())
    }

    pub(super) fn atomic_write_json<T: Serialize>(&self, path: &Path, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec_pretty(value)?;
        self.atomic_write(path, &bytes)
    }

    pub(super) fn atomic_write_object(&self, path: &Path, data: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", now_unix()));
        {
            let mut f = File::create(&tmp)?;
            if let Some(prealloc) = crate::util::eulerfs_prealloc_len(
                &self.objects_dir(),
                data.len() as u64,
            ) {
                crate::util::preallocate_file(&f, prealloc);
            }
            f.write_all(data)?;
            f.flush()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }

    // 先写临时文件，再原子重命名覆盖目标文件。
    pub(super) fn atomic_write(&self, path: &Path, data: &[u8]) -> Result<()> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension(format!("tmp-{}", now_unix()));
        {
            let mut f = File::create(&tmp)?;
            f.write_all(data)?;
            f.flush()?;
        }
        fs::rename(&tmp, path)?;
        Ok(())
    }
}
