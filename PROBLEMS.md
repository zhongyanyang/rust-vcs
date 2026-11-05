# Problems

## 仓库初始化可能覆盖残缺仓库的分支引用

- **位置**：`repo/core.rs` 的仓库初始化逻辑。
- **状态**：待确认、暂不修改。

### 问题描述

当前代码只有在 `HEAD`、`refs/heads/main` 和 `index.json` 三个文件全部存在时，才认为仓库已经初始化：

```rust
let already_initialized =
    head_file.is_file() && main_ref.is_file() && index_file.is_file();
```

随后，程序会补建缺失的目录和文件。如果仓库只缺少其中一个关键文件，`already_initialized` 会是 `false`，程序会进入首次初始化分支，创建新的初始提交，并重新写入 `refs/heads/main`。

### 潜在风险

一个已经包含对象或提交、但关键文件不完整的仓库，可能被误判为新仓库。重新写入 `refs/heads/main` 后，原分支引用可能丢失，导致已有提交无法通过正常分支引用访问。

### 示例

如果以下文件存在：

- `.myvcs/HEAD`
- `.myvcs/refs/heads/main`

但 `.myvcs/index.json` 缺失，程序会先创建空的 `index.json`，然后仍进入首次初始化分支，并用新初始提交的 ID 覆盖 `main`。

### 后续修改建议

将初始化状态明确区分为三种：

1. 所有关键文件都不存在：执行首次初始化。
2. 所有关键文件都存在：报告仓库已经初始化。
3. 只有部分关键文件存在：报告仓库结构不完整，不自动覆盖或修复分支引用。

修改前应增加测试，覆盖首次初始化、重复初始化和残缺仓库三种情况。
