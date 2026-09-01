//! CPE 子网测试工具 — 主控 / 辅测 agent 双机自动化 Ping 与吞吐测试
//!
//! 用法:
//!   cpe_test                     交互选择模式（小白直接双击）
//!   cpe_test agent [--port N]    辅测机常驻服务
//!   cpe_test master [--agent-host IP] [--auto] [--resume] [--config FILE]
//!   cpe_test scan  [--prefix 192.168.,10.10.]   查看本机网卡识别结果

mod agent;
mod cancel;
pub mod clock;
mod cmd;
mod config;
mod console;
mod http_client;
mod master;
mod nic;
#[cfg(test)]
mod parser_properties;
mod ping;
mod protocol;
mod rate;
mod reason;
mod report;
mod resource;
mod screenshot;
mod util;
mod verdict;

/// 控制台默认端口。与 agent 的 28801 错开，两个都在本机跑时不打架。
const DEFAULT_UI_PORT: u16 = 28800;
/// 辅测机状态页端口。只监听回环，和 agent 的业务端口（默认 28801）分开：
/// 业务端口必须对局域网开放，状态页不该跟着一起开放出去。
const DEFAULT_AGENT_UI_PORT: u16 = 28802;

use console::ask;
use master::ui::{run_master, MasterOpts};

fn main() {
    setup_console();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let interactive_launch = args.is_empty();

    let code = real_main(args);

    // 双击启动的场景：结束前停一下，别让窗口一闪而过
    if interactive_launch {
        let _ = ask("\n按回车退出...");
    }
    std::process::exit(code);
}

