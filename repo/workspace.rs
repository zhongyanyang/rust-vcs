use super::*;
use std::ffi::OsStr;
use std::time::UNIX_EPOCH;

impl Repo {
    /// 枚举工作区候选文件，跳过元数据与构建目录。
    fn tracked_working_files(&self) -> Result<Vec<String>> {
        let mut files = Vec::new();
        for entry in WalkDir::new(&self.root)
            .into_iter()//转换成迭代器
            .filter_entry(|e| !Self::should_skip(e.path(), &self.root))//过滤掉不想要的文件
        {
            let entry = entry?;
            let p = entry.path();
            if p.is_file() {
                let rel = p
                    .strip_prefix(&self.root)?
                    .to_string_lossy()
                    .replace('\\', "/");
                files.push(rel);
            }
        }
        files.sort();
        Ok(files)
    }
    /// 依据根目录下的第一层名称，判断是否是应该被跳过的元数据目录、构建目录或 Git 仓库目录
    fn should_skip(path: &Path, root: &Path) -> bool {
        let Some(rel) = path.strip_prefix(root).ok() else {
            return false;
        };
        let Some(first) = rel.components().next() else {
            return false;
        };

        let name = first.as_os_str();
        name == OsStr::new(META_DIR) || name == OsStr::new("target") || name == OsStr::new(".git")
    }

