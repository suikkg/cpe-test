//! **不可变执行计划**：从这里往后，跑的就是这一份，不再有第二次推导。
//!
//! 这个模块存在的理由是一个具体的缺口。控制台此前的流程是：
//!
//! ```text
//! 界面勾选 → compile_request → units（展示给用户复核）+ cfg
//!                                  ↓ 丢弃
//!                            cfg → 临时 json → run_master → build_units（第二次）
//! ```
//!
//! 用户在复核页上确认的那批单元被丢掉了，真正执行的是**从配置重新推导**出来
//! 的另一批。两次推导之间隔着一次 JSON 序列化和两处独立的代码路径；只要
//! 其中任何一处有损或有分叉，复核页就是在撒谎——而且撒得毫无痕迹，报告上
//! 一切正常，只是跑的不是你确认的东西。
//!
//! `plan_hash` 因此**必须算在单元本身上**，不能算在请求报文上：请求报文相同
//! 不代表推导出的单元相同，而使用者关心的从来是后者。带着这个哈希走完
//! 「预览 → 确认 → 执行」，执行端在真正开跑前再算一次、对不上就拒绝跑。

use crate::config::Config;
use crate::master::builder::Unit;
use crate::protocol::HostInfo;
use crate::util::md5_hex;

/// 计划格式版本。参与哈希，避免跨版本的哈希被误认为可比。
pub const EXECUTION_PLAN_VERSION: u32 = 1;

/// 一次运行**确定要执行的全部内容**。
///
/// 构造出来之后不再改动：字段都是只读的，没有任何 `&mut self` 方法。想改
/// 计划只能重新构造一份，那样哈希会变，确认流程也就会重新走一遍。
#[derive(Debug, Clone)]
pub struct ExecutionPlan {
    pub version: u32,
    pub created_at: String,
    /// 计划成型时的网口拓扑指纹。**只作诊断，不进 `plan_hash`**。
    ///
    /// 拓扑真正影响到执行内容时，单元本身就会变，`plan_hash` 自然跟着变。
    /// 反过来，重扫网卡时协商速率抖一下、link-local 地址换一个，拓扑指纹会
    /// 变而单元一模一样——把它算进闸门，只会在什么都没变的情况下拦下运行。
    /// 拓扑漂移另有 `NicDrift` 专门报告，不需要闸门再兼一份。
    pub topology_hash: String,
    /// 计划成型时的配置指纹。
    pub config_hash: String,
    /// **执行内容**的指纹：由上面三者加单元指纹算出。
    pub plan_hash: String,
    units: Vec<Unit>,
    notices: Vec<String>,
}

impl ExecutionPlan {
    pub fn new(
        cfg: &Config,
        topology_hash: impl Into<String>,
        units: Vec<Unit>,
        notices: Vec<String>,
    ) -> Self {
        let topology_hash = topology_hash.into();
        let config_hash = config_fingerprint(cfg);
        let plan_hash = md5_hex(&format!(
            "cpe-plan-v{EXECUTION_PLAN_VERSION}|{config_hash}|{}",
            units_fingerprint(&units)
        ));
        Self {
            version: EXECUTION_PLAN_VERSION,
            created_at: crate::util::now_full(),
            topology_hash,
            config_hash,
            plan_hash,
            units,
            notices,
        }
    }

    pub fn units(&self) -> &[Unit] {
        &self.units
    }

    pub fn notices(&self) -> &[String] {
        &self.notices
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty()
    }

    /// 执行前的最后一道闸：这份计划是不是当初确认的那一份。
    ///
    /// `expected` 为空表示调用方没有走过确认流程（命令行直跑），不设闸。
    pub fn matches(&self, expected: Option<&str>) -> bool {
        match expected.map(str::trim).filter(|value| !value.is_empty()) {
            Some(expected) => expected == self.plan_hash,
            None => true,
        }
    }
}

/// 配置的指纹——**只算「跑什么、怎么判」，不算「怎么调起来的」**。
///
/// 有几个字段是由调用方式决定的，不是由计划决定的：命令行的
/// `--no-open/--resume/--screenshot/--agent-host/--token/--prefix`，以及控制台
/// 固定传的 `no_open: true`。`run_master` 会在加载配置之后按 `MasterOpts`
/// 把它们盖掉。
///
/// 不归一就会出事，而且是致命的那种：控制台编译计划时 `open_report` 还是
/// 默认的 true，写进临时配置；`run_master` 拿着 `no_open: true` 把它改成
/// false——配置一变哈希就变，闸门于是把**每一次**控制台运行都判成「计划已
/// 过期」。功能一切正常，全被自己的闸门挡死。
fn canonical_for_fingerprint(cfg: &Config) -> Config {
    let mut cfg = cfg.clone();
    cfg.open_report = false;
    cfg.resume = false;
    cfg.screenshot = false;
    cfg.agent_host = String::new();
    cfg.agent_port = 0;
    cfg.agent_token = String::new();
    cfg.ipv4_prefixes = Vec::new();
    cfg
}