fn real_main(args: Vec<String>) -> i32 {
    let mode = args.first().map(|s| s.as_str()).unwrap_or("");
    match mode {
        "__cpe-watchdog" => {
            let Some(pid) = args.get(1).and_then(|value| value.parse::<u32>().ok()) else {
                return 2;
            };
            util::run_process_watchdog(pid)
        }
        "agent" => {
            let f = parse_flags(&args[1..]);
            let config_path = flag_value(&f, "config");
            let (mut cfg, _) = config::load_config(config_path.as_deref());
            let port = f
                .get("port")
                .and_then(|p| p.parse().ok())
                .unwrap_or(cfg.agent_port);
            if let Some(token) = flag_value(&f, "token") {
                cfg.agent_token = token;
            }
            if let Some(bind) = flag_value(&f, "bind") {
                cfg.agent_bind = bind;
            }
            let ui_port = if f.contains_key("no-ui") {
                None
            } else {
                Some(
                    f.get("ui-port")
                        .and_then(|p| p.parse().ok())
                        .unwrap_or(DEFAULT_AGENT_UI_PORT),
                )
            };
            let ui_bind = flag_value(&f, "ui-bind").unwrap_or_else(|| "127.0.0.1".into());
            agent::run(port, &cfg, ui_port, &ui_bind); // 不返回
            0
        }
        "master" => {
            let f = parse_flags(&args[1..]);
            run_master(MasterOpts {
                agent_host: flag_value(&f, "agent-host"),
                agent_port: f.get("agent-port").and_then(|p| p.parse().ok()),
                config_path: flag_value(&f, "config"),
                prefixes: flag_value(&f, "prefix").map(|p| split_csv(&p)),
                agent_token: flag_value(&f, "token"),
                auto: f.contains_key("auto"),
                resume: f.contains_key("resume"),
                no_open: f.contains_key("no-open"),
                screenshot: f.contains_key("screenshot"),
                // 命令行直跑没有复核页，不设计划闸。
                expected_plan_hash: None,
                // 命令行没有进度页要伺候，日志就是全部输出。
                observer: None,
                // 命令行这条路的输入就是 config.json 本身，不必再存一份计划原文。
                console_request: None,
            })
        }
        // 从一个已有的 run 目录重新出报告。
        //
        // 用途按重要性排：①**崩溃恢复**——主控在第 10 小时死掉，run 目录里
        // 有增量落盘的 rows.jsonl，用它把已完成部分的完整报告放出来；
        // ②改了报告模板之后拿历史数据重渲染；③排查报告问题时在本地复现
        // 用户的那一份数据。
        "report" => {
            let f = parse_flags(&args[1..]);
            let dir = args[1..]
                .iter()
                .find(|arg| !arg.starts_with("--"))
                .cloned()
                .or_else(|| flag_value(&f, "dir"));
            let Some(dir) = dir else {
                eprintln!("用法: cpe_test report <runs/run_目录>");
                return 2;
            };
            master::replay_report(std::path::Path::new(&dir))
        }
        "scan" => {
            let f = parse_flags(&args[1..]);
            let (cfg, _) = config::load_config(flag_value(&f, "config").as_deref());
            let prefixes = flag_value(&f, "prefix")
                .map(|p| split_csv(&p))
                .unwrap_or(cfg.ipv4_prefixes);
            println!("按前缀 {prefixes:?} 扫描本机网卡...\n");
            let info = nic::scan_host(&prefixes);
            println!("{}", nic::format_nic_table("【本机】", &info));
            println!("(如果少了网卡：检查网线/WiFi 是否连接，或加 --prefix 指定你的网段前缀)");
            0
        }
        "monitor" => {
            let f = parse_flags(&args[1..]);
            let (cfg, _) = config::load_config(flag_value(&f, "config").as_deref());
            let interval: u64 = f.get("interval").and_then(|s| s.parse().ok()).unwrap_or(1);
            let duration: u64 = f.get("duration").and_then(|s| s.parse().ok()).unwrap_or(0);
            let csv_path = flag_value(&f, "csv");
            let iface = flag_value(&f, "iface");
            run_monitor_mode(
                &cfg,
                iface.as_deref(),
                interval,
                duration,
                csv_path.as_deref(),
            )
        }
        "ui" => {
            let f = parse_flags(&args[1..]);
            let port = f
                .get("port")
                .and_then(|p| p.parse().ok())
                .unwrap_or(DEFAULT_UI_PORT);
            // token 走命令行会留在 shell 历史和 ps 的命令行列里，同机的别人
            // 看得到，所以两个都支持环境变量。
            let ui_token = flag_value(&f, "ui-token")
                .or_else(|| std::env::var("CPE_UI_TOKEN").ok())
                .unwrap_or_else(|| config::DEFAULT_TOKEN.to_string());
            master::webui::run(master::webui::UiOpts {
                bind: flag_value(&f, "ui-bind").unwrap_or_else(|| "127.0.0.1".into()),
                port,
                config_path: flag_value(&f, "config"),
                // 页面上的 token 框留空时沿用这里的值，和配置文件里的
                // agent_token 是同一条回落链。
                agent_token: flag_value(&f, "token")
                    .or_else(|| std::env::var("CPE_AGENT_TOKEN").ok()),
                ui_token,
            })
        }
        "-h" | "--help" | "help" => {
            print_help();
            0
        }
        "" => {
            // 无参数：交互选择
            println!("==============================================");
            println!("  CPE 子网测试工具 v{}", env!("CARGO_PKG_VERSION"));
            println!("  两台电脑之间自动化 Ping / iperf3 / ctsTraffic 测试");
            println!("  ctsTraffic 仅支持 Windows 10+");
            println!("==============================================");
            println!("\n请选择模式:");
            println!("  [1] 图形控制台（浏览器里勾选执行，推荐） *");
            println!("  [2] 子网测试（主控命令行，逐项问答）");
            println!("  [3] 辅测 agent（被控端，先在那台上启动）");
            println!("  [4] 只看本机网卡识别结果");
            println!("  [5] 独立网卡速率监控（单机观察链路速率，按 Ctrl+C 停止）");
            let c = ask("选择(回车=默认1): ");
            match c.trim() {
                "2" => run_master(MasterOpts::default()),
                "3" => {
                    let (cfg, _) = config::load_config(None);
                    agent::run(
                        cfg.agent_port,
                        &cfg,
                        Some(DEFAULT_AGENT_UI_PORT),
                        "127.0.0.1",
                    );
                    0
                }
                "4" => {
                    let (cfg, _) = config::load_config(None);
                    let info = nic::scan_host(&cfg.ipv4_prefixes);
                    println!("{}", nic::format_nic_table("【本机】", &info));
                    0
                }
                "5" => {
                    let (cfg, _) = config::load_config(None);
                    run_monitor_mode(&cfg, None, 1, 0, None)
                }
                _ => master::webui::run(master::webui::UiOpts {
                    bind: "127.0.0.1".into(),
                    port: DEFAULT_UI_PORT,
                    config_path: None,
                    agent_token: std::env::var("CPE_AGENT_TOKEN").ok(),
                    ui_token: std::env::var("CPE_UI_TOKEN")
                        .unwrap_or_else(|_| config::DEFAULT_TOKEN.to_string()),
                }),
            }
        }
        other => {
            eprintln!("未知模式: {other}\n");
            print_help();
            2
        }
    }
}

