use super::*;
use super::commit_graph::CommitGraph;

impl Repo {
    /// 管理分支：传入 delete 时删除分支，否则传入 name 时创建分支，均未传入时列出分支。
    /// 每个分支对应 refs/heads 下的一个引用文件，内容为该分支指向的提交 ID。
    pub fn branch(&self, name: Option<&str>, delete: Option<&str>) -> Result<()> {
        let heads = self.meta.join("refs").join("heads");
        fs::create_dir_all(&heads)?;

        // 删除优先处理；只移除分支引用，不删除提交对象或工作区文件。
        if let Some(del_name) = delete {
            let p = heads.join(del_name);
            if !p.exists() {
                bail!("branch does not exist: {}", del_name);
            }
            fs::remove_file(p)?;
            println!("deleted branch {}", del_name);
            return Ok(());
        }

        // 新分支指向当前 HEAD 的提交；尚无提交时无法创建分支。
        // 这里只写入提交 ID，共享已有提交和历史，不复制文件，也不切换当前分支。
        if let Some(name) = name {
            let head = self
                .head_commit_id()?
                .ok_or_else(|| anyhow!("cannot create branch without commit"))?;
            self.atomic_write(&heads.join(name), format!("{}\n", head).as_bytes())?;
            println!("created branch {}", name);
            return Ok(());
        }

        // 未指定创建或删除操作时，收集分支名，排序后输出，并用 * 标记当前分支。
        let cur = self.current_branch_name();
        let mut names = Vec::new();
        for e in fs::read_dir(&heads)? {
            let e = e?;
            if e.path().is_file() {
                names.push(e.file_name().to_string_lossy().to_string());
            }
        }
        names.sort();
        for n in names {
            if cur.as_deref() == Some(n.as_str()) {
                println!("* {}", n);
            } else {
                println!("  {}", n);
            }
        }
        Ok(())
    }

    /// 将工作区切换到目标分支或指定提交，并清空暂存区和合并冲突状态。
    /// `target` 可以是分支名，也可以是提交 ID（包括历史提交）。
    /// 切换分支时 HEAD 保存分支引用；切换提交时 HEAD 直接保存提交 ID，进入分离状态。
    pub fn checkout(&self, target: &str) -> Result<()> {
        let heads = self.meta.join("refs").join("heads");
        let branch_path = heads.join(target);

        // 当前提交的快照用于识别切换后需要删除的旧文件；恢复时会覆盖目标路径的内容。
        let current = self.head_tree()?;

        // 优先按分支名处理，从分支引用文件中读取它当前指向的提交 ID。
        if branch_path.exists() {
            let commit_id = fs::read_to_string(&branch_path)
                .with_context(|| format!("failed to read branch ref {}", branch_path.display()))?
                .trim()
                .to_string();
            if commit_id.is_empty() {
                bail!("branch `{}` has no commits", target);
            }
            let commit = self.read_commit(&commit_id)?;
            self.restore_tree(&current, &commit.tree)?;
            // HEAD 跟随该分支，后续提交会推进这个分支的引用。
            self.atomic_write(
                &self.meta.join("HEAD"),
                format!("ref: refs/heads/{}\n", target).as_bytes(),
            )?;
            // 清理上一次工作状态留下的暂存记录和冲突标记。
            self.write_index(&Index::default())?;
            self.clear_merge_conflicts()?;
            println!("switched to branch {}", target);
            return Ok(());
        }

        // 未找到同名分支时，尝试将 target 解析为提交 ID；无效目标会报错退出。
        // 这里定位的是用户指定的提交，并不是寻找某个分支的最新提交。
        let commit_id = self.resolve_commit(target)?;
        let commit = self.read_commit(&commit_id)?;
        self.restore_tree(&current, &commit.tree)?;
        // HEAD 直接指向提交，不关联分支，即 detached HEAD（分离 HEAD）状态。
        self.atomic_write(
            &self.meta.join("HEAD"),
            format!("{}\n", commit_id).as_bytes(),
        )?;
        // 与切换分支一样，清空暂存区和合并冲突状态。
        self.write_index(&Index::default())?;
        self.clear_merge_conflicts()?;
        println!("HEAD detached at {}", short_id(&commit_id));
        Ok(())
    }

