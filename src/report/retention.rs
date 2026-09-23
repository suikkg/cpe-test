//! 历史目录的保留策略：`runs/` 不能无上限地长下去。
//!
//! # 为什么需要
//!
//! 一次 210 单元、11.5 小时的全量跑，带截图和逐样本 CSV，一个目录就是几百 MB、
//! 上千个文件。日更回归跑一年，`runs/` 会长到几十 GB，而 `/api/runs` 每次列表
//! 都要对每个目录递归算一次字节数——历史页会从「打开就有」变成「转好几秒」。
//!
//! 这是个运维问题不是缺陷，但对一个定位为长期回归的工具，它会慢慢变成缺陷。
//!
//! # 三条纪律
//!
//! 1. **默认不删**（`keep_runs: 0`）。删数据这件事必须由人显式选择——
//!    这个仓库对「在没人注意的情况下改变行为」有明确戒律，而没有什么比
//!    「升级一次，历史记录少了一半」更符合那个描述。
//! 2. **只删自己写出来的形状**：目录名必须匹配 `run_<数字与下划线>`，
//!    而且必须是**不跟随符号链接**的普通目录。`runs/x -> /etc` 名字也能命中，
//!    跟着走就是删别人的东西。
//! 3. **保留最新的 N 个**，按目录名排序——目录名带时间戳，字典序就是时间序，
//!    不必读文件系统时间（重放一次报告 mtime 就会变，按它排会把刚看过的
//!    那一份判成最新）。

use std::path::{Path, PathBuf};

/// 目录名必须长成 `run_20260910_073330_75043` 这样才考虑删。
///
/// 前缀之后只允许数字和下划线：这是 `create_run_paths` 生成的全部形状
/// （`run_<时间>_<进程号>` 加可选的 `_<序号>`）。人手放进 `runs/` 的任何
/// 别的东西——归档、备注、别人的目录——都不在删除范围里。
fn looks_like_a_run_directory(name: &str) -> bool {
    let Some(rest) = name.strip_prefix("run_") else {
        return false;
    };
    !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit() || b == b'_')
}

/// 挑出该删的那些目录，**不执行删除**。
///
/// 纯函数便于穷举：删数据的逻辑不该只能靠「跑一遍看少了什么」来验证。
/// 入参是目录名列表，返回要删的那些（最旧的在前）。
pub fn victims(mut names: Vec<String>, keep: usize) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    names.retain(|name| looks_like_a_run_directory(name));
    // 目录名带时间戳，字典序就是时间序。**不用 mtime**：重放一次报告它就会变，
    // 按它排会把刚刚看过的那一份判成最新的。
    names.sort();
    let over = names.len().saturating_sub(keep);
    names.into_iter().take(over).collect()
}

/// 扫 `runs/`，按 `keep` 删掉最旧的那些。返回被删掉的目录名。
///
/// `keep == 0` 时**一个都不删**并立刻返回——默认就是这个值。
///
/// 失败一律降级为跳过：清理是收尾动作，不该让一轮已经跑完的测试变成失败。
pub fn enforce(runs_dir: &Path, keep: usize) -> Vec<String> {
    if keep == 0 {
        return Vec::new();
    }
    let Ok(entries) = std::fs::read_dir(runs_dir) else {
        return Vec::new();
    };
    let names: Vec<String> = entries
        .flatten()
        .filter(|entry| {
            // `file_type()` 直接来自目录项且**不跟随链接**。名字对得上的
            // 符号链接照样能命中白名单，跟着走就是删链接指向的东西。
            entry.file_type().is_ok_and(|kind| kind.is_dir())
        })
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect();

    let mut removed = Vec::new();
    for name in victims(names, keep) {
        // 再确认一次这个名字在这里仍然是个普通目录：上一次枚举之后它可能
        // 已经被换成链接。这一步和 `master::webui::runs::resolve_run_dir`
        // 是同一条纪律。
        let path: PathBuf = runs_dir.join(&name);
        let is_plain_dir = std::fs::symlink_metadata(&path)
            .map(|meta| meta.file_type().is_dir())
            .unwrap_or(false);
        if !is_plain_dir {
            continue;
        }
        if std::fs::remove_dir_all(&path).is_ok() {
            removed.push(name);
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn zero_keeps_everything_because_deleting_data_needs_an_explicit_choice() {
        // 默认值就是 0。升级一次就悄悄删掉一半历史，是这条策略最该防的事。
        let all = names(&["run_1", "run_2", "run_3"]);
        assert!(victims(all, 0).is_empty());
    }

    #[test]
    fn the_newest_n_survive_and_the_oldest_go_first() {
        let all = names(&[
            "run_20260101_000000_1",
            "run_20260301_000000_3",
            "run_20260201_000000_2",
        ]);
        assert_eq!(
            victims(all, 2),
            names(&["run_20260101_000000_1"]),
            "只该删最旧的那一个"
        );
    }

    #[test]
    fn anything_that_is_not_our_own_directory_shape_is_never_touched() {
        // 人手放进 runs/ 的归档、备注、别人的目录都不在删除范围里。
        let mixed = names(&[
            "run_20260101_000000_1",
            "run_20260102_000000_2",
            "run_20260103_000000_3",
            "归档-验收用",
            "run_backup",
            ".DS_Store",
            "runs_old",
        ]);
        let gone = victims(mixed, 1);
        assert_eq!(
            gone,
            names(&["run_20260101_000000_1", "run_20260102_000000_2"])
        );
        assert!(
            !gone.iter().any(|name| name == "run_backup"),
            "`run_backup` 不是本工具生成的形状（含字母），不许删"
        );
    }

    #[test]
    fn keeping_more_than_exists_deletes_nothing() {
        let all = names(&["run_1", "run_2"]);
        assert!(victims(all, 10).is_empty());
    }

    #[test]
    fn enforce_removes_real_directories_and_leaves_foreign_names_alone() {
        let root = std::env::temp_dir().join(format!(
            "cpe_retention_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        for name in [
            "run_20260101_000000_1",
            "run_20260102_000000_2",
            "run_20260103_000000_3",
            "验收归档",
        ] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let removed = enforce(&root, 2);
        assert_eq!(removed, names(&["run_20260101_000000_1"]));
        assert!(!root.join("run_20260101_000000_1").exists());
        assert!(root.join("run_20260103_000000_3").exists());
        assert!(root.join("验收归档").exists(), "非本工具的目录一个都不许动");

        let _ = std::fs::remove_dir_all(&root);
    }
}