    // 将文件或目录递归加入暂存区。
    pub fn add_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            bail!("usage: myvcs add <path> [path ...]")
        }
        crate::debug_log!("add paths count={}", paths.len());

        let mut index = self.read_index()?;
        let mut added = 0usize;
        //paths实际上是文件名/文件夹名
        for raw in paths {
            //root是std::env::current_dir()，进入工作区，运行程序的地方，下面有个.myvsc
            let p = self.root.join(raw);
            if !p.exists() {
                bail!("path does not exist: {}", raw);
            }

            if p.is_file() {
                self.stage_one(&p, &mut index)?;
                added += 1;
                continue;
            }
            //是个目录
            for e in WalkDir::new(&p)
                .into_iter()
                .filter_entry(|e| !Self::should_skip(e.path(), &self.root))//跳过不想要的文件
            {
                let e = e?;
                if e.path().is_file() {
                    self.stage_one(e.path(), &mut index)?;
                    added += 1;
                }
            }
        }
        //把内存里的index保存到磁盘
        self.write_index(&index)?;
        println!("staged {} file(s)", added);
        Ok(())
    }

    // 将路径删除加入暂存，并在工作区存在时删除文件。
    pub fn rm_paths(&self, paths: &[String]) -> Result<()> {
        if paths.is_empty() {
            bail!("usage: myvcs rm <path> [path ...]")
        }

        let mut index = self.read_index()?;
        let head_tree = self.head_tree()?;
        //把当前提交和暂存区里的文件路径合并、去重，保存到 tracked 集合
        //因为刚提交成功后又执行add，index就可能包含与上次提交相同的文件
        let tracked: BTreeSet<String> = head_tree
            .keys()
            .chain(index.entries.keys())
            .cloned()
            .collect();

        let mut removed_count = 0usize;
        let mut touched = BTreeSet::new();

        for raw in paths {
            let candidate = self.root.join(raw);
            let mut targets = Vec::new();

            if candidate.exists() {//在当前工作区里面找
                if candidate.is_file() {
                    //把 完整路径转成仓库内的相对路径，并统一用 / 分隔
                    let rel = candidate
                        .strip_prefix(&self.root)
                        .context("path is outside repository")?
                        .to_string_lossy() //把路径转换成字符串
                        .replace('\\', "/");
                    targets.push(rel);
                } else {
                    for e in WalkDir::new(&candidate)
                        .into_iter()
                        .filter_entry(|e| !Self::should_skip(e.path(), &self.root))
                    {
                        let e = e?;
                        if e.path().is_file() {
                            let rel = e
                                .path()
                                .strip_prefix(&self.root)
                                .context("path is outside repository")?
                                .to_string_lossy()
                                .replace('\\', "/");
                            targets.push(rel);
                        }
                    }
                }
            } else {//在 tracked 中找出与用户输入匹配的路径，加入待处理列表 targets
                let prefix = raw.trim_end_matches('/');  //去掉输入末尾的 /
                for p in &tracked {
                    if p == prefix || p.starts_with(&format!("{}/", prefix)) {  //后者说明文件在要删除的目录下
                        targets.push(p.clone());
                    }
                }
            }

            if targets.is_empty() {
                bail!("path does not exist: {}", raw);
            }

            for rel in targets {
                //跳过已经处理过的路径
                if !touched.insert(rel.clone()) {
                    continue;
                }
                //如果文件不在已跟踪的路径集合里，也跳过
                if !tracked.contains(&rel) {
                    continue;
                }
                //更新暂存区
                let mut changed = false; //表示这个文件的暂存记录是否发生了变化
                if index.entries.remove(&rel).is_some() {
                    changed = true;
                }
                if index.removed.insert(rel.clone()) {
                    changed = true;
                }
                //删除磁盘/工作区上的文件
                let fp = self.root.join(&rel);
                if fp.exists() {
                    fs::remove_file(&fp)
                        .with_context(|| format!("failed to remove file {}", fp.display()))?;
                }
                //只有暂存记录发生变化，才增加计数
                if changed {
                    removed_count += 1;
                }
            }
        }
        //把更新后的暂存区保存到磁盘
        self.write_index(&index)?;
        println!("staged removal of {} file(s)", removed_count);
        Ok(())
    }
    //path是一个文件路径
    ///把一个文件的内容保存成对象，然后把文件信息记录进暂存区
    fn stage_one(&self, path: &Path, index: &mut Index) -> Result<()> {
        if Self::should_skip(path, &self.root) {
            return Ok(());
        }
        //这一串是在生成文件的仓库内相对路径
        let rel = path
            .strip_prefix(&self.root)
            .context("path is outside repository")?
            .to_string_lossy()   //把路径转换成可用的字符串
            .replace('\\', "/");    //把 Windows 的 \ 统一改成 /
        //读取文件内容，把内容保存到对象库，并得到一个对象 ID
        let oid = self
            .write_object_from_path(path)
            .with_context(|| format!("failed to write blob for {}", rel))?;
        //文件的相对路径和它在对象库里的id
        index.entries.insert(rel.clone(), oid);
        index.removed.remove(&rel);     // 取消待删除状态，防止同一个文件会同时处于两种冲突状态
        Ok(())
    }

    // 展示已暂存、未暂存、未跟踪与冲突状态。
    pub fn status(&self) -> Result<()> {
        let head_tree = self.head_tree()?;
        let index = self.read_index()?;
        let files = self.tracked_working_files()?;
        let status_cache = self.read_status_cache()?;
        let mut new_cache = StatusCache::default();
        let conflicts = self.read_merge_conflicts()?;

        let mut staged = Vec::new();//暂存区与上次提交的差异
        
        //准备提交的内容，与上次提交相比有没有变化？
        //收集已暂存的新增操作
        for (p, oid) in &index.entries {
            match head_tree.get(p) {
                Some(h) if h == oid => {}//版本号一样，说明这个暂存文件没有变化
                _ => staged.push(p.clone()),
            }
        }
        //收集已暂存的删除操作（并且对象是上次提交的内容）
        for p in &index.removed {
            if head_tree.contains_key(p) {
                staged.push(format!("deleted: {}", p));
            }
        }
        //工作区与暂存区、已经提交的比较，收集 modified
        let mut modified = Vec::new();
        let tracked: BTreeSet<_> = head_tree
            .keys()
            .chain(index.entries.keys())
            .chain(index.removed.iter())//先遍历前面的内容，再遍历 removed 集合中的文件路径
            .cloned()
            .collect();
        for p in tracked {
            let fp = self.root.join(&p);
            //文件不存在：记录删除
            if !fp.exists() {
                if head_tree.contains_key(&p) {
                    modified.push(format!("deleted: {}", p));
                }
                continue;
            }
            //文件存在：获取文件信息，计算或复用内容哈希
            //metadata 读取文件的属性
            let md = fs::metadata(&fp)
                .with_context(|| format!("failed to stat working file {}", fp.display()))?;
            let mtime_secs = md
                .modified()    // 获取修改时间：Result
                .ok()   // 成功变 Some，失败变 None
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())  // 计算距离 1970-01-01 的时长
                .map(|d| d.as_secs())  // 把时长转换成秒数
                .unwrap_or(0);  // 任一步失败，就使用 0
            let size = md.len();
            //文件 p 的暂存版本哈希，没有暂存记录则使用 HEAD 版本
            let staged_hash = index
                .entries
                .get(&p)
                .cloned()
                .or_else(|| head_tree.get(&p).cloned());//or_else 表示：前面是 Some 就直接使用；前面是 None，才执行闭包，尝试后备查找

            if index.removed.contains(&p) {
                continue;
            }
            //工作区哈希与暂存版本哈希不同：记录未暂存修改
            if let Some(staged_hash) = staged_hash {
                let working_hash = if let Some(prev) = status_cache.entries.get(&p) {
                    if prev.mtime_secs == mtime_secs && prev.size == size {
                        prev.hash.clone()
                    } else {
                        hex_sha256_file(&fp).with_context(|| {
                            format!("failed to hash working file {}", fp.display())
                        })?
                    }
                } else {
                    hex_sha256_file(&fp)
                        .with_context(|| format!("failed to hash working file {}", fp.display()))?
                };

                new_cache.entries.insert(
                    p.clone(),
                    StatusCacheEntry {
                        mtime_secs,
                        size,
                        hash: working_hash.clone(),
                    },
                );

                if staged_hash != working_hash {
                    modified.push(format!("modified: {}", p));
                }
            }
        }

        // 缓存写入采用尽力而为策略；写失败不影响 status 输出。
        if let Err(e) = self.write_status_cache(&new_cache) {
            crate::debug_log!("status cache write skipped: {}", e);
        }
        //收集未跟踪文件：工作区存在，但不在暂存区，也不在上次提交中
        let tracked_set: BTreeSet<_> = tracked_working_set(&head_tree, &index);
        let mut untracked = Vec::new();
        for p in files {
            if !tracked_set.contains(&p) {
                untracked.push(p);
            }
        }

        println!(
            "On branch {}",
            self.current_branch_name()
                .unwrap_or_else(|| "(detached HEAD)".to_string())
        );

        if staged.is_empty() {
            println!("\nNo staged changes");
        } else {
            println!("\nChanges to be committed:");
            for p in staged {
                println!("  {}", p);
            }
        }

        if modified.is_empty() {
            println!("\nNo unstaged changes");
        } else {
            println!("\nChanges not staged for commit:");
            for p in modified {
                println!("  {}", p);
            }
        }

        if untracked.is_empty() {
            println!("\nNo untracked files");
        } else {
            println!("\nUntracked files:");
            for p in untracked {
                println!("  {}", p);
            }
        }

        if !conflicts.is_empty() {
            println!("\nMerge conflicts:");
            for p in conflicts {
                println!("  {}", p);
            }
        }

        Ok(())
    }

    /// 对比工作区与 HEAD 快照差异。
    pub fn diff(&self, path: Option<&str>) -> Result<()> {
        let head_tree = self.head_tree()?;
        let mut targets = Vec::new();
        if let Some(p) = path {
            targets.push(p.to_string());
        } else {
            let set: BTreeSet<String> = head_tree.keys().cloned().collect();
            targets.extend(set);  //把 set 中的所有元素逐个追加到 targets 中
        }

        if targets.is_empty() {
            println!("nothing to diff");
            return Ok(());
        }

        for p in targets {
            let old = match head_tree.get(&p) {
                Some(oid) => Some(self.read_blob_resolved(oid)?),
                None => None,
            };
            let work_path = self.root.join(&p);
            let new = if work_path.exists() {
                Some(fs::read(&work_path).with_context(|| {
                    format!("failed to read working file {}", work_path.display())
                })?)
            } else {
                None
            };

            if old == new {
                continue;
            }

            println!("diff -- {}", p);
            print_text_diff_or_binary(&p, old.as_deref(), new.as_deref());
            println!();
        }

        Ok(())
    }
}
