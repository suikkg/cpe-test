//! 内环工作台：集合、共享流量配置、套件任务与分配。只在后端展开。
use super::config::{Direction, InnerConfig, ParameterOptions, Protocol, Thresholds};
use super::plan::{Plan, Unit};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Workspace {
    pub groups: Vec<LinkGroup>,
    pub recipes: Vec<Recipe>,
    pub suites: Vec<Suite>,
    pub bindings: Vec<Binding>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LinkGroup {
    pub id: String,
    pub name: String,
    pub links: Vec<String>,
    #[serde(default)]
    pub all_links: bool,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub parameters: ParameterOptions,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Suite {
    pub id: String,
    pub name: String,
    pub tasks: Vec<Task>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub id: String,
    pub name: String,
    pub protocol: Protocol,
    pub recipe: Option<String>,
    pub ip_versions: Vec<u8>,
    pub directions: Vec<Direction>,
    pub duration_secs: Option<u64>,
    pub repeats: Option<u32>,
    pub thresholds: Option<Thresholds>,
    pub ping_options: Option<super::config::PingOptions>,
    pub max_udp_loss_pct: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub group: String,
    pub suite: String,
    /// 空清单表示整集合，新增网口自动参与。
    pub links: Vec<String>,
}
#[derive(Debug, Clone, Serialize)]
pub struct Origin {
    pub group: String,
    pub suite: String,
    pub task: String,
}

fn unique_ids<'a>(items: impl Iterator<Item = (&'a str, &'a str)>) -> Result<(), String> {
    let mut seen = HashSet::new();
    for (id, name) in items {
        if id.is_empty() || id.len() > 128 || name.trim().is_empty() || !seen.insert(id) {
            return Err("集合、套件、任务和流量配置须有唯一标识和非空名称".into());
        }
    }
    Ok(())
}
impl Workspace {
    pub fn validate(&self, cfg: &InnerConfig) -> Result<(), String> {
        if self.groups.len() > 64
            || self.recipes.len() > 128
            || self.suites.len() > 64
            || self.bindings.len() > 256
            || self.suites.iter().any(|s| s.tasks.len() > 128)
        {
            return Err("工作台超过集合、配置、套件或任务数量上限".into());
        }
        unique_ids(self.groups.iter().map(|g| (g.id.as_str(), g.name.as_str())))?;
        unique_ids(
            self.recipes
                .iter()
                .map(|g| (g.id.as_str(), g.name.as_str())),
        )?;
        unique_ids(self.suites.iter().map(|g| (g.id.as_str(), g.name.as_str())))?;
        for suite in &self.suites {
            unique_ids(suite.tasks.iter().map(|t| (t.id.as_str(), t.name.as_str())))?;
        }
        for group in &self.groups {
            let mut seen = HashSet::new();
            for name in &group.links {
                if !cfg.links.iter().any(|l| l.name == *name) || !seen.insert(name) {
                    return Err(format!(
                        "集合 {} 引用了不存在或重复的网口 {name}",
                        group.name
                    ));
                }
            }
        }
        let mut baseline = cfg.clone();
        baseline.workspace = None;
        baseline.rounds = 1;
        baseline.directions = vec![Direction::Upload, Direction::Download, Direction::Bidir];
        for link in &mut baseline.links {
            link.enabled = false;
        }
        baseline.validate()?;
        for suite in &self.suites {
            for task in &suite.tasks {
                if let Some(t) = &task.thresholds {
                    if t.values()
                        .into_iter()
                        .flatten()
                        .any(|v| !v.is_finite() || v <= 0.0 || v > 1e6)
                    {
                        return Err("任务门限应留空或为正数 Mbps（上限 1000000）".into());
                    }
                    if !task.directions.contains(&Direction::Bidir)
                        && (t.bidir_total_min_mbps.is_some()
                            || t.tool_bidir_total_min_mbps.is_some())
                    {
                        return Err("单向任务不接受双向合计门限".into());
                    }
                }
                let mut checked = cfg.clone();
                for link in &mut checked.links {
                    link.enabled = false;
                }
                self.task_config(&checked, task)?.validate()?;
            }
        }
        let mut seen = HashSet::new();
        for binding in &self.bindings {
            let group = self
                .groups
                .iter()
                .find(|g| g.id == binding.group)
                .ok_or("分配引用的链路集合不存在")?;
            if !self.suites.iter().any(|s| s.id == binding.suite)
                || !seen.insert((&binding.group, &binding.suite))
            {
                return Err("分配引用的套件不存在或集合与套件重复分配".into());
            }
            let mut selected = HashSet::new();
            if binding.links.iter().any(|name| {
                !(group.links.contains(name)
                    || (group.all_links && cfg.links.iter().any(|l| l.name == *name)))
                    || !selected.insert(name)
            }) {
                return Err("分配中的网口必须属于对应集合且不得重复".into());
            }
        }
        Ok(())
    }
    fn task_config(&self, cfg: &InnerConfig, task: &Task) -> Result<InnerConfig, String> {
        let mut effective = cfg.clone();
        effective.workspace = None;
        effective.rounds = 1;
        effective.repeats = task.repeats.unwrap_or(1);
        effective.protocols = vec![task.protocol];
        effective.ip_versions = task.ip_versions.clone();
        effective.directions = task.directions.clone();
        effective.duration_secs = task.duration_secs.unwrap_or(cfg.duration_secs);
        effective.parameter_options = ParameterOptions::default();
        effective.tcp_window = None;
        effective.udp_mbps = None;
        effective.udp_length = None;
        effective.udp_window = None;
        if task.protocol != Protocol::Udp && task.max_udp_loss_pct.is_some() {
            return Err("UDP 丢包门槛只适用于 UDP 任务".into());
        }
        effective.max_udp_loss_pct = if task.protocol == Protocol::Udp {
            task.max_udp_loss_pct.or(cfg.max_udp_loss_pct)
        } else {
            None
        };
        if task.protocol == Protocol::Ping && (task.recipe.is_some() || task.thresholds.is_some()) {
            return Err("Ping 不引用流量配置".into());
        }
        if let Some(id) = &task.recipe {
            let recipe = self
                .recipes
                .iter()
                .find(|r| r.id == *id && r.protocol == task.protocol)
                .ok_or_else(|| format!("任务 {} 的流量配置不存在或协议不匹配", task.name))?;
            effective.parameter_options = recipe.parameters.clone();
        }
        if let Some(ping) = &task.ping_options {
            effective.ping_options = ping.clone();
        }
        for link in &mut effective.links {
            if !task.directions.contains(&Direction::Bidir) {
                link.bidir_total_min_mbps = None;
                link.tool_bidir_total_min_mbps = None;
                for t in [&mut link.tcp_thresholds, &mut link.udp_thresholds]
                    .into_iter()
                    .flatten()
                {
                    t.bidir_total_min_mbps = None;
                    t.tool_bidir_total_min_mbps = None;
                }
            }
            if let Some(t) = task.thresholds.as_ref().filter(|_| link.enabled) {
                let base = link.for_protocol(task.protocol);
                let merged = Thresholds {
                    upload_min_mbps: t.upload_min_mbps.or(base.upload_min_mbps),
                    download_min_mbps: t.download_min_mbps.or(base.download_min_mbps),
                    bidir_total_min_mbps: t.bidir_total_min_mbps.or(base.bidir_total_min_mbps),
                    tool_upload_min_mbps: link
                        .measurement
                        .uses_tool()
                        .then(|| t.tool_upload_min_mbps.or(base.tool_upload_min_mbps))
                        .flatten(),
                    tool_download_min_mbps: link
                        .measurement
                        .uses_tool()
                        .then(|| t.tool_download_min_mbps.or(base.tool_download_min_mbps))
                        .flatten(),
                    tool_bidir_total_min_mbps: link
                        .measurement
                        .uses_tool()
                        .then(|| {
                            t.tool_bidir_total_min_mbps
                                .or(base.tool_bidir_total_min_mbps)
                        })
                        .flatten(),
                };
                match task.protocol {
                    Protocol::Tcp => link.tcp_thresholds = Some(merged.clone()),
                    Protocol::Udp => link.udp_thresholds = Some(merged.clone()),
                    Protocol::Ping => return Err("Ping 不接受速率门限".into()),
                }
            }
        }
        Ok(effective)
    }
    pub fn build(&self, cfg: &InnerConfig) -> Result<Plan, String> {
        let mut units: Vec<Unit> = Vec::new();
        let mut estimated_secs = 0;
        let mut seen_units = HashSet::new();
        for round in 1..=cfg.rounds {
            for binding in &self.bindings {
                let group = self
                    .groups
                    .iter()
                    .find(|g| g.id == binding.group)
                    .ok_or("集合不存在")?;
                let suite = self
                    .suites
                    .iter()
                    .find(|s| s.id == binding.suite)
                    .ok_or("套件不存在")?;
                for task in &suite.tasks {
                    let mut selected = cfg.clone();
                    for link in &mut selected.links {
                        link.enabled = link.enabled
                            && (group.all_links || group.links.contains(&link.name))
                            && (binding.links.is_empty() || binding.links.contains(&link.name));
                    }
                    let effective = self.task_config(&selected, task)?;
                    let built = super::plan::build(&effective)?;
                    let effective = std::sync::Arc::new(effective);
                    for mut unit in built.units {
                        if round > 1 {
                            unit.id = crate::util::md5_hex(&format!("{}|round={round}", unit.id));
                        }
                        if !seen_units.insert(unit.id.clone()) {
                            continue;
                        }
                        unit.origin = Some(Origin {
                            group: group.name.clone(),
                            suite: suite.name.clone(),
                            task: task.name.clone(),
                        });
                        unit.round = round;
                        unit.index = units.len() + 1;
                        unit.execution_config = Some(std::sync::Arc::clone(&effective));
                        estimated_secs += super::plan::unit_estimated_secs(&effective, &unit);
                        units.push(unit);
                        if units.len() > 4096 {
                            return Err("内环计划最多 4096 个单元".into());
                        }
                    }
                }
            }
        }
        let links: Vec<_> = cfg
            .active_links()
            .into_iter()
            .filter(|i| units.iter().any(|u| u.link == *i))
            .collect();
        let agents = cfg
            .agents
            .iter()
            .filter(|a| links.iter().any(|i| cfg.links[*i].host == a.id))
            .map(|a| a.id.clone())
            .collect();
        Ok(Plan {
            uses_master: links.iter().any(|i| cfg.links[*i].host == "master"),
            units,
            links,
            agents,
            estimated_secs,
        })
    }
}
