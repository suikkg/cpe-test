from pathlib import Path
import re


def rep(path: str, old: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    if old not in text:
        raise SystemExit(f"{path}: expected text not found: {old[:160]!r}")
    p.write_text(text.replace(old, new, 1), encoding="utf-8")


def rex(path: str, pattern: str, new: str) -> None:
    p = Path(path)
    text = p.read_text(encoding="utf-8")
    next_text, count = re.subn(pattern, new, text, count=1, flags=re.S)
    if count != 1:
        raise SystemExit(f"{path}: regex matched {count}: {pattern[:160]!r}")
    p.write_text(next_text, encoding="utf-8")


# ---- Config -----------------------------------------------------------------
rep(
    "src/config.rs",
    '''#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PingCfg {
    pub count: u32,
    pub payload_sizes: Vec<u32>,
    /// PASS 的最大 RTT 门限（ms）。Ping 还必须同时满足 0% 丢包。
    pub max_rtt_ms: f64,
}

impl Default for PingCfg {
    fn default() -> Self {
        PingCfg {
            count: 180,
            payload_sizes: vec![32, 1600, 65500],
            max_rtt_ms: 20.0,
        }
    }
}
''',
    '''#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct PingCfg {
    pub count: u32,
    pub payload_sizes: Vec<u32>,
    /// payload <= 此值时归为 small。
    pub small_max_bytes: u32,
    /// payload <= 此值时归为 medium；再大归为 large。
    pub medium_max_bytes: u32,
    /// 兼容旧配置字段；现在表示“纯有线 + small”档最大 RTT。
    pub max_rtt_ms: f64,
    pub wired_small_avg_rtt_ms: f64,
    pub wired_medium_avg_rtt_ms: f64,
    pub wired_medium_max_rtt_ms: f64,
    pub wired_large_avg_rtt_ms: f64,
    pub wired_large_max_rtt_ms: f64,
    pub wifi_small_avg_rtt_ms: f64,
    pub wifi_small_max_rtt_ms: f64,
    pub wifi_medium_avg_rtt_ms: f64,
    pub wifi_medium_max_rtt_ms: f64,
    pub wifi_large_avg_rtt_ms: f64,
    pub wifi_large_max_rtt_ms: f64,
}

impl Default for PingCfg {
    fn default() -> Self {
        PingCfg {
            count: 180,
            payload_sizes: vec![32, 1600, 65500],
            small_max_bytes: 128,
            medium_max_bytes: 2000,
            max_rtt_ms: 30.0,
            wired_small_avg_rtt_ms: 10.0,
            wired_medium_avg_rtt_ms: 20.0,
            wired_medium_max_rtt_ms: 50.0,
            wired_large_avg_rtt_ms: 50.0,
            wired_large_max_rtt_ms: 100.0,
            wifi_small_avg_rtt_ms: 30.0,
            wifi_small_max_rtt_ms: 80.0,
            wifi_medium_avg_rtt_ms: 50.0,
            wifi_medium_max_rtt_ms: 100.0,
            wifi_large_avg_rtt_ms: 100.0,
            wifi_large_max_rtt_ms: 200.0,
        }
    }
}
''',
)

rep(
    "src/config.rs",
    '''        if !self.ping.max_rtt_ms.is_finite() || self.ping.max_rtt_ms <= 0.0 {
            problems.push(format!(
                "ping.max_rtt_ms={} 必须是大于 0 的有限值",
                self.ping.max_rtt_ms
            ));
        }
        problems
''',
    '''        if self.ping.small_max_bytes == 0 {
            problems.push("ping.small_max_bytes 必须大于 0".into());
        }
        if self.ping.medium_max_bytes <= self.ping.small_max_bytes {
            problems.push(format!(
                "ping.medium_max_bytes={} 必须大于 ping.small_max_bytes={}",
                self.ping.medium_max_bytes, self.ping.small_max_bytes
            ));
        }
        for (name, avg, max) in [
            ("wired.small", self.ping.wired_small_avg_rtt_ms, self.ping.max_rtt_ms),
            ("wired.medium", self.ping.wired_medium_avg_rtt_ms, self.ping.wired_medium_max_rtt_ms),
            ("wired.large", self.ping.wired_large_avg_rtt_ms, self.ping.wired_large_max_rtt_ms),
            ("wifi.small", self.ping.wifi_small_avg_rtt_ms, self.ping.wifi_small_max_rtt_ms),
            ("wifi.medium", self.ping.wifi_medium_avg_rtt_ms, self.ping.wifi_medium_max_rtt_ms),
            ("wifi.large", self.ping.wifi_large_avg_rtt_ms, self.ping.wifi_large_max_rtt_ms),
        ] {
            if !avg.is_finite() || avg <= 0.0 {
                problems.push(format!("ping.{name}.avg_rtt_ms={avg} 必须是大于 0 的有限值"));
            }
            if !max.is_finite() || max <= 0.0 {
                problems.push(format!("ping.{name}.max_rtt_ms={max} 必须是大于 0 的有限值"));
            }
            if avg > max {
                problems.push(format!("ping.{name}.avg_rtt_ms={avg} 不能大于 max_rtt_ms={max}"));
            }
        }
        problems
''',
)
rep("src/config.rs", "assert_eq!(c.ping.max_rtt_ms, 20.0);", "assert_eq!(c.ping.max_rtt_ms, 30.0);")

# ---- Backend WebUI request/bootstrap -----------------------------------------
request_fields = '''    #[serde(default)]
    pub(super) ping_small_max_bytes: u32,
    #[serde(default)]
    pub(super) ping_medium_max_bytes: u32,
    #[serde(default)]
    pub(super) ping_wired_small_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_small_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_medium_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_medium_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_large_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wired_large_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_small_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_small_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_medium_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_medium_max_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_large_avg_rtt_ms: f64,
    #[serde(default)]
    pub(super) ping_wifi_large_max_rtt_ms: f64,
'''
rep(
    "src/master/webui/model.rs",
    '''    /// Ping 最大 RTT 门限（ms）；0/缺省 = 沿用配置里的 `ping.max_rtt_ms`。
    #[serde(default)]
    pub(super) ping_max_rtt_ms: f64,
''',
    '''    /// 兼容旧前端：有线 small 最大 RTT；0 = 沿用配置。
    #[serde(default)]
    pub(super) ping_max_rtt_ms: f64,
''' + request_fields,
)

bootstrap_fields = '''    pub(super) ping_small_max_bytes: u32,
    pub(super) ping_medium_max_bytes: u32,
    pub(super) ping_wired_small_avg_rtt_ms: f64,
    pub(super) ping_wired_small_max_rtt_ms: f64,
    pub(super) ping_wired_medium_avg_rtt_ms: f64,
    pub(super) ping_wired_medium_max_rtt_ms: f64,
    pub(super) ping_wired_large_avg_rtt_ms: f64,
    pub(super) ping_wired_large_max_rtt_ms: f64,
    pub(super) ping_wifi_small_avg_rtt_ms: f64,
    pub(super) ping_wifi_small_max_rtt_ms: f64,
    pub(super) ping_wifi_medium_avg_rtt_ms: f64,
    pub(super) ping_wifi_medium_max_rtt_ms: f64,
    pub(super) ping_wifi_large_avg_rtt_ms: f64,
    pub(super) ping_wifi_large_max_rtt_ms: f64,
'''
rep(
    "src/master/webui/model.rs",
    '''    pub(super) ping_max_rtt_ms: f64,
    pub(super) screenshot: bool,
''',
    '''    pub(super) ping_max_rtt_ms: f64,
''' + bootstrap_fields + '''    pub(super) screenshot: bool,
''',
)

rep(
    "src/master/webui/api.rs",
    '''        ping_max_rtt_ms: state.cfg.ping.max_rtt_ms,
        screenshot: state.cfg.screenshot,
''',
    '''        ping_max_rtt_ms: state.cfg.ping.max_rtt_ms,
        ping_small_max_bytes: state.cfg.ping.small_max_bytes,
        ping_medium_max_bytes: state.cfg.ping.medium_max_bytes,
        ping_wired_small_avg_rtt_ms: state.cfg.ping.wired_small_avg_rtt_ms,
        ping_wired_small_max_rtt_ms: state.cfg.ping.max_rtt_ms,
        ping_wired_medium_avg_rtt_ms: state.cfg.ping.wired_medium_avg_rtt_ms,
        ping_wired_medium_max_rtt_ms: state.cfg.ping.wired_medium_max_rtt_ms,
        ping_wired_large_avg_rtt_ms: state.cfg.ping.wired_large_avg_rtt_ms,
        ping_wired_large_max_rtt_ms: state.cfg.ping.wired_large_max_rtt_ms,
        ping_wifi_small_avg_rtt_ms: state.cfg.ping.wifi_small_avg_rtt_ms,
        ping_wifi_small_max_rtt_ms: state.cfg.ping.wifi_small_max_rtt_ms,
        ping_wifi_medium_avg_rtt_ms: state.cfg.ping.wifi_medium_avg_rtt_ms,
        ping_wifi_medium_max_rtt_ms: state.cfg.ping.wifi_medium_max_rtt_ms,
        ping_wifi_large_avg_rtt_ms: state.cfg.ping.wifi_large_avg_rtt_ms,
        ping_wifi_large_max_rtt_ms: state.cfg.ping.wifi_large_max_rtt_ms,
        screenshot: state.cfg.screenshot,
''',
)

helper = '''fn apply_ping_policy_overrides(cfg: &mut crate::config::PingCfg, req: &RunRequest) {
    if req.ping_small_max_bytes > 0 { cfg.small_max_bytes = req.ping_small_max_bytes; }
    if req.ping_medium_max_bytes > 0 { cfg.medium_max_bytes = req.ping_medium_max_bytes; }
    if req.ping_wired_small_avg_rtt_ms > 0.0 { cfg.wired_small_avg_rtt_ms = req.ping_wired_small_avg_rtt_ms; }
    let wired_small_max = if req.ping_wired_small_max_rtt_ms > 0.0 { req.ping_wired_small_max_rtt_ms } else { req.ping_max_rtt_ms };
    if wired_small_max > 0.0 { cfg.max_rtt_ms = wired_small_max; }
    if req.ping_wired_medium_avg_rtt_ms > 0.0 { cfg.wired_medium_avg_rtt_ms = req.ping_wired_medium_avg_rtt_ms; }
    if req.ping_wired_medium_max_rtt_ms > 0.0 { cfg.wired_medium_max_rtt_ms = req.ping_wired_medium_max_rtt_ms; }
    if req.ping_wired_large_avg_rtt_ms > 0.0 { cfg.wired_large_avg_rtt_ms = req.ping_wired_large_avg_rtt_ms; }
    if req.ping_wired_large_max_rtt_ms > 0.0 { cfg.wired_large_max_rtt_ms = req.ping_wired_large_max_rtt_ms; }
    if req.ping_wifi_small_avg_rtt_ms > 0.0 { cfg.wifi_small_avg_rtt_ms = req.ping_wifi_small_avg_rtt_ms; }
    if req.ping_wifi_small_max_rtt_ms > 0.0 { cfg.wifi_small_max_rtt_ms = req.ping_wifi_small_max_rtt_ms; }
    if req.ping_wifi_medium_avg_rtt_ms > 0.0 { cfg.wifi_medium_avg_rtt_ms = req.ping_wifi_medium_avg_rtt_ms; }
    if req.ping_wifi_medium_max_rtt_ms > 0.0 { cfg.wifi_medium_max_rtt_ms = req.ping_wifi_medium_max_rtt_ms; }
    if req.ping_wifi_large_avg_rtt_ms > 0.0 { cfg.wifi_large_avg_rtt_ms = req.ping_wifi_large_avg_rtt_ms; }
    if req.ping_wifi_large_max_rtt_ms > 0.0 { cfg.wifi_large_max_rtt_ms = req.ping_wifi_large_max_rtt_ms; }
}

'''
rep("src/master/webui/plan.rs", "pub(super) fn config_from_request(state: &UiState, req: &RunRequest) -> Config {\n", helper + "pub(super) fn config_from_request(state: &UiState, req: &RunRequest) -> Config {\n")
rep("src/master/webui/plan.rs", '''    if req.ping_max_rtt_ms != 0.0 {
        cfg.ping.max_rtt_ms = req.ping_max_rtt_ms;
    }
''', "    apply_ping_policy_overrides(&mut cfg.ping, req);\n")
rep("src/master/webui/plan.rs", '''    if req.ping_max_rtt_ms != 0.0 {
        cfg.ping.max_rtt_ms = req.ping_max_rtt_ms;
    }
''', "    apply_ping_policy_overrides(&mut cfg.ping, req);\n")

# ---- Executor ---------------------------------------------------------------
rex(
    "src/master/executor/ping_leg.rs",
    r'''/// Wi-Fi 空口允许正常的竞争/重传抖动.*?fn ping_acceptance\(out: &PingOut, policy: PingLatencyPolicy\) -> bool \{.*?\n\}''',
    '''#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PingPayloadClass { Small, Medium, Large }
impl PingPayloadClass {
    fn label(self) -> &'static str { match self { Self::Small => "small", Self::Medium => "medium", Self::Large => "large" } }
}
#[derive(Debug, Clone, Copy)]
struct PingLatencyPolicy { wifi: bool, class: PingPayloadClass, avg_rtt_ms: f64, max_rtt_ms: f64 }
fn nic_looks_wifi(nic: &NicInfo) -> bool {
    if nic.is_wifi || !nic.wifi_band.trim().is_empty() { return true; }
    let role = nic.role.to_ascii_lowercase();
    let name = nic.name.to_ascii_lowercase();
    let description = nic.description.to_ascii_lowercase();
    role.contains("wifi") || name.contains("wi-fi") || name.contains("wifi") || name.contains("wlan") || description.contains("wi-fi") || description.contains("wifi") || description.contains("wireless")
}
fn classify_ping_payload(cfg: &crate::config::PingCfg, payload: u32) -> PingPayloadClass {
    if payload <= cfg.small_max_bytes { PingPayloadClass::Small }
    else if payload <= cfg.medium_max_bytes { PingPayloadClass::Medium }
    else { PingPayloadClass::Large }
}
fn ping_latency_policy(src: &NicInfo, dst: &NicInfo, payload: u32, cfg: &crate::config::PingCfg) -> PingLatencyPolicy {
    let wifi = nic_looks_wifi(src) || nic_looks_wifi(dst);
    let class = classify_ping_payload(cfg, payload);
    let (avg_rtt_ms, max_rtt_ms) = match (wifi, class) {
        (false, PingPayloadClass::Small) => (cfg.wired_small_avg_rtt_ms, cfg.max_rtt_ms),
        (false, PingPayloadClass::Medium) => (cfg.wired_medium_avg_rtt_ms, cfg.wired_medium_max_rtt_ms),
        (false, PingPayloadClass::Large) => (cfg.wired_large_avg_rtt_ms, cfg.wired_large_max_rtt_ms),
        (true, PingPayloadClass::Small) => (cfg.wifi_small_avg_rtt_ms, cfg.wifi_small_max_rtt_ms),
        (true, PingPayloadClass::Medium) => (cfg.wifi_medium_avg_rtt_ms, cfg.wifi_medium_max_rtt_ms),
        (true, PingPayloadClass::Large) => (cfg.wifi_large_avg_rtt_ms, cfg.wifi_large_max_rtt_ms),
    };
    PingLatencyPolicy { wifi, class, avg_rtt_ms, max_rtt_ms }
}
fn ping_acceptance(out: &PingOut, policy: PingLatencyPolicy) -> bool {
    let avg_ok = out.rtt_avg.is_some_and(|v| v.is_finite() && v <= policy.avg_rtt_ms);
    let max_ok = out.rtt_max.is_some_and(|v| v.is_finite() && v <= policy.max_rtt_ms);
    out.ok && out.sent > 0 && out.received == out.sent && avg_ok && max_ok
}''',
)
rep("src/master/executor/ping_leg.rs", "        let latency_policy = ping_latency_policy(&t.src.nic, &t.dst.nic, self.cfg.ping.max_rtt_ms);\n", "        let latency_policy = ping_latency_policy(&t.src.nic, &t.dst.nic, t.payload, &self.cfg.ping);\n")
rex("src/master/executor/ping_leg.rs", r'''        let avg_rtt_ok = avg_rtt_ms\.is_none_or\(\|limit\| \{.*?        \}\);''', "        let avg_rtt_ok = out.rtt_avg.is_some_and(|rtt| rtt.is_finite() && rtt <= avg_rtt_ms);")
rep("src/master/executor/ping_leg.rs", "        } else if latency_policy.wifi && out.rtt_avg.is_none() {\n", "        } else if out.rtt_avg.is_none() {\n")
text = Path("src/master/executor/ping_leg.rs").read_text(encoding="utf-8")
text = text.replace("avg_rtt_ms.unwrap_or_default()", "avg_rtt_ms")
Path("src/master/executor/ping_leg.rs").write_text(text, encoding="utf-8")
rex(
    "src/master/executor/ping_leg.rs",
    r'''        let kind_label = match t\.purpose \{.*?        \};''',
    '''        let medium = if latency_policy.wifi { "Wi-Fi" } else { "有线" };
        let criteria = format!(
            "{medium}/{}：0% 丢包，平均 RTT <= {:.0}ms，最大 RTT <= {:.0}ms",
            latency_policy.class.label(), avg_rtt_ms, max_rtt_ms
        );
        let kind_label = match t.purpose {
            PingPurpose::SubnetTest if unit.bidir => format!("★双向子网PING-{tag}（{criteria}）"),
            PingPurpose::SubnetTest => format!("子网PING（{criteria}）"),
            PingPurpose::SubnetDiagnostic => "故障诊断-子网PING".into(),
            PingPurpose::GatewayDiagnostic => "故障诊断-网卡到网关PING".into(),
        };''',
)
rex(
    "src/master/executor/ping_leg.rs",
    r'''#\[cfg\(test\)\]\nmod tests \{.*\Z''',
    '''#[cfg(test)]
mod tests {
    use super::*;
    fn out(received: u32, avg: f64, max: f64) -> PingOut {
        PingOut { ok: received > 0, sent: 180, received, lost: 180 - received, loss_pct: (180 - received) as f64 / 1.8, rtt_min: Some(2.0), rtt_avg: Some(avg), rtt_max: Some(max), ..Default::default() }
    }
    #[test]
    fn arbitrary_payloads_use_ranges() {
        let c = crate::config::PingCfg::default();
        assert_eq!(classify_ping_payload(&c, 32), PingPayloadClass::Small);
        assert_eq!(classify_ping_payload(&c, 128), PingPayloadClass::Small);
        assert_eq!(classify_ping_payload(&c, 129), PingPayloadClass::Medium);
        assert_eq!(classify_ping_payload(&c, 1472), PingPayloadClass::Medium);
        assert_eq!(classify_ping_payload(&c, 2000), PingPayloadClass::Medium);
        assert_eq!(classify_ping_payload(&c, 2001), PingPayloadClass::Large);
        assert_eq!(classify_ping_payload(&c, 65500), PingPayloadClass::Large);
    }
    #[test]
    fn wired_thresholds_scale_by_class() {
        let c = crate::config::PingCfg::default(); let n = NicInfo::default();
        for (p,a,m) in [(32,10.0,30.0),(1600,20.0,50.0),(65500,50.0,100.0)] {
            let x = ping_latency_policy(&n,&n,p,&c);
            assert!(ping_acceptance(&out(180,a,m),x));
            assert!(!ping_acceptance(&out(180,a+0.1,m),x));
            assert!(!ping_acceptance(&out(180,a,m+0.1),x));
        }
    }
    #[test]
    fn wifi_thresholds_scale_by_class_and_require_zero_loss() {
        let c = crate::config::PingCfg::default(); let w = NicInfo { is_wifi: true, ..Default::default() }; let n = NicInfo::default();
        for (p,a,m) in [(32,30.0,80.0),(1600,50.0,100.0),(65500,100.0,200.0)] {
            let x = ping_latency_policy(&w,&n,p,&c);
            assert!(ping_acceptance(&out(180,a,m),x));
            assert!(!ping_acceptance(&out(179,1.0,2.0),x));
        }
    }
    #[test]
    fn old_agent_wifi_metadata_is_recognized() {
        for n in [NicInfo { role: "WIFI5G".into(), ..Default::default() }, NicInfo { name: "WLAN 3".into(), ..Default::default() }, NicInfo { description: "Intel Wireless Adapter".into(), ..Default::default() }] { assert!(nic_looks_wifi(&n)); }
    }
}
''',
)

# ---- Frontend state/DTO ------------------------------------------------------
globals_fields = '''  ping_small_max_bytes: number;
  ping_medium_max_bytes: number;
  ping_wired_small_avg_rtt_ms: number;
  ping_wired_small_max_rtt_ms: number;
  ping_wired_medium_avg_rtt_ms: number;
  ping_wired_medium_max_rtt_ms: number;
  ping_wired_large_avg_rtt_ms: number;
  ping_wired_large_max_rtt_ms: number;
  ping_wifi_small_avg_rtt_ms: number;
  ping_wifi_small_max_rtt_ms: number;
  ping_wifi_medium_avg_rtt_ms: number;
  ping_wifi_medium_max_rtt_ms: number;
  ping_wifi_large_avg_rtt_ms: number;
  ping_wifi_large_max_rtt_ms: number;
'''
rep("ui/src/domain/globals.ts", '''  /** 0 = 沿用配置里的 `ping.max_rtt_ms`。 */
  ping_max_rtt_ms: number;
''', '''  /** 旧字段兼容：有线 small 最大 RTT。 */
  ping_max_rtt_ms: number;
''' + globals_fields)
zero_fields = "".join(f"    {name}: 0,\n" for name in [
    "ping_small_max_bytes","ping_medium_max_bytes","ping_wired_small_avg_rtt_ms","ping_wired_small_max_rtt_ms","ping_wired_medium_avg_rtt_ms","ping_wired_medium_max_rtt_ms","ping_wired_large_avg_rtt_ms","ping_wired_large_max_rtt_ms","ping_wifi_small_avg_rtt_ms","ping_wifi_small_max_rtt_ms","ping_wifi_medium_avg_rtt_ms","ping_wifi_medium_max_rtt_ms","ping_wifi_large_avg_rtt_ms","ping_wifi_large_max_rtt_ms"
])
rep("ui/src/domain/globals.ts", "    ping_max_rtt_ms: 0,\n  };\n", "    ping_max_rtt_ms: 0,\n" + zero_fields + "  };\n")
checks = "".join(f" &&\n    globals.{name} === 0" for name in [
    "ping_small_max_bytes","ping_medium_max_bytes","ping_wired_small_avg_rtt_ms","ping_wired_small_max_rtt_ms","ping_wired_medium_avg_rtt_ms","ping_wired_medium_max_rtt_ms","ping_wired_large_avg_rtt_ms","ping_wired_large_max_rtt_ms","ping_wifi_small_avg_rtt_ms","ping_wifi_small_max_rtt_ms","ping_wifi_medium_avg_rtt_ms","ping_wifi_medium_max_rtt_ms","ping_wifi_large_avg_rtt_ms","ping_wifi_large_max_rtt_ms"
])
rep("ui/src/domain/globals.ts", "    globals.ping_max_rtt_ms === 0\n", "    globals.ping_max_rtt_ms === 0" + checks + "\n")

dto_fields = bootstrap_fields.replace("    pub(super) ", "  ").replace(": f64", ": number").replace(": u32", ": number")
rep("ui/src/api/dto.ts", "  ping_max_rtt_ms: number;\n  screenshot: boolean;\n", "  ping_max_rtt_ms: number;\n" + dto_fields + "  screenshot: boolean;\n")

names = [
    "ping_small_max_bytes","ping_medium_max_bytes","ping_wired_small_avg_rtt_ms","ping_wired_small_max_rtt_ms","ping_wired_medium_avg_rtt_ms","ping_wired_medium_max_rtt_ms","ping_wired_large_avg_rtt_ms","ping_wired_large_max_rtt_ms","ping_wifi_small_avg_rtt_ms","ping_wifi_small_max_rtt_ms","ping_wifi_medium_avg_rtt_ms","ping_wifi_medium_max_rtt_ms","ping_wifi_large_avg_rtt_ms","ping_wifi_large_max_rtt_ms"
]
request_lines = "".join(f"    {n}: globals.{n},\n" for n in names)
rep("ui/src/state/plan.ts", "    ping_max_rtt_ms: globals.ping_max_rtt_ms,\n", "    ping_max_rtt_ms: globals.ping_max_rtt_ms,\n" + request_lines)
rerun_lines = "".join(f"    {n}: count(request.{n}, 0),\n" for n in names)
rep("ui/src/domain/rerun.ts", "    ping_max_rtt_ms: count(request.ping_max_rtt_ms, 0),\n", "    ping_max_rtt_ms: count(request.ping_max_rtt_ms, 0),\n" + rerun_lines)

# ---- Advanced WebUI entry ---------------------------------------------------
p = Path("ui/src/views/run/GlobalDefaults.vue")
s = p.read_text(encoding="utf-8")
s = s.replace("  parseTokenList,\n} from '../../domain/globals';", "  parseTokenList,\n  type UiGlobals,\n} from '../../domain/globals';")
s = s.replace("function positiveDecimal(key: 'ping_max_rtt_ms') {", "function positiveDecimal(key: keyof UiGlobals) {")
s = s.replace("      plan.globals[key] = Number.isFinite(value) && value > 0 ? value : 0;", "      (plan.globals[key] as number) = Number.isFinite(value) && value > 0 ? value : 0;")
s = s.replace("const pingMaxRtt = positiveDecimal('ping_max_rtt_ms');", "const policyKeys = [\n  'ping_small_max_bytes','ping_medium_max_bytes','ping_wired_small_avg_rtt_ms','ping_wired_small_max_rtt_ms','ping_wired_medium_avg_rtt_ms','ping_wired_medium_max_rtt_ms','ping_wired_large_avg_rtt_ms','ping_wired_large_max_rtt_ms','ping_wifi_small_avg_rtt_ms','ping_wifi_small_max_rtt_ms','ping_wifi_medium_avg_rtt_ms','ping_wifi_medium_max_rtt_ms','ping_wifi_large_avg_rtt_ms','ping_wifi_large_max_rtt_ms',\n] as const;\nconst policy = Object.fromEntries(policyKeys.map((key) => [key, positiveDecimal(key)])) as Record<(typeof policyKeys)[number], ReturnType<typeof positiveDecimal>>;")
s = re.sub(r'''      <label>\n        <span>有线 Ping 最大 RTT（ms）</span>.*?      </label>\n''', "", s, count=1, flags=re.S)
marker = "    </div>\n\n    <p class=\"muted hint\">"
advanced = '''    </div>

    <details class="policy">
      <summary><strong>Ping 高级阈值</strong> <span class="muted">自动按链路类型 × payload 档位选择；需要时可临时收紧/放宽</span></summary>
      <div class="policy-grid">
        <label><span>small 最大字节</span><input v-model="policy.ping_small_max_bytes" :placeholder="String(configured?.ping_small_max_bytes ?? 128)" /></label>
        <label><span>medium 最大字节</span><input v-model="policy.ping_medium_max_bytes" :placeholder="String(configured?.ping_medium_max_bytes ?? 2000)" /></label>
        <template v-for="row in [
          ['有线 small','ping_wired_small_avg_rtt_ms','ping_wired_small_max_rtt_ms'],
          ['有线 medium','ping_wired_medium_avg_rtt_ms','ping_wired_medium_max_rtt_ms'],
          ['有线 large','ping_wired_large_avg_rtt_ms','ping_wired_large_max_rtt_ms'],
          ['Wi-Fi small','ping_wifi_small_avg_rtt_ms','ping_wifi_small_max_rtt_ms'],
          ['Wi-Fi medium','ping_wifi_medium_avg_rtt_ms','ping_wifi_medium_max_rtt_ms'],
          ['Wi-Fi large','ping_wifi_large_avg_rtt_ms','ping_wifi_large_max_rtt_ms'],
        ]" :key="row[0]">
          <label><span>{{ row[0] }} Avg RTT（ms）</span><input v-model="policy[row[1] as keyof typeof policy]" :placeholder="String((configured as any)?.[row[1]] ?? '')" /></label>
          <label><span>{{ row[0] }} Max RTT（ms）</span><input v-model="policy[row[2] as keyof typeof policy]" :placeholder="String((configured as any)?.[row[2]] ?? '')" /></label>
        </template>
      </div>
      <p class="muted hint">留空 = 沿用主控 config.json。默认分类：small ≤ 128，medium ≤ 2000，其余为 large；所有档位仍要求 0% 丢包。</p>
    </details>

    <p class="muted hint">'''
if marker not in s:
    raise SystemExit("GlobalDefaults marker missing")
s = s.replace(marker, advanced, 1)
s = re.sub(r'''      <strong>Ping 一律要求 0% 丢包</strong>.*?RTT 数据缺失同样不通过。''', '''      <strong>Ping 一律要求 0% 丢包</strong>，RTT 按“有线/Wi‑Fi × small/medium/large”自动选 Avg/Max 门限；
      <code>-l</code> 可以是任意值，不依赖固定的 32/1600/65500。需要收紧时展开上面的高级阈值。''', s, count=1, flags=re.S)
s = s.replace(".hint { margin: 9px 0 0; font-size: 12px; }", ".hint { margin: 9px 0 0; font-size: 12px; }\n.policy { margin-top: 10px; border-top: 1px dashed var(--line); padding-top: 9px; }\n.policy summary { cursor: pointer; font-size: 12px; }\n.policy-grid { display: grid; grid-template-columns: repeat(auto-fit, minmax(190px, 1fr)); gap: 8px; margin-top: 9px; }")
p.write_text(s, encoding="utf-8")