/// 独立网卡速率监控：交互选择网卡（或指定 iface）后连续采样，Ctrl+C 停止。
fn run_monitor_mode(
    cfg: &config::Config,
    iface_override: Option<&str>,
    interval: u64,
    duration: u64,
    csv_path: Option<&str>,
) -> i32 {
    let iface: String = if let Some(name) = iface_override {
        let _ = std::fs::write(".cpe_monitor_iface", name);
        name.to_string()
    } else {
        let info = nic::scan_host(&cfg.ipv4_prefixes);
        let ifs = &info.interfaces;
        if ifs.is_empty() {
            eprintln!(
                "未发现可用网卡（匹配前缀 {:?}），请检查网线/WiFi 连接。",
                cfg.ipv4_prefixes
            );
            return 1;
        }
        let saved = std::fs::read_to_string(".cpe_monitor_iface")
            .ok()
            .map(|s| s.trim().to_string());
        let saved_idx = saved
            .as_ref()
            .and_then(|s| ifs.iter().position(|n| n.name.eq_ignore_ascii_case(s)));
        // 始终展示网卡列表
        println!("{}", nic::format_nic_table("【本机】", &info));
        for (i, n) in ifs.iter().enumerate() {
            let mut extra = String::new();
            if n.speed_mbps > 0 {
                extra.push_str(&format!("  {}Mbps", n.speed_mbps));
            }
            if !n.wifi_band.is_empty() {
                extra.push_str(&format!("  {}", n.wifi_band));
            }
            println!(
                " [{}] {}  {:<12}  {:<16}  {}",
                i + 1,
                n.role,
                n.name,
                n.ipv4,
                extra
            );
        }
        let (name, prompt) = if let Some(idx) = saved_idx {
            (
                &ifs[idx].name,
                format!("回车使用上次网卡 [{}], 或输入序号切换: ", ifs[idx].name),
            )
        } else {
            (&ifs[0].name, "选择网卡序号(回车=1): ".to_string())
        };
        let choice = ask(&prompt);
        let final_name = if choice.is_empty() {
            name.clone()
        } else if let Ok(n) = choice.parse::<usize>() {
            if n >= 1 && n <= ifs.len() {
                let picked = ifs[n - 1].name.clone();
                let _ = std::fs::write(".cpe_monitor_iface", &picked);
                picked
            } else {
                name.clone()
            }
        } else {
            name.clone()
        };
        if saved_idx.is_none() {
            let _ = std::fs::write(".cpe_monitor_iface", &final_name);
        }
        final_name
    };

    if let Err(e) = nic::monitor::run_continuous(&nic::monitor::ContinuousOpts {
        iface: &iface,
        interval_secs: interval,
        duration_secs: duration,
        csv_path,
    }) {
        eprintln!("监控错误: {e}");
        return 1;
    }
    0
}

fn print_help() {
    println!(
        r#"CPE 子网测试工具 v{}

吞吐后端:
  iperf3                     跨平台通用测试
  ctsTraffic                 仅 Windows 10+；两端需同版本程序和 ctsTraffic.exe

用法:
  cpe_test                    交互模式（双击运行就是这个，默认进图形控制台）
  cpe_test ui                 图形控制台：浏览器里勾选执行 (默认 127.0.0.1:28800)
      --port N                指定控制台端口
      --config FILE           指定配置文件 (默认找 ./config.json)
      --token SECRET          与 agent 相同的共享令牌（页面上 token 框留空即用它）
                              也可用 CPE_AGENT_TOKEN 环境变量
      --ui-bind IP            控制台监听地址 (默认 127.0.0.1；IPv6 直接写 ::1 或 ::)
                              非回环地址必须同时给 --ui-token，否则拒绝启动
      --ui-token SECRET       浏览器打开控制台的口令；也可用 CPE_UI_TOKEN 环境变量
  cpe_test agent              辅测机启动常驻服务 (默认端口 28801)
      --port N                指定监听端口
      --token SECRET          共享访问令牌（agent 与主控必须一致；不配置则不启用认证）
      --bind IP               监听地址 (默认 0.0.0.0；可设 127.0.0.1 或测试网卡 IP)
      --ui-port N             本机状态页端口 (默认 28802)
      --ui-bind IP            状态页监听地址 (默认 127.0.0.1；IPv6 直接写 ::1 或 ::)
                              状态页只读且无口令，放开等于公开本机网卡与地址
      --no-ui                 不起状态页，只跑服务
  cpe_test master             主控发起测试
      --agent-host IP         辅测机 IP
      --agent-port N          辅测机端口 (默认 28801)
      --token SECRET          与 agent 相同的共享访问令牌
      --config FILE           指定配置文件 (默认找 ./config.json)
      --auto                  免交互：按配置文件 tests 全部执行
      --resume                24小时内已 PASS 的任务跳过
      --no-open               结束后不自动打开报告
      --prefix A.,B.          临时指定 IPv4 前缀过滤
   cpe_test report <run目录>    从已有运行目录重放报告
       用途: 主控崩溃后把已完成部分的完整报告放出来（结果每个单元都已落盘），
             或改了报告模板后拿历史数据重渲染。例: cpe_test report runs/run_20260830_101112_1234
   cpe_test scan               查看本机网卡识别结果
       --prefix A.,B.
   cpe_test monitor            独立网卡速率监控 (按 Ctrl+C 停止)
       --iface NAME / -n NAME  网卡名称 (不指定则用上次选择或交互选)
       --interval N / -i N     采样间隔秒数 (默认 1)
       --duration N / -d N     监控时长秒数 (0=不限，默认 0)
       --csv FILE / -c FILE    输出 CSV 文件路径 (可选)

文件:
  config.json                 配置文件（可选，同目录）
  runs/run_<时间>_<进程号>/    每次运行的报告、主控日志和全部附件
    report.html               测试报告
    master.log                主控运行进度、摘要和错误
    rows.jsonl                每个单元跑完即追加的结果明细（崩溃后靠它重放报告）
    meta.json                 本次运行的元信息（报告抬头、计划哈希）
    iperf_outputs/            iperf3/ctsTraffic 原文、NIC CSV、截图
  task_results.json           跨运行结果库（RESUME 用）
"#,
        env!("CARGO_PKG_VERSION")
    );
}

