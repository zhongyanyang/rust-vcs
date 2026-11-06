use super::*;
use crate::util::{
    BlobTemperature, advise_cold_blob, advise_hot_blob, categorize_blob_temperature, detect_fs_type,
    detect_numa_topology,
};

impl Repo {
    ///清理和压缩版本仓库，减少磁盘占用，不是回收程序内存
    pub fn gc(&self) -> Result<()> {
        crate::util::maybe_set_background_nice();
        crate::debug_log!("gc start");

        // 检测结果只是用于输出调试日志，没有直接调整性能参数
        if crate::util::should_enable_openeuler_io_hints() {
            let fs = detect_fs_type(&self.root);
            let numa = detect_numa_topology();
            crate::debug_log!(
                "gc env fs={} numa_nodes={} io_chunk={}",
                fs,
                numa.node_count,
                crate::util::io_chunk_size_bytes()
            );
        }
        //列出不能删除的对象，再尝试压缩它们
        let mut reachable = BTreeSet::new();
        for head in self.all_ref_heads()? {
            self.mark_from_commit(&head, &mut reachable)?;
        }
        //不要漏了游离head的情况
        if let Some(detached) = self.head_commit_id()?
            && self.current_ref()?.is_none()
        {
            self.mark_from_commit(&detached, &mut reachable)?;
        }
        //should_run_deep_delta_gc() 判断系统状态是否能执行差量优化；允许就优化名单中的文件内容，否则打印跳过信息
        let optimized = if crate::util::should_run_deep_delta_gc() {
            self.optimize_reachable_blobs_with_delta(&reachable)?
        } else {
            println!("gc: deep-delta optimization skipped by load-aware policy");
            0
        };
        //在 openEuler 上找出冷数据，向操作系统建议减少它们对内存缓存的占用
        if crate::util::should_enable_openeuler_io_hints() {
            let blob_latest_ts = self.collect_blob_latest_timestamps()?;
            let mut cold_hinted = 0usize;
            for (oid, ts) in blob_latest_ts {
                //筛选冷数据
                if categorize_blob_temperature(ts) != BlobTemperature::Cold {
                    continue;
                }
                //获取路径失败或打开失败，就跳过该对象
                let Ok(path) = self.object_path(&oid) else {
                    continue;
                };
                if let Ok(f) = File::open(&path) {
                    //发送缓存建议：调用 advise_cold_blob()，告诉内核这些数据暂时不太需要，可以减少缓存保留
                    advise_cold_blob(&f);
                    cold_hinted += 1;
                }
            }
            if cold_hinted > 0 {
                crate::debug_log!("gc cold-hinted {} reachable cold blob(s)", cold_hinted);
            }
        }
        //不在保留名单里的文件删掉
        let all = self.all_object_ids()?;
        let mut removed = 0usize;

        for oid in all {
            if !reachable.contains(&oid) {
                let p = self.object_path(&oid)?;
                if p.exists() {
                    fs::remove_file(&p)?;
                    removed += 1;
                }
            }
        }

        println!(
            "gc completed: optimized {} blob(s), removed {} unreachable object(s)",
            optimized, removed
        );
        crate::debug_log!(
            "gc done reachable={} optimized={} removed={}",
            reachable.len(),
            optimized,
            removed
        );
        Ok(())
    }