/// 配置的指纹。
pub fn config_fingerprint(cfg: &Config) -> String {
    md5_hex(&serde_json::to_string(&canonical_for_fingerprint(cfg)).unwrap_or_default())
}

/// 单元的指纹——**这次到底要跑什么**。
///
/// 用 `Debug` 而不是 `Serialize`：`Unit` 及其下的任务结构没有也不需要 serde
/// 派生，而这个哈希从不需要跨版本或跨进程稳定——预览和执行发生在同一个进程、
/// 同一个二进制里，它要回答的只是「这两次推导出来的是不是同一批单元」。
/// 把它当成可持久化的标识去用是错的，所以这里不提供任何存盘路径。
pub fn units_fingerprint(units: &[Unit]) -> String {
    let mut buf = String::new();
    for unit in units {
        buf.push_str(&format!("{unit:?}\n"));
    }
    md5_hex(&buf)
}

/// 网口拓扑的指纹。
///
/// 计划是按「当时看到的网口」推导出来的：IP 变了、网卡没了，同一份配置会
/// 推出不同的单元。把拓扑并进计划哈希，确认过的计划就不会在拓扑变化之后
/// 被静默地当成还有效。
pub fn topology_fingerprint(master: &HostInfo, agent: &HostInfo) -> String {
    let value = serde_json::json!({ "master": master, "agent": agent });
    md5_hex(&serde_json::to_string(&value).unwrap_or_default())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 计划哈希**不能**被「调用方式」影响，只能被「跑什么」影响。
    ///
    /// 这条守的是一个会让整个控制台跑不起来的坑：控制台编译计划时 cfg 里
    /// `open_report` 还是默认的 true，写进临时配置交给 `run_master`；而
    /// `run_master` 拿到的 `MasterOpts { no_open: true, .. }` 会把它改成
    /// false。配置一变，哈希就变，闸门于是把**每一次**控制台运行都判成
    /// 「计划已过期」——功能没问题，全是被自己的闸门挡死的。
    ///
    /// 所以这几个字段必须在算指纹前归一：它们来自调用方式（命令行开关、
    /// 控制台按钮），不来自计划本身，改它们不会让「跑什么」有任何不同。
    #[test]
    fn the_fingerprint_ignores_switches_that_come_from_how_it_was_invoked() {
        let planned = Config {
            open_report: true,
            ..Default::default()
        };

        // run_master 按 MasterOpts 施加的那几个覆盖。
        let mut executed = planned.clone();
        executed.open_report = false;
        executed.resume = true;
        executed.screenshot = true;
        executed.agent_host = "10.0.0.9".into();
        executed.agent_port = 12345;
        executed.agent_token = "token".into();

        assert_eq!(
            config_fingerprint(&planned),
            config_fingerprint(&executed),
            "调用开关不该影响计划身份，否则控制台每次都会被自己的闸门挡下"
        );
    }

    /// 反过来：真正决定「跑什么、怎么判」的字段变了，哈希必须变。
    #[test]
    fn the_fingerprint_still_tracks_what_actually_matters() {
        let base = Config::default();
        let mut changed = base.clone();
        changed.iperf.rate_check.max_udp_loss_pct = Some(1.0);
        assert_ne!(
            config_fingerprint(&base),
            config_fingerprint(&changed),
            "丢包门槛决定 PASS/FAIL，必须进计划身份"
        );

        let mut renamed = base.clone();
        renamed.require_same_subnet_for_iperf = !base.require_same_subnet_for_iperf;
        assert_ne!(
            config_fingerprint(&base),
            config_fingerprint(&renamed),
            "同网段约束决定生成哪些单元，必须进计划身份"
        );
    }

    /// 拓扑指纹只作诊断：网卡重扫抖一下但单元没变，不该拦下运行。
    #[test]
    fn a_topology_reading_that_does_not_change_the_units_does_not_invalidate_the_plan() {
        let cfg = Config::default();
        let a = ExecutionPlan::new(&cfg, "topology-before", Vec::new(), Vec::new());
        let b = ExecutionPlan::new(&cfg, "topology-after", Vec::new(), Vec::new());
        assert_eq!(
            a.plan_hash, b.plan_hash,
            "拓扑读数变了但要跑的东西一样，闸门不该拦"
        );
        assert_ne!(a.topology_hash, b.topology_hash, "拓扑指纹本身照常记录");
    }

    /// 配置要能原样穿过一次 JSON 往返——控制台正是这么把计划交给执行端的。
    #[test]
    fn the_fingerprint_survives_the_json_round_trip_the_console_uses() {
        let cfg = Config::default();
        let json = serde_json::to_string_pretty(&cfg).expect("序列化");
        let back: Config = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(
            config_fingerprint(&cfg),
            config_fingerprint(&back),
            "配置过一趟临时 json 就变了样，闸门会把每次控制台运行都挡下"
        );
    }
}