    /// 合并策略：能快进则快进，否则执行三方合并。
    /// 把另一个分支的内容合并到当前分支，另一个分支保留，不会被删除
    pub fn merge_fast_forward_only(&self, branch: &str) -> Result<()> {
        crate::debug_log!("merge start branch={}", branch);
        let current = self
            .head_commit_id()?
            .ok_or_else(|| anyhow!("no current commit to merge into"))?;
        let target = self.resolve_commit(branch)?;
        //两份文本快照都一样，没有冲突
        if current == target {
            self.clear_merge_conflicts()?;
            println!("already up to date");
            return Ok(());
        }
        // 当前提交是目标提交的祖先，直接快进到目标提交。
        if self.is_ancestor(&current, &target)? {
            let current_tree = self.head_tree()?;
            let target_commit = self.read_commit(&target)?;
            self.restore_tree(&current_tree, &target_commit.tree)?;
            self.update_head_to_commit(&target)?;
            self.clear_merge_conflicts()?;
            crate::debug_log!("merge fast-forward target={}", target);
            println!("fast-forward merge to {}", short_id(&target));
            return Ok(());
        }//“分离”指的是 HEAD 不再通过分支名定位提交，而是直接定位某个提交
        // 目标提交是当前提交的祖先，说明当前分支已经包含了目标分支的所有内容。
        if self.is_ancestor(&target, &current)? {
            self.clear_merge_conflicts()?;
            println!("already up to date");
            return Ok(());
        }
        //双方都不是对方的祖先，历史已经分叉了
        let base = self
            .find_lca(&current, &target)?
            .ok_or_else(|| anyhow!("failed to find common ancestor"))?;//没找到共同祖先就出错
        let base_commit = self.read_commit(&base)?;
        let ours_commit = self.read_commit(&current)?;
        let theirs_commit = self.read_commit(&target)?;

        let (merged_tree, conflicts) =
            self.merge_trees_3way(&base_commit.tree, &ours_commit.tree, &theirs_commit.tree)?;
        crate::debug_log!(
            "merge 3-way base={} ours={} theirs={} conflicts_precheck={}",
            base,
            current,
            target,
            conflicts.len()
        );

        self.restore_tree(&ours_commit.tree, &merged_tree)?;
        //有冲突
        if !conflicts.is_empty() {
            self.write_merge_conflicts(&conflicts)?;
            println!("merge has conflicts; resolve then commit manually:");
            for p in conflicts {
                println!("  {}", p);
            }
            return Ok(());
        }
        //没有冲突
        let author = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        let merge_commit = Commit {
            tree: merged_tree,
            parents: vec![current.clone(), target.clone()],
            parent: None,
            message: format!("Merge branch {}", branch),
            author,
            timestamp: now_unix() as i64,
        };
        let raw = serde_json::to_vec(&merge_commit)?;
        // 合并提交为 Hot，使用快速压缩
        let new_id = self.write_object_with_temp(&raw, crate::util::BlobTemperature::Hot)?;
        self.update_head_to_commit(&new_id)?;
        self.write_index(&Index::default())?;
        self.clear_merge_conflicts()?;
        crate::debug_log!("merge commit id={}", new_id);
        println!(
            "merged {} into current branch as {}",
            branch,
            short_id(&new_id)
        );
        Ok(())
    }

    fn merge_trees_3way(
        &self,
        base: &BTreeMap<String, String>,
        ours: &BTreeMap<String, String>,
        theirs: &BTreeMap<String, String>,
    ) -> Result<(BTreeMap<String, String>, Vec<String>)> {
        let mut all_paths = BTreeSet::new();//收集所有的文件路径
        all_paths.extend(base.keys().cloned());
        all_paths.extend(ours.keys().cloned());
        all_paths.extend(theirs.keys().cloned());

        let mut out = BTreeMap::new();
        let mut conflicts = Vec::new();

        for p in all_paths {//对每个文件进行处理
            //找出三个提交里该文件的版本
            let b = base.get(&p);
            let o = ours.get(&p);
            let t = theirs.get(&p);

            // 当前版本和目标版本一样
            if o == t {
                if let Some(oid) = o {
                    out.insert(p, oid.clone());
                }
                continue;
            }
            //当前版本和共同祖先版本一样
            if b == o {
                if let Some(oid) = t {
                    out.insert(p, oid.clone());
                }
                continue;
            }
            //目标版本和共同祖先版本一样
            if b == t {
                if let Some(oid) = o {
                    out.insert(p, oid.clone());
                }
                continue;
            }
            //当前版本和目标版本都在共同祖先版本的基础上修改了，且不一样，产生冲突
            let merged = self.render_conflict_blob(o.cloned(), t.cloned())?;
            // 冲突标记内容为 Warm，平衡速度与压缩
            let oid = self.write_object_with_temp(&merged, crate::util::BlobTemperature::Warm)?;
            out.insert(p.clone(), oid);
            conflicts.push(p);
        }//到这里处理完毕后，每个文件对应的oid可能是找到了的最新版本，也可能是需要处理的冲突提示

        Ok((out, conflicts))
    }

