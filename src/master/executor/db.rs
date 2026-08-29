//! 断点续跑用的结果库。
//!
//! 只回答一个问题：这个单元先前跑过、结果还新鲜吗？

use super::*;

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

    /// 原子写（tmp + rename）
    pub fn save(&self) {
        let tmp = self.path.with_extension("tmp");
        if let Ok(text) = serde_json::to_string_pretty(&self.map) {
            if std::fs::write(&tmp, text).is_ok() {
                let _ = std::fs::rename(&tmp, &self.path);
            }
        }
    }
}