    ///是给仓库做完整性检查的命令，检查保存的提交历史和文件内容有没有缺失、损坏
    pub fn verify(&self) -> Result<()> {
        crate::util::maybe_set_background_nice();
        crate::debug_log!("verify start");
        let mut checked_commits = 0usize;
        let mut checked_blobs = 0usize;
        let mut reachable = BTreeSet::new();
        let mut blob_latest_ts = BTreeMap::new();
        //
        let mut commit_stack = self.all_ref_heads()?;
        if let Some(detached) = self.head_commit_id()?
            && self.current_ref()?.is_none()
        {
            commit_stack.push(detached);
        }
        //
        let mut seen_commits = BTreeSet::new();

        while let Some(id) = commit_stack.pop() {
            if !seen_commits.insert(id.clone()) {
                continue;
            }
            reachable.insert(id.clone());
            //检查该提交对象是否存在：重新计算它的哈希看是否和原来id一样
            //因为按 ID 找到文件，只能说明文件名对了，不能保证里面的内容是对的
            let raw = self.read_object(&id)?;
            if hex_sha256(&raw) != id {
                bail!("commit object hash mismatch: {}", id);
            }

            let c: Commit =
                serde_json::from_slice(&raw).with_context(|| format!("invalid commit {}", id))?;
            checked_commits += 1;
            //对每一份提交快照里的文件内容对象，加入到保留名单，并更新它的最新引用时间
            for oid in c.tree.values() {
                reachable.insert(oid.clone());
                checked_blobs += 1;//实际统计的是“遍历到的 blob 引用次数”
                //映射最初为空没问题：第一次遇到某个 ID 时插入，再次遇到时取出已有记录
                let entry = blob_latest_ts.entry(oid.clone()).or_insert(c.timestamp);
                if c.timestamp > *entry {
                    *entry = c.timestamp;
                }
            }

            for p in Self::commit_parents(&c) {
                commit_stack.push(p);
            }
        }

        for (oid, ts) in &blob_latest_ts {
            let bytes = self.read_blob_resolved(oid)?;
            if hex_sha256(&bytes) != *oid {
                bail!("blob hash mismatch: {}", oid);
            }

            if crate::util::should_enable_openeuler_io_hints() {
                let temp = categorize_blob_temperature(*ts);
                //哈希正确，跳过
                let Ok(path) = self.object_path(oid) else {
                    continue;
                };
                if let Ok(f) = File::open(&path) {
                    // 根据文件内容对象的冷热等级，向操作系统发送内存缓存建议。
                    match temp {
                        // 热数据：建议提前把文件数据加载到内存，方便后续读取。
                        BlobTemperature::Hot => advise_hot_blob(&f),
                        // 冷数据：建议减少缓存保留，让内存优先用于其他数据。
                        BlobTemperature::Cold => advise_cold_blob(&f),
                        // 温数据：不做任何操作。
                        BlobTemperature::Warm => {}
                    }
                }
            }
        }

        println!(
            "verify ok: {} commit(s), {} blob reference(s), {} reachable object(s)",
            checked_commits,
            checked_blobs,
            reachable.len()
        );

        crate::debug_log!(
            "verify done commits={} blobs={} reachable={}",
            checked_commits,
            checked_blobs,
            reachable.len()
        );
        Ok(())
    }