/// 取一个「必须带值」的参数。
///
/// `parse_flags` 见到 `--ui-bind` 后面没跟值时会记成空串——布尔开关
/// （`--auto`、`--resume`）就是靠这条存在的，不能改。但对带值的参数来说，
/// 空串是个会一路装成真值走下去的毒药：`--ui-bind` 空串会拼出 ":28800"
/// 这种拼不出来的监听地址，`--ui-token` 空串会 short-circuit 掉
/// `.or_else(|| env::var(...))`，让明明导出了环境变量的人被告知没设口令。
/// 这里统一把空串当成「没给」，并且出声——静默忽略一个用户明确写下的参数
/// 同样不行。
fn flag_value(flags: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    let value = flags.get(key)?.trim().to_string();
    if value.is_empty() {
        eprintln!("!! --{key} 后面缺少值，已忽略（按默认值或环境变量处理）");
        return None;
    }
    Some(value)
}

fn parse_flags(args: &[String]) -> std::collections::HashMap<String, String> {
    let short_map: &[(&str, &str)] = &[
        ("i", "interval"),
        ("n", "iface"),
        ("c", "csv"),
        ("d", "duration"),
    ];
    let mut map = std::collections::HashMap::new();
    let mut i = 0;
    while i < args.len() {
        let a = &args[i];
        if let Some(key) = a.strip_prefix("--") {
            let next_is_val = args
                .get(i + 1)
                .map(|n| !n.starts_with("--"))
                .unwrap_or(false);
            if next_is_val {
                map.insert(key.to_string(), args[i + 1].clone());
                i += 2;
            } else {
                map.insert(key.to_string(), String::new());
                i += 1;
            }
        } else if a.starts_with('-') && a.len() == 2 {
            let ch = &a[1..2];
            if let Some(&(_, long)) = short_map.iter().find(|&&(s, _)| s == ch) {
                let next_is_val = args
                    .get(i + 1)
                    .map(|n| !n.starts_with('-'))
                    .unwrap_or(false);
                if next_is_val {
                    map.insert(long.to_string(), args[i + 1].clone());
                    i += 2;
                } else {
                    map.insert(long.to_string(), String::new());
                    i += 1;
                }
            } else {
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    map
}

fn split_csv(s: &str) -> Vec<String> {
    s.split(',')
        .map(|x| x.trim().to_string())
        .filter(|x| !x.is_empty())
        .collect()
}

/// Windows 控制台切 UTF-8，中文不乱码；顺带声明 DPI 感知让截图拿全屏
fn setup_console() {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::System::Console::{SetConsoleCP, SetConsoleOutputCP};
        let _ = SetConsoleOutputCP(65001);
        let _ = SetConsoleCP(65001);
        let _ = windows::Win32::UI::WindowsAndMessaging::SetProcessDPIAware();
    }
}
