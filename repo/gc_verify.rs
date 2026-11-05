use super::*;
use crate::util::{
    BlobTemperature, advise_cold_blob, advise_hot_blob, categorize_blob_temperature, detect_fs_type,
    detect_numa_topology,
};

impl Repo {
    // 标记可达对象，优化部分 blob，然后清理不可达对象。
    pub fn gc(&self) -> Result<()> {
        crate::util::maybe_set_background_nice();
        crate::debug_log!("gc start");

        // openEuler 优化：采集 FS/NUMA 信息用于调优决策
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

        let mut reachable = BTreeSet::new();
        for head in self.all_ref_heads()? {
            self.mark_from_commit(&head, &mut reachable)?;
        }

        if let Some(detached) = self.head_commit_id()?
            && self.current_ref()?.is_none()
        {
            self.mark_from_commit(&detached, &mut reachable)?;
        }

        let optimized = if crate::util::should_run_deep_delta_gc() {
            self.optimize_reachable_blobs_with_delta(&reachable)?
        } else {
            println!("gc: deep-delta optimization skipped by load-aware policy");
            0
        };

        if crate::util::should_enable_openeuler_io_hints() {
            let blob_latest_ts = self.collect_blob_latest_timestamps()?;
            let mut cold_hinted = 0usize;
            for (oid, ts) in blob_latest_ts {
                if categorize_blob_temperature(ts) != BlobTemperature::Cold {
                    continue;
                }
                let Ok(path) = self.object_path(&oid) else {
                    continue;
                };
                if let Ok(f) = File::open(&path) {
                    advise_cold_blob(&f);
                    cold_hinted += 1;
                }
            }
            if cold_hinted > 0 {
                crate::debug_log!("gc cold-hinted {} reachable cold blob(s)", cold_hinted);
            }
        }

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

    // 校验提交/对象完整性与可遍历性。
    pub fn verify(&self) -> Result<()> {
        crate::util::maybe_set_background_nice();
        crate::debug_log!("verify start");
        let mut checked_commits = 0usize;
        let mut checked_blobs = 0usize;
        let mut reachable = BTreeSet::new();
        let mut blob_latest_ts = BTreeMap::new();

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
            reachable.insert(id.clone());

            let raw = self.read_object(&id)?;
            if hex_sha256(&raw) != id {
                bail!("commit object hash mismatch: {}", id);
            }

            let c: Commit =
                serde_json::from_slice(&raw).with_context(|| format!("invalid commit {}", id))?;
            checked_commits += 1;

            for oid in c.tree.values() {
                reachable.insert(oid.clone());
                checked_blobs += 1;
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
                let Ok(path) = self.object_path(oid) else {
                    continue;
                };
                if let Ok(f) = File::open(&path) {
                    match temp {
                        BlobTemperature::Hot => advise_hot_blob(&f),
                        BlobTemperature::Cold => advise_cold_blob(&f),
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

    // 读取所有分支头指向的提交 ID。
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

    // 深度遍历提交图并标记其引用的 tree/blob 对象。
    fn mark_from_commit(&self, start: &str, marked: &mut BTreeSet<String>) -> Result<()> {
        let mut stack = vec![start.to_string()];
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

    // 遍历可达提交，收集 blob 的最新提交时间戳。
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
            for oid in commit.tree.values() {
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

    // 枚举对象库存储的全部对象 ID。
    fn all_object_ids(&self) -> Result<Vec<String>> {
        let mut out = Vec::new();
        let base = self.objects_dir();
        if !base.exists() {
            return Ok(out);
        }

        for d in fs::read_dir(&base)? {
            let d = d?;
            let prefix = d.file_name().to_string_lossy().to_string();
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

    // 当压缩后更小且收益足够时，将可达 blob 重编码为 delta。
    fn optimize_reachable_blobs_with_delta(&self, reachable: &BTreeSet<String>) -> Result<usize> {
        let mut optimized = 0usize;
        let mut done_targets = BTreeSet::new();

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

            let c = self.read_commit(&id)?;
            let parents = Self::commit_parents(&c);
            if let Some(parent_id) = parents.first() {
                let parent = self.read_commit(parent_id)?;
                for (path, target_oid) in &c.tree {
                    if !reachable.contains(target_oid) || done_targets.contains(target_oid) {
                        continue;
                    }
                    let Some(base_oid) = parent.tree.get(path) else {
                        continue;
                    };
                    if base_oid == target_oid {
                        continue;
                    }
                    if !reachable.contains(base_oid) {
                        continue;
                    }

                    let current_raw = self.read_object(target_oid)?;
                    if decode_delta_blob(&current_raw)?.is_some() {
                        done_targets.insert(target_oid.clone());
                        continue;
                    }

                    let base = self.read_blob_resolved(base_oid)?;
                    let target = current_raw;
                    let ops = build_delta(&base, &target);
                    if ops.is_empty() {
                        done_targets.insert(target_oid.clone());
                        continue;
                    }

                    let delta_blob = DeltaBlob {
                        base: base_oid.clone(),
                        ops,
                        size: target.len(),
                    };
                    let encoded = encode_delta_blob(&delta_blob)?;

                    let compressed_old = compress_bytes(&target)?;
                    let compressed_new = compress_bytes(&encoded)?;
                    if compressed_new.len() + 16 >= compressed_old.len() {
                        done_targets.insert(target_oid.clone());
                        continue;
                    }

                    self.write_object_for_existing_oid(target_oid, &encoded)?;
                    optimized += 1;
                    done_targets.insert(target_oid.clone());
                }
            }

            for p in parents {
                commit_stack.push(p);
            }
        }

        Ok(optimized)
    }
}