    /// 生成供打印的冲突文本提示，返回字节数组
    fn render_conflict_blob(
        &self,
        ours: Option<String>, 
        theirs: Option<String>, 
    ) -> Result<Vec<u8>> {
        // 有对象 ID 就读取文件内容；没有文件就用空字节数组表示。? 会在读取失败时返回错误。
        let ours_bytes = match ours {
            Some(oid) => self.read_blob_resolved(&oid)?,
            None => Vec::new(),
        };
        let theirs_bytes = match theirs {
            Some(oid) => self.read_blob_resolved(&oid)?,
            None => Vec::new(),
        };

        // from_utf8 的作用就是确认读出来的字节可以当作这种文字使用：成功返回 Ok(&str)，失败返回 Err。
        // match 同时检查两个结果组成的元组，只有双方都成功才进入文本处理分支。
        match (
            std::str::from_utf8(&ours_bytes),
            std::str::from_utf8(&theirs_bytes),
        ) {
            (Ok(o), Ok(t)) => {
                let s = format!("<<<<<<< OURS\n{}\n=======\n{}\n>>>>>>> THEIRS\n", o, t);
                Ok(s.into_bytes())
            }
            // 只要有一方不是 UTF-8 文本，就直接返回二进制冲突标记，避免乱码
            _ => Ok(b"[binary conflict: OURS vs THEIRS]\n".to_vec()),
        }
    }

    /// 将工作区恢复为 `target_tree` 描述的文件快照。
    ///
    /// `current_tree` 和 `target_tree` 都是“文件路径 -> blob ID”的映射：
    /// blob ID 指向对象库中的文件内容。恢复时先删除目标快照中不存在的旧文件，
    /// 再读取目标 blob 并写回工作区。
    pub(super) fn restore_tree(
        &self,
        current_tree: &BTreeMap<String, String>,
        target_tree: &BTreeMap<String, String>,
    ) -> Result<()> {
        // 删除当前快照中有、目标快照中没有的文件。
        for p in current_tree.keys() {
            if !target_tree.contains_key(p) {
                let fp = self.root.join(p);
                if fp.exists() {
                    fs::remove_file(fp)?;
                }
            }
        }

        // 将目标快照中的每个 blob 读取出来，并写入对应路径。
        for (p, oid) in target_tree {
            let data = self
                .read_blob_resolved(oid)
                .with_context(|| format!("failed to read blob {} for path {}", oid, p))?;
            let fp = self.root.join(p);

            // 目标文件可能位于尚不存在的嵌套目录中，因此先创建父目录。
            if let Some(parent) = fp.parent() {
                fs::create_dir_all(parent)?;
            }

            // 使用原子写入，避免写入过程中出错时留下不完整的文件。
            self.atomic_write(&fp, &data)
                .with_context(|| format!("failed to restore file {}", fp.display()))?;
        }

        Ok(())
    }

    // 从 descendant 开始沿父提交往前找，判断能否到达 ancestor。
    // ancestor 是要查找的提交 ID，参数名不代表已经确认了祖先关系。
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        // 缓存不存在就读取提交记录并构建，存在则复用；? 表示失败时返回错误。
        self.get_or_build_commit_graph()?;
        // 获取缓存的锁，cg_guard 是允许访问内部 Option<CommitGraph> 的结构体。
        let cg_guard = self.commit_graph();
        // as_ref() 把 Option<CommitGraph> 借用为 Option<&CommitGraph>，不取走数据。
        // 有提交图时，将它的引用命名为 cg；为 None 时跳过这个代码块。
        if let Some(cg) = cg_guard.as_ref() {
            // 利用已缓存的代数快速判断：Some(false) 表示确定不是祖先，None 表示无法确定。
            if let Some(result) = cg.fast_ancestor_check(ancestor, descendant) {
                if !result {
                    // 提前返回时 cg_guard 也会自动销毁并释放锁。
                    return Ok(false);
                }
                // 如果返回 Some(true)，这里仍继续执行后面的遍历确认。
            }
        }
        // 提前销毁 cg_guard 并释放锁；后面的查找只需读取提交记录，无需继续占用缓存锁。
        drop(cg_guard);

