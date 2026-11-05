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

    // 合并策略：能快进则快进，否则执行三方合并。
    pub fn merge_fast_forward_only(&self, branch: &str) -> Result<()> {
        crate::debug_log!("merge start branch={}", branch);
        let current = self
            .head_commit_id()?
            .ok_or_else(|| anyhow!("no current commit to merge into"))?;
        let target = self.resolve_commit(branch)?;

        if current == target {
            self.clear_merge_conflicts()?;
            println!("already up to date");
            return Ok(());
        }

        if self.is_ancestor(&current, &target)? {
            let current_tree = self.head_tree()?;
            let target_commit = self.read_commit(&target)?;
            self.restore_tree(&current_tree, &target_commit.tree)?;
            self.update_head_to_commit(&target)?;
            self.clear_merge_conflicts()?;
            crate::debug_log!("merge fast-forward target={}", target);
            println!("fast-forward merge to {}", short_id(&target));
            return Ok(());
        }

        if self.is_ancestor(&target, &current)? {
            self.clear_merge_conflicts()?;
            println!("already up to date");
            return Ok(());
        }

        let base = self
            .find_lca(&current, &target)?
            .ok_or_else(|| anyhow!("failed to find common ancestor"))?;
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

        if !conflicts.is_empty() {
            self.write_merge_conflicts(&conflicts)?;
            println!("merge has conflicts; resolve then commit manually:");
            for p in conflicts {
                println!("  {}", p);
            }
            return Ok(());
        }

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
        let mut all_paths = BTreeSet::new();
        all_paths.extend(base.keys().cloned());
        all_paths.extend(ours.keys().cloned());
        all_paths.extend(theirs.keys().cloned());

        let mut out = BTreeMap::new();
        let mut conflicts = Vec::new();

        for p in all_paths {
            let b = base.get(&p);
            let o = ours.get(&p);
            let t = theirs.get(&p);

            // 经典三方合并规则；无法自动归并时写入冲突内容。
            if o == t {
                if let Some(oid) = o {
                    out.insert(p, oid.clone());
                }
                continue;
            }

            if b == o {
                if let Some(oid) = t {
                    out.insert(p, oid.clone());
                }
                continue;
            }

            if b == t {
                if let Some(oid) = o {
                    out.insert(p, oid.clone());
                }
                continue;
            }

            let merged = self.render_conflict_blob(o.cloned(), t.cloned())?;
            // 冲突标记内容为 Warm，平衡速度与压缩
            let oid = self.write_object_with_temp(&merged, crate::util::BlobTemperature::Warm)?;
            out.insert(p.clone(), oid);
            conflicts.push(p);
        }

        Ok((out, conflicts))
    }

    fn render_conflict_blob(
        &self,
        ours: Option<String>,
        theirs: Option<String>,
    ) -> Result<Vec<u8>> {
        let ours_bytes = match ours {
            Some(oid) => self.read_blob_resolved(&oid)?,
            None => Vec::new(),
        };
        let theirs_bytes = match theirs {
            Some(oid) => self.read_blob_resolved(&oid)?,
            None => Vec::new(),
        };

        match (
            std::str::from_utf8(&ours_bytes),
            std::str::from_utf8(&theirs_bytes),
        ) {
            (Ok(o), Ok(t)) => {
                let s = format!("<<<<<<< OURS\n{}\n=======\n{}\n>>>>>>> THEIRS\n", o, t);
                Ok(s.into_bytes())
            }
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

    // 遍历提交图判断祖先关系（Commit-Graph 加速）。
    fn is_ancestor(&self, ancestor: &str, descendant: &str) -> Result<bool> {
        // 尝试构建/复用 Commit-Graph 做 O(1) 剪枝
        self.get_or_build_commit_graph()?;
        let cg_guard = self.commit_graph();
        if let Some(cg) = cg_guard.as_ref() {
            if let Some(result) = cg.fast_ancestor_check(ancestor, descendant) {
                if !result {
                    return Ok(false);
                }
            }
        }
        drop(cg_guard); // 释放锁，避免递归死锁

        let mut stack = vec![descendant.to_string()];
        let mut seen = BTreeSet::new();
        while let Some(id) = stack.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if id == ancestor {
                return Ok(true);
            }
            let c = self.read_commit(&id)?;
            for p in Self::commit_parents(&c) {
                stack.push(p);
            }
        }
        Ok(false)
    }

    // 通过 BFS 计算祖先深度表，用于 LCA 评分。
    fn ancestor_depths(&self, start: &str) -> Result<BTreeMap<String, usize>> {
        let mut out = BTreeMap::new();
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

    // 选择合并深度最小的共同祖先作为 LCA。
    fn find_lca(&self, a: &str, b: &str) -> Result<Option<String>> {
        let da = self.ancestor_depths(a)?;
        let db = self.ancestor_depths(b)?;

        let mut best: Option<(String, usize)> = None;
        for (id, depth_a) in da {
            if let Some(depth_b) = db.get(&id) {
                let score = depth_a + *depth_b;
                match &best {
                    Some((_, best_score)) if score >= *best_score => {}
                    _ => best = Some((id, score)),
                }
            }
        }

        Ok(best.map(|(id, _)| id))
    }

    /// 懒加载构建/复用 Commit-Graph（关联到当前 Repo 实例）。
    fn get_or_build_commit_graph(&self) -> Result<()> {
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
