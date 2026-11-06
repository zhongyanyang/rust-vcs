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
        //第二种情况下，提交 B 可能没有任何分支指向它，所以这里要额外加入
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

    /// 计算提交是第几代：没有父提交时为第 1 代，否则为父提交的最大代数加 1。
    /// 例如 A → B → C（按提交先后排列），计算 C 时先算 B，再算 A；
    /// 然后逐层返回，得到 A = 1、B = 2、C = 3，并把结果存入 generations。
    fn compute_gen(
        repo: &Repo, // 从这个仓库读取提交记录。
        commit_id: &str, // 当前要计算的提交 ID。
        generations: &mut BTreeMap<String, u64>, // 已算好的“提交 ID → 代数”表。
        visited: &mut BTreeSet<String>, // 已遇到过的提交 ID，不允许重复。
    ) -> Result<u64> {
        // 先查结果表：算过的提交直接返回，不再读取它的父提交。
        // get() 返回 Option<&u64>；Some(&generation) 匹配出引用，并复制里面的数字。
        if let Some(&generation) = generations.get(commit_id) {
            return Ok(generation);
        }

        // insert() 在首次加入时返回 true，ID 已存在时返回 false；! 表示取反。
        // 如果已遇到这个提交，却还没有算好结果，说明沿父提交绕回来了。
        // 正常提交历史不会有环；这里遇到异常数据时返回 0，避免无限调用自己。
        if !visited.insert(commit_id.to_string()) {
            return Ok(0);
        }

        // 读取当前提交，再从记录中取出父提交 ID；? 表示读取失败就返回错误。
        let commit = repo.read_commit(commit_id)?;
        let parents = Repo::commit_parents(&commit);

        let max_parent_gen = if parents.is_empty() {
            // 没有父提交时先取 0，下面统一加 1，得到第 1 代。
            0
        } else {
            let mut max_gen = 0u64;
            for p in &parents {
                // 用父提交 ID 再次调用本函数：先算出父提交的代数，才能算自己。
                // &mut 参数让这些调用共用并更新同一份结果表和已访问集合。
                let pg = Self::compute_gen(repo, p, generations, visited)?;
                // 合并提交可能有多个父提交，取其中最大的代数。
                if pg > max_gen {
                    max_gen = pg;
                }
            }
            max_gen
        };

        // 自己的代数比父提交的最大代数多 1；记录下来，供后续调用直接查表。
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
