# CLAUDE.md

本仓库的协作规范、门禁命令和不可破坏的不变量**全部**写在 `AGENTS.md` 与
`.ai/PROJECT_ARCHITECTURE.md` 里，本文不复制它们，只负责把它们接进上下文。
**动手改代码之前先读这两份。**

@AGENTS.md
@.ai/PROJECT_ARCHITECTURE.md

---

## 审查这个仓库时额外注意

1. **`git diff` 不等于改动面。** 新特性经常以未 `git add` 的文件形式停在工作树里
   （内环 v2 一度有近万行处于 untracked）。做 review 前先跑 `git status --short`，
   对 `??` 的文件执行 `git add -N` 再审，否则 `/code-review` 这类锚定 diff 的工具
   会直接看不见它们。

2. **缺陷经常在「本次没改过」的老代码里。** 只读 diff 会漏掉这一整类。

3. **四条门禁全绿不是「没缺陷」的证据。** 历史上 749→764 个测试全程全绿的同时，
   仍在持续查出会导致静默错判和丢数据的缺陷。绿灯只说明没退回去。

4. **每条修复的前提要独立证伪一次。** 反例：曾经有人以「Windows 的 `std::fs::rename`
   不保证覆盖目标」为由手写 `MoveFileExW`，实际 std 内部就是
   `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)` 且多一条只读属性兜底——
   那次「修复」是净回退。见 `src/master/executor/db.rs::save` 上的注释。

5. **发现要按威胁模型分级。** 本地 `runs/`、`inner_runs/` 目录的符号链接 TOCTOU
   需要攻击者已有本机写权限，那时他换掉 exe 更省事；这类记 backlog，不占主线改动面。
