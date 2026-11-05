use std::collections::{BTreeMap, BTreeSet};

use anyhow::Result;

use crate::repo::Repo;

pub struct CommitGraph {
    generations: BTreeMap<String, u64>,
}

impl CommitGraph {
    pub fn build(repo: &Repo) -> Result<Self> {
        let mut generations = BTreeMap::new();
        let mut visited = BTreeSet::new();

        // 收集所有引用头作为入口
        let mut roots = repo.all_ref_heads()?;
        if let Some(detached) = repo.head_commit_id()?
            && repo.current_ref()?.is_none()
        {
            roots.push(detached);
        }

        // 从每个入口做 DFS 后序遍历计算 generation number
        for root in &roots {
            Self::compute_gen(repo, root, &mut generations, &mut visited)?;
        }

        crate::debug_log!(
            "commit-graph built: {} commits indexed",
            generations.len()
        );
        Ok(Self { generations })
    }

    /// 递归计算 generation number（后序遍历，带记忆化）。
    fn compute_gen(
        repo: &Repo,
        commit_id: &str,
        generations: &mut BTreeMap<String, u64>,
        visited: &mut BTreeSet<String>,
    ) -> Result<u64> {
        // 已计算 → 直接返回
        if let Some(&generation) = generations.get(commit_id) {
            return Ok(generation);
        }

        // 检测环（防御性，正常提交图无环）
        if !visited.insert(commit_id.to_string()) {
            return Ok(0);
        }

        let commit = repo.read_commit(commit_id)?;
        let parents = Repo::commit_parents(&commit);

        let max_parent_gen = if parents.is_empty() {
            0 // 根提交 generation = 1（见下方 +1）
        } else {
            let mut max_gen = 0u64;
            for p in &parents {
                let pg = Self::compute_gen(repo, p, generations, visited)?;
                if pg > max_gen {
                    max_gen = pg;
                }
            }
            max_gen
        };

        let generation = max_parent_gen + 1;
        generations.insert(commit_id.to_string(), generation);
        Ok(generation)
    }

    pub fn generation(&self, commit_id: &str) -> Option<u64> {
        self.generations.get(commit_id).copied()
    }

    pub fn fast_ancestor_check(&self, ancestor: &str, descendant: &str) -> Option<bool> {
        let gen_a = self.generation(ancestor)?;
        let gen_d = self.generation(descendant)?;

        if gen_a > gen_d {
            return Some(false);
        }

        if gen_a == gen_d && ancestor == descendant {
            return Some(false);
        }

        None
    }

    /// 已索引的提交数。
    pub fn len(&self) -> usize {
        self.generations.len()
    }
}
