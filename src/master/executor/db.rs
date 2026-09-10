//! 断点续跑用的结果库。
//!
//! 只回答一个问题：这个单元先前跑过、结果还新鲜吗？

use super::*;
use std::io::Write;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct DbEnt {
    pub ok: bool,
    pub time: String,
    pub title: String,
}

pub struct ResultDb {
    pub(super) path: PathBuf,
    pub(super) map: HashMap<String, DbEnt>,
}

pub const RESUME_MAX_AGE_HOURS: i64 = 24;

pub(super) fn resume_age_is_fresh(age: chrono::Duration) -> bool {
    age >= chrono::Duration::seconds(-60) && age < chrono::Duration::hours(RESUME_MAX_AGE_HOURS)
}

impl ResultDb {
    pub fn load(path: PathBuf) -> Self {
        let map = std::fs::read_to_string(&path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_default();
        ResultDb { path, map }
    }

    /// 24 小时内 PASS 过则返回该次时间
    pub fn fresh_pass(&self, id: &str) -> Option<String> {
        let e = self.map.get(id)?;
        if !e.ok {
            return None;
        }
        let t = chrono::NaiveDateTime::parse_from_str(&e.time, "%Y-%m-%d %H:%M:%S").ok()?;
        let now = chrono::Local::now().naive_local();
        let age = now.signed_duration_since(t);
        if resume_age_is_fresh(age) {
            Some(e.time.clone())
        } else {
            None
        }
    }

    pub fn set(&mut self, id: &str, ok: bool, title: &str) {
        self.map.insert(
            id.to_string(),
            DbEnt {
                ok,
                time: now_full(),
                title: title.to_string(),
            },
        );
    }

    /// 原子写（tmp + rename）。
    ///
    /// tmp 用 `create_new` 打开：目标名若被预置成符号链接就直接失败，不跟着写到目录外。
    /// 覆盖用 `std::fs::rename` 就够——Windows 上 std 内部走的就是
    /// `MoveFileExW(.., MOVEFILE_REPLACE_EXISTING)`，并且在 `ERROR_ACCESS_DENIED` 时
    /// 还会用 `FileRenameInfoEx` 兜住只读属性。**不要在这里手写 `MoveFileExW`**：
    /// 那样既不会多出覆盖语义，还会把 std 的只读兜底丢掉。
    pub fn save(&self) {
        let tmp = self.path.with_extension("tmp");
        if let Ok(text) = serde_json::to_string_pretty(&self.map) {
            let _ = std::fs::remove_file(&tmp);
            let result = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&tmp)
                .and_then(|mut file| file.write_all(text.as_bytes()))
                .and_then(|_| std::fs::rename(&tmp, &self.path));
            if result.is_err() {
                let _ = std::fs::remove_file(&tmp);
            }
        }
    }
}
