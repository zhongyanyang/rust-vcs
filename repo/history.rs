use super::*;

impl Repo {
    /// 在当前提交的文件快照上应用暂存区修改，创建并保存新提交。
    /// 旧提交保持不变；新提交只记录直接父提交 ID，祖先通过父提交逐级查找。
    /// 保存成功后，将当前分支（分离 HEAD 时为 HEAD 本身）指向新提交。
    pub fn commit(&self, message: &str) -> Result<()> {
        crate::debug_log!("commit start message={}", message);
        let index = self.read_index()?;
        if index.entries.is_empty() && index.removed.is_empty() {
            bail!("nothing to commit (index is empty)")
        }

        // 读取旧快照得到内存中的独立数据，修改 tree 不会改写旧提交。
        // 暂存区只记录变更，因此从旧快照出发，才能保留未修改文件。
        // 先应用删除，再应用新增/修改。
        let mut tree = self.head_tree()?;
        for p in &index.removed {
            tree.remove(p);
        }
        for (p, oid) in index.entries {
            tree.insert(p, oid);
        }

        let parent = self.head_commit_id()?;
        let author = std::env::var("USER").unwrap_or_else(|_| "unknown".to_string());
        let timestamp = now_unix() as i64;
        let commit = Commit {
            tree,
            // Option 转成迭代器后有 0 或 1 个元素：首次提交为 []，普通提交为 [父提交 ID]。
            // 这里只记录直接父提交，不复制其祖先列表；合并提交由合并流程另行创建。
            parents: parent.clone().into_iter().collect(),
            // 兼容旧格式的单父提交字段；新提交统一使用上面的 parents。
            parent: None,
            message: message.to_string(),
            author,
            timestamp,
        };

        let raw = serde_json::to_vec(&commit)?;
        // 新建提交对象为 Hot，使用快速压缩
        let id = self.write_object_with_temp(&raw, crate::util::BlobTemperature::Hot)?;
        self.update_head_to_commit(&id)?;
        self.write_index(&Index::default())?;
        self.clear_merge_conflicts()?;
        crate::debug_log!("commit done id={}", id);
        println!("[{}] {}", short_id(&id), message);
        Ok(())
    }

    /// 从 HEAD 向后打印提交链。
    pub fn log(&self) -> Result<()> {
        let mut cur = self.head_commit_id()?;
        if cur.is_none() {
            println!("no commits yet");
            return Ok(());
        }

        while let Some(id) = cur {
            let c = self.read_commit(&id)?;
            println!("commit {}", id);
            println!("Author: {}", c.author);
            println!("Date:   {}", c.timestamp);
            println!();
            println!("    {}", c.message);
            println!();
            cur = Self::commit_parents(&c).into_iter().next();
        }

        Ok(())
    }

    // 将工作区恢复到目标提交，并移动 HEAD。
    pub fn reset_hard(&self, commit: &str) -> Result<()> {
        let current = self.head_tree()?;
        let target = self.read_commit(commit)?;
        self.restore_tree(&current, &target.tree)?;
        self.update_head_to_commit(commit)?;
        self.write_index(&Index::default())?;
        self.clear_merge_conflicts()?;
        println!("HEAD is now at {}", short_id(commit));
        Ok(())
    }
}