    /// 读取所有分支头指向的提交 ID。
    pub(super) fn all_ref_heads(&self) -> Result<Vec<String>> {
        let mut ids = Vec::new();
        let heads = self.meta.join("refs").join("heads");
        if !heads.exists() {
            return Ok(ids);
        }
        for e in fs::read_dir(heads)? {
            let e = e?;
            if e.path().is_file() {
                let id = fs::read_to_string(e.path())?.trim().to_string();
                if !id.is_empty() {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    }

    // 深度遍历提交图并标记其引用的 tree/blob 对象
    ///marked收集start及其祖先的提交id和文件快照对象id
    fn mark_from_commit(&self, start: &str, marked: &mut BTreeSet<String>) -> Result<()> {
        let mut stack = vec![start.to_string()];//栈结构
        while let Some(id) = stack.pop() {
            if !marked.insert(id.clone()) {
                continue;
            }
            let commit = self.read_commit(&id)?;
            for blob in commit.tree.values() {
                marked.insert(blob.clone());
            }
            for p in Self::commit_parents(&commit) {
                stack.push(p);
            }
        }
        Ok(())
    }

    ///记录的是引用该内容的最新提交时间
    fn collect_blob_latest_timestamps(&self) -> Result<BTreeMap<String, i64>> {
        let mut out = BTreeMap::new();
        let mut commit_stack = self.all_ref_heads()?;
        if let Some(detached) = self.head_commit_id()?
            && self.current_ref()?.is_none()
        {
            commit_stack.push(detached);
        }

        let mut seen_commits = BTreeSet::new();
        while let Some(id) = commit_stack.pop() {
            if !seen_commits.insert(id.clone()) {
                continue;
            }
            let commit = self.read_commit(&id)?;
            //记录：每个文件内容对象，最近一次出现在哪个时间的提交中
            for oid in commit.tree.values() {
                //在 out 中查找这个对象的记录
                //没有记录：先存入当前提交时间;已有记录：取到已保存的时间
                let entry = out.entry(oid.clone()).or_insert(commit.timestamp);
                if commit.timestamp > *entry {
                    *entry = commit.timestamp;
                }
            }
            for p in Self::commit_parents(&commit) {
                commit_stack.push(p);
            }
        }
        Ok(out)
    }

    /// 枚举对象库存储的全部对象 ID
    fn all_object_ids(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let base = self.objects_dir();
        if !base.exists() {
            return Ok(out);
        }

        for d in fs::read_dir(&base)? {
            let d = d?;
            let prefix = d.file_name().to_string_lossy().to_string();
            //如果外层遇到的条目不是目录，就跳过
            //因为这里采用的是“目录前缀 + 文件名”存储对象的方式
            if !d.path().is_dir() {
                continue;
            }
            for f in fs::read_dir(d.path())? {
                let f = f?;
                if f.path().is_file() {
                    let name = f.file_name().to_string_lossy().to_string();
                    out.push(format!("{}{}", prefix, name));
                }
            }
        }

        out.sort();
        Ok(out)
    }

    ///用差量的方式优化已保存的提交存储
    fn optimize_reachable_blobs_with_delta(&self, reachable: &BTreeSet<String>) -> Result<usize> {
        let mut optimized = 0usize;
        let mut done_targets = BTreeSet::new();//记录处理过的文件对象版本
        //构建“待检查提交列表”,这是一个栈，先压入最新的提交
        let mut commit_stack = self.all_ref_heads()?;
        if let Some(detached) = self.head_commit_id()?
            && self.current_ref()?.is_none()
        {
            commit_stack.push(detached);
        }
        //记录已经检查过的提交，避免重复处理
        let mut seen_commits = BTreeSet::new();
        while let Some(id) = commit_stack.pop() {
            //已经检查过，跳过
            if !seen_commits.insert(id.clone()) {
                continue;
            }

            let c = self.read_commit(&id)?;
            let parents = Self::commit_parents(&c);
            if let Some(parent_id) = parents.first() {
                let parent = self.read_commit(parent_id)?;
                //遍历快照里每个文件路径和对应的版本，并和旧提交快照进行比对，可增量优化的就优化
                for (path, target_oid) in &c.tree {
                    //目标对象不在保留名单里或目标对象已经处理过
                    if !reachable.contains(target_oid) || done_targets.contains(target_oid) {
                        continue;
                    }
                    //找不到旧版本：跳过
                    let Some(base_oid) = parent.tree.get(path) else {
                        continue;
                    };
                    //新旧内容一样：跳过
                    if base_oid == target_oid {
                        continue;
                    }
                    //旧版本不在保留名单里：跳过
                    //因为差量存储依赖旧版本才能还原新版本
                    //如果把新版本存成“旧版本 + 差异”，但旧版本不在保留名单中，后续 GC 就可能把旧版本删掉
                    if !reachable.contains(base_oid) {
                        continue;
                    }

                    let current_raw = self.read_object(target_oid)?;
                    //当前内容已经是差量格式：跳过
                    if decode_delta_blob(&current_raw)?.is_some() {
                        done_targets.insert(target_oid.clone());
                        continue;
                    }

                    let base = self.read_blob_resolved(base_oid)?;
                    let target = current_raw;
                    //建立增量指令并返回建立结果
                    let ops = build_delta(&base, &target);
                    if ops.is_empty() {
                        done_targets.insert(target_oid.clone());
                        continue;
                    }
                    //开始做存储增量准备
                    let delta_blob = DeltaBlob {
                        base: base_oid.clone(),
                        ops,
                        size: target.len(),
                    };
                    let encoded = encode_delta_blob(&delta_blob)?;

                    let compressed_old = compress_bytes(&target)?;
                    let compressed_new = compress_bytes(&encoded)?;
                    //新版比旧版小不超过 16 字节：跳过
                    if compressed_new.len() + 16 >= compressed_old.len() {
                        done_targets.insert(target_oid.clone());
                        continue;
                    }
                    //把原来id对应的文件内容替换成增量格式
                    self.write_object_for_existing_oid(target_oid, &encoded)?;
                    optimized += 1;
                    done_targets.insert(target_oid.clone());
                }
            }

            for p in parents {
                commit_stack.push(p);
            }
        }

        Ok(optimized)//返回优化个数
    }
}
