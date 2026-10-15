//! 内环两轮对比。身份来自类型化配置与测量参数，判定只读历史结果。
use super::config::{Direction, InnerConfig, Parameters, Protocol};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::BTreeMap;

#[derive(Deserialize)]
struct SavedRow {
    index: usize,
    link: String,
    host: String,
    protocol: Option<Protocol>,
    direction: Option<Direction>,
    ip_version: Option<u8>,
    parameters: Option<Parameters>,
    requested_parameters: Option<Parameters>,
    duration_secs: Option<u64>,
    round: Option<u32>,
    repeat: Option<u32>,
    verdict: String,
    #[serde(default)]
    resumed: bool,
    total_mbps: Option<f64>,
    #[serde(default)]
    legs: Vec<SavedLeg>,
}
#[derive(Deserialize)]
struct SavedLeg {
    mbps: Option<f64>,
    #[serde(default)]
    source: Option<String>,
}
fn rate(row: &SavedRow) -> Option<f64> {
    row.total_mbps
        .or_else(|| (row.legs.len() == 1).then(|| row.legs[0].mbps).flatten())
}
fn rate_change(a: &SavedRow, b: &SavedRow) -> Option<f64> {
    if a.legs
        .iter()
        .map(|leg| &leg.source)
        .ne(b.legs.iter().map(|leg| &leg.source))
    {
        return None;
    }
    let (before, after) = (rate(a)?, rate(b)?);
    (before.is_finite() && after.is_finite() && before > 0.0 && after >= 0.0)
        .then_some(after / before - 1.0)
}
#[derive(Serialize)]
pub(super) struct Change {
    pub title: String,
    pub status: &'static str,
    pub baseline_index: Option<usize>,
    pub current_index: Option<usize>,
    pub baseline_verdict: Option<String>,
    pub current_verdict: Option<String>,
    pub baseline_mbps: Option<f64>,
    pub current_mbps: Option<f64>,
    pub rate_change_pct: Option<f64>,
}
/// 网卡口径时读哪块板侧接口：和 [`identity`] 里那一项同一条规则（Ping 和工具口径
/// 不涉及板侧计数，不进身份也不印）。
///
/// 它进了身份就必须印进标题：不印的话，换了统计口的两轮会出现两行标题一字不差的
/// 「新增」「缺失」，读的人看不出它们为什么没对上。
fn counter_label(row: &SavedRow, cfg: &InnerConfig) -> Option<String> {
    if row.protocol? == Protocol::Ping {
        return None;
    }
    let link = cfg
        .links
        .iter()
        .find(|l| l.name == row.link && l.host == row.host)?;
    if link.measurement == super::config::Measurement::Tool {
        return None;
    }
    Some(match link.board_rx_interface.trim() {
        "" => "统计口 自动".into(),
        name => format!("统计口 {name}"),
    })
}
fn title(row: &SavedRow, cfg: &InnerConfig) -> String {
    format!(
        "{} / {} / IPv{} / {} / {} / 第 {} 遍 / 重复 {}",
        row.host,
        row.link,
        row.ip_version.unwrap_or(4),
        row.protocol.map(|p| p.label()).unwrap_or("未知协议"),
        row.direction.map(|d| d.label()).unwrap_or("未知方向"),
        row.round.unwrap_or(1),
        row.repeat.unwrap_or(1)
    ) + &row
        .requested_parameters
        .as_ref()
        .or(row.parameters.as_ref())
        .zip(row.protocol)
        .map(|(p, proto)| format!(" / {}", p.label(proto)))
        .unwrap_or_default()
        + &counter_label(row, cfg)
            .map(|label| format!(" / {label}"))
            .unwrap_or_default()
}
fn identity(row: &SavedRow, cfg: &InnerConfig, report: &Value) -> Option<String> {
    let link = cfg
        .links
        .iter()
        .find(|l| l.name == row.link && l.host == row.host)?;
    let version = row.ip_version.unwrap_or(4);
    if ![4, 6].contains(&version) {
        return None;
    }
    let protocol = row.protocol?;
    let direction = row.direction?;
    let parameters = row
        .requested_parameters
        .as_ref()
        .or(row.parameters.as_ref())?;
    let duration = row
        .duration_secs
        .or_else(|| cfg.workspace.is_none().then_some(cfg.duration_secs))?;
    let serial = report
        .get("capability")
        .and_then(|c| c.get("serial"))
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(&cfg.serial);
    if serial.is_empty() {
        return None;
    }
    let agent = cfg
        .agents
        .iter()
        .find(|a| a.id == link.host)
        .map(|a| json!([a.id, a.address, a.port]));
    Some(
        json!([
            serial,
            agent,
            link.host,
            link.local_interface,
            link.local_address(version),
            link.board_address(version),
            version,
            protocol,
            direction,
            parameters,
            if protocol == Protocol::Ping {
                0
            } else {
                duration
            },
            if protocol == Protocol::Ping {
                None
            } else {
                Some(link.measurement)
            },
            if protocol == Protocol::Ping || link.measurement == super::config::Measurement::Tool {
                None
            } else {
                Some(link.board_rx_interface.trim())
            },
            row.round.unwrap_or(1),
            row.repeat.unwrap_or(1)
        ])
        .to_string(),
    )
}
/// 一侧的行和它所在那一轮的配置。
type Side<'a> = (&'a SavedRow, &'a InnerConfig);
fn change(a: Option<Side>, b: Option<Side>, status: &'static str) -> Change {
    let (row, cfg) = b.or(a).expect("至少一侧存在");
    let (a, b) = (a.map(|(row, _)| row), b.map(|(row, _)| row));
    Change {
        title: title(row, cfg),
        status,
        baseline_index: a.map(|r| r.index),
        current_index: b.map(|r| r.index),
        baseline_verdict: a.map(|r| r.verdict.clone()),
        current_verdict: b.map(|r| r.verdict.clone()),
        baseline_mbps: a.and_then(rate),
        current_mbps: b.and_then(rate),
        rate_change_pct: a
            .zip(b)
            .and_then(|(a, b)| rate_change(a, b))
            .map(|r| r * 100.0),
    }
}
pub(super) fn reports(baseline: &Value, current: &Value) -> Result<Value, String> {
    let read = |report: &Value| -> Result<(InnerConfig, Vec<SavedRow>), String> {
        let cfg =
            super::config::parse_config(&report.get("config").ok_or("历史缺少配置")?.to_string())?;
        let rows = serde_json::from_value(report.get("units").cloned().ok_or("历史缺少单元记录")?)
            .map_err(|e| format!("历史单元结构无效：{e}"))?;
        Ok((cfg, rows))
    };
    let (a_cfg, a) = read(baseline)?;
    let (b_cfg, b) = read(current)?;
    if a.is_empty() && b.is_empty() {
        return Err("两轮均没有测试单元，不生成对比".into());
    }
    let mut grouped: BTreeMap<String, (Vec<&SavedRow>, Vec<&SavedRow>)> = BTreeMap::new();
    let mut changes = Vec::new();
    for row in &a {
        if let Some(key) = identity(row, &a_cfg, baseline) {
            grouped.entry(key).or_default().0.push(row);
        } else {
            changes.push(change(Some((row, &a_cfg)), None, "UNMATCHABLE"));
        }
    }
    for row in &b {
        if let Some(key) = identity(row, &b_cfg, current) {
            grouped.entry(key).or_default().1.push(row);
        } else {
            changes.push(change(None, Some((row, &b_cfg)), "UNMATCHABLE"));
        }
    }
    for (a, b) in grouped.values() {
        if a.len() > 1 || b.len() > 1 {
            changes.extend(
                a.iter()
                    .map(|r| change(Some((r, &a_cfg)), None, "UNMATCHABLE")),
            );
            changes.extend(
                b.iter()
                    .map(|r| change(None, Some((r, &b_cfg)), "UNMATCHABLE")),
            );
            continue;
        }
        let left = a.first().copied();
        let right = b.first().copied();
        let status = match (left, right) {
            (None, _) => "ADDED",
            (_, None) => "MISSING",
            (Some(a), Some(b)) if a.resumed || b.resumed => "RESUMED",
            (Some(a), Some(b)) if a.verdict == "SKIP" || b.verdict == "SKIP" => "SKIPPED",
            (Some(a), Some(b)) if a.verdict == "PASS" && b.verdict != "PASS" => "REGRESSION",
            (Some(a), Some(b)) if a.verdict != "PASS" && b.verdict == "PASS" => "RECOVERED",
            (Some(a), Some(b))
                if rate_change(a, b).is_some_and(|change| {
                    change <= -crate::report::compare::SIGNIFICANT_RATE_CHANGE
                }) =>
            {
                "SLOWER"
            }
            (Some(a), Some(b)) if a.verdict == b.verdict => "SAME",
            _ => "CHANGED",
        };
        changes.push(change(
            left.map(|row| (row, &a_cfg)),
            right.map(|row| (row, &b_cfg)),
            status,
        ));
    }
    changes.sort_by_key(|r| {
        (
            r.current_index.or(r.baseline_index).unwrap_or(usize::MAX),
            r.current_index.is_none(),
        )
    });
    let finished = |report: &Value| {
        report
            .get("current")
            .and_then(Value::as_str)
            .is_some_and(str::is_empty)
            && report.get("error") == Some(&Value::Null)
    };
    let count = |status| changes.iter().filter(|r| r.status == status).count();
    Ok(
        json!({"baseline_finished":finished(baseline),"current_finished":finished(current),"regressions":count("REGRESSION") + count("SLOWER"),"missing":count("MISSING"),"added":count("ADDED"),"unmatchable":count("UNMATCHABLE"),"resumed":count("RESUMED"),"skipped":count("SKIPPED"),"rows":changes}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn changed_counter_interface_or_selected_source_is_not_a_receiver_rate_regression() {
        // 2026-10-14 实机：首轮手选 eth1，复测换成 br0。
        let mut a = saved("MEASURED");
        a_link_interface(&mut a, "eth1");
        let mut b = a.clone();
        a_link_interface(&mut b, "br0");
        let result = reports(&a, &b).unwrap();
        assert_eq!(result["missing"], 1);
        assert_eq!(result["added"], 1);
        // 两行标题必须看得出差在哪：统计口进了身份，也要进标题。
        let titles: Vec<&str> = result["rows"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["title"].as_str().unwrap())
            .collect();
        assert_ne!(titles[0], titles[1], "{titles:?}");
        assert!(
            titles.iter().any(|t| t.ends_with("统计口 br0")),
            "{titles:?}"
        );
        assert!(
            titles.iter().any(|t| t.ends_with("统计口 eth1")),
            "{titles:?}"
        );
        // 留空按地址自动识别，标题照实写「自动」；工具口径不读板侧计数，不印。
        let mut auto = a.clone();
        a_link_interface(&mut auto, "");
        let title = reports(&auto, &auto).unwrap()["rows"][0]["title"].clone();
        assert!(title.as_str().unwrap().ends_with("统计口 自动"), "{title}");
        let mut tool = a.clone();
        tool["config"]["links"][0]["measurement"] = json!("tool");
        let title = reports(&tool, &tool).unwrap()["rows"][0]["title"].clone();
        assert!(!title.as_str().unwrap().contains("统计口"), "{title}");
        let mut b = a.clone();
        b["units"][0]["legs"][0]["mbps"] = json!(10.0);
        b["units"][0]["legs"][0]["source"] = json!("tool");
        let result = reports(&a, &b).unwrap();
        assert_eq!(result["regressions"], 0);
        assert!(result["rows"][0]["rate_change_pct"].is_null());
    }
    fn a_link_interface(report: &mut Value, name: &str) {
        report["config"]["links"][0]["board_rx_interface"] = json!(name);
    }
    fn saved(verdict: &str) -> Value {
        let mut cfg = super::super::tests::example();
        cfg.serial = "test-board".into();
        let link = &cfg.links[0];
        json!({"config":cfg,"current":"","error":null,"probe_only":false,"units":[{"index":1,"link":link.name,"host":link.host,"protocol":"tcp","direction":"upload","ip_version":4,"parameters":{"streams":1,"tcp_window":null,"udp_mbps":null,"udp_length":null,"udp_window":null},"duration_secs":20,"round":1,"repeat":1,"verdict":verdict,"resumed":false,"total_mbps":null,"legs":[{"mbps":900.0}]}]})
    }
    #[test]
    fn receiver_rate_drop_uses_the_subnet_threshold_without_changing_saved_verdicts() {
        let mut a = saved("MEASURED");
        a["units"][0]["legs"][0]["mbps"] = json!(100.0);
        for (after, status, regressions) in
            [(96.0, "SAME", 0), (95.0, "SLOWER", 1), (90.0, "SLOWER", 1)]
        {
            let mut b = a.clone();
            b["units"][0]["legs"][0]["mbps"] = json!(after);
            let result = reports(&a, &b).unwrap();
            assert_eq!(result["rows"][0]["status"], status);
            assert_eq!(result["regressions"], regressions);
            assert_eq!(result["rows"][0]["current_verdict"], "MEASURED");
            b["units"][0]["resumed"] = json!(true);
            assert_eq!(reports(&a, &b).unwrap()["regressions"], 0);
        }
        let mut zero = a.clone();
        zero["units"][0]["legs"][0]["mbps"] = json!(0.0);
        assert!(reports(&zero, &a).unwrap()["rows"][0]["rate_change_pct"].is_null());
        assert_eq!(
            reports(&saved("PASS"), &saved("MEASURED")).unwrap()["regressions"],
            1
        );
    }
    #[test]
    fn thresholds_and_names_are_not_test_identity_but_directions_rounds_and_parameters_are() {
        let baseline = saved("PASS");
        let mut current = saved("RATE_FAIL");
        current["config"]["links"][0]["upload_min_mbps"] = json!(1200.0);
        let out = reports(&baseline, &current).unwrap();
        assert_eq!(out["regressions"], 1);
        assert_eq!(out["rows"][0]["current_mbps"], 900.0);
        for key in ["direction", "round", "parameters"] {
            let mut different = current.clone();
            different["units"][0][key] = match key {
                "direction" => json!("download"),
                "round" => json!(2),
                _ => {
                    json!({"streams":4,"tcp_window":null,"udp_mbps":null,"udp_length":null,"udp_window":null})
                }
            };
            let out = reports(&baseline, &different).unwrap();
            assert_eq!(out["missing"], 1);
            assert_eq!(out["added"], 1);
            assert_eq!(out["regressions"], 0);
        }
    }
    #[test]
    fn historical_reuse_duplicates_and_missing_structure_never_become_unique_real_measurements() {
        let baseline = saved("PASS");
        let mut current = saved("RATE_FAIL");
        current["units"][0]["resumed"] = json!(true);
        assert_eq!(reports(&baseline, &current).unwrap()["resumed"], 1);
        current["units"] = json!([current["units"][0].clone(), current["units"][0].clone()]);
        assert_eq!(reports(&baseline, &current).unwrap()["unmatchable"], 3);
        current["units"] = json!([baseline["units"][0].clone()]);
        current["units"][0]
            .as_object_mut()
            .unwrap()
            .remove("parameters");
        let out = reports(&baseline, &current).unwrap();
        assert_eq!(out["unmatchable"], 1);
        assert_eq!(out["missing"], 1);
        assert_eq!(out["regressions"], 0);
    }
}