        // Vec 存放待检查的提交 ID：push 放到末尾，pop 从末尾取出，后放入的先检查。
        // 最开始只检查 descendant 自己。
        let mut stack = vec![descendant.to_string()];
        // 记录已检查的提交，避免多个分支通向同一个父提交时重复处理。
        let mut seen = BTreeSet::new();
        // 每轮取出并移除一个 ID；Some(id) 时执行循环，列表空了返回 None，结束循环。
        while let Some(id) = stack.pop() {
            // clone() 复制 ID 字符串放入集合，保留 id 供下面使用。
            // insert() 遇到已有 ID 返回 false；取反后执行 continue，跳到下一轮。
            if !seen.insert(id.clone()) {
                continue;
            }
            // 找到了要查的提交，立即返回 true；遍历本身也把同一提交视为可到达。
            if id == ancestor {
                return Ok(true);
            }
            // 尚未找到：读取当前提交，把所有父提交加入待检查列表，继续向历史追溯。
            // read_commit 失败时，? 会立即把错误返回给调用者。
            let c = self.read_commit(&id)?;
            for p in Self::commit_parents(&c) {
                stack.push(p);
            }
        }
        // 所有能到达的提交都检查完，仍未找到目标。
        Ok(false)
    }

    /// 通过 BFS 计算祖先深度表
    fn ancestor_depths(&self, start: &str) -> Result<BTreeMap<String, usize>> {
        let mut out = BTreeMap::new();
        // 创建双端队列，每个元素是一对数据：(提交ID, 距离起点经过了几步)
        // push_back 从尾部加入，pop_front 从头部取出：先加入的先处理，按步数逐层查找
        let mut queue = std::collections::VecDeque::new();
        queue.push_back((start.to_string(), 0usize));
        while let Some((id, d)) = queue.pop_front() {
            if out.contains_key(&id) {
                continue;
            }
            out.insert(id.clone(), d);
            let c = self.read_commit(&id)?;
            for p in Self::commit_parents(&c) {
                queue.push_back((p, d + 1));
            }
        }
        Ok(out)
    }

    /// 选择合并深度最小的共同祖先作为 LCA
    ///Ok(Some(id)) 成功找到共同祖先，返回它的 ID
    ///Ok(None) 查找过程没有出错，但没找到共同祖先
    fn find_lca(&self, a: &str, b: &str) -> Result<Option<String>> {
        let da = self.ancestor_depths(a)?;
        let db = self.ancestor_depths(b)?;

        let mut best: Option<(String, usize)> = None;
        for (id, depth_a) in da {
            if let Some(depth_b) = db.get(&id) {
                let score = depth_a + *depth_b;
                match &best {
                    //先拆格式，然后检查条件
                    Some((_, best_score)) if score >= *best_score    => {}
                    _ => best = Some((id, score)),
                }
            }
        }

        Ok(best.map(|(id, _)| id))
    }

    /// 懒加载构建/复用 Commit-Graph（关联到当前 Repo 实例）。
    fn get_or_build_commit_graph(&self) -> Result<()> {
        // 锁住当前进程内存中的提交图缓存；其他线程若对同一个 Mutex 调用 lock()，会等待。
        // lock() 成功后，unwrap() 取出 MutexGuard 结构体，存入 cg_opt；它不复制或取走缓存。
        // cg_opt 可读取内部的 Option；通过 *cg_opt 可修改它，即使这里拿到的只是 &self。
        // cg_opt 离开作用域（包括提前 return 或 ? 返回）时，会自动释放锁。
        // 若其他线程持锁时发生 panic（执行被异常打断），锁可能被标记为中毒；
        // 此时 lock() 返回 Err，unwrap() 会触发 panic，打断当前线程执行。
        // 这个锁只协调内存访问；不同进程对磁盘仓库的访问由 with_repo_lock 的文件锁协调。
        let mut cg_opt = self.commit_graph.lock().unwrap();
        if cg_opt.is_some() {
            return Ok(());
        }
        crate::debug_log!("commit-graph: building...");
        let cg = CommitGraph::build(self)?;
        crate::debug_log!(
            "commit-graph: ready, {} commits indexed",
            cg.len()
        );
        *cg_opt = Some(cg);
        Ok(())
    }

    fn commit_graph(&self) -> std::sync::MutexGuard<'_, Option<CommitGraph>> {
        self.commit_graph.lock().unwrap()
    }
}
