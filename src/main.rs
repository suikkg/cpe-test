//! CPE 子网测试工具 — 单文件双模式（主控 / 辅测 agent）
//!
//! 用法:
//!   cpe_test                     交互选择模式（小白直接双击）
//!   cpe_test agent [--port N]    辅测机常驻服务
//!   cpe_test master [--agent-host IP] [--auto] [--resume] [--config FILE]
//!   cpe_test scan  [--prefix 192.168.,10.10.]   查看本机网卡识别结果

mod agent;
mod cmd;
mod config;
mod http_client;
mod master;
mod nic;
mod ping;
mod protocol;
mod report;
mod screenshot;
mod util;

use master::ui::{run_master, MasterOpts};
use util::ask;

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
        "agent" => {
            let f = parse_flags(&args[1..]);
            let (cfg, _) = config::load_config(f.get("config").map(|s| s.as_str()));
            let port = f
                .get("port")
                .and_then(|p| p.parse().ok())
                .unwrap_or(cfg.agent_port);
            agent::run(port, &cfg); // 不返回
            0
        }
        "master" => {
            let f = parse_flags(&args[1..]);
            run_master(MasterOpts {
                agent_host: f.get("agent-host").cloned(),
                agent_port: f.get("agent-port").and_then(|p| p.parse().ok()),
                config_path: f.get("config").cloned(),
                prefixes: f.get("prefix").map(|p| split_csv(p)),
                auto: f.contains_key("auto"),
                resume: f.contains_key("resume"),
                no_open: f.contains_key("no-open"),
                screenshot: f.contains_key("screenshot"),
            })
        }
        "scan" => {
            let f = parse_flags(&args[1..]);
            let (cfg, _) = config::load_config(f.get("config").map(|s| s.as_str()));
            let prefixes = f
                .get("prefix")
                .map(|p| split_csv(p))
                .unwrap_or(cfg.ipv4_prefixes);
            println!("按前缀 {prefixes:?} 扫描本机网卡...\n");
            let info = nic::scan_host(&prefixes);
            println!("{}", nic::format_nic_table("【本机】", &info));
            println!("(如果少了网卡：检查网线/WiFi 是否连接，或加 --prefix 指定你的网段前缀)");
            0
        }
        "-h" | "--help" | "help" => {
            print_help();
            0
        }
        "" => {
            // 无参数：交互选择
            println!("==============================================");
            println!("  CPE 子网测试工具 v{}", env!("CARGO_PKG_VERSION"));
            println!("  两台电脑之间自动化 ping / iperf3 灌包测试");
            println!("==============================================");
            println!("\n这台电脑是哪个角色?");
            println!("  [1] 主控（带键盘操作、发起测试的这台） *");
            println!("  [2] 辅测 agent（被控端，先在那台上启动）");
            println!("  [3] 只看本机网卡识别结果");
            let c = ask("选择(回车=默认1): ");
            match c.trim() {
                "2" => {
                    let (cfg, _) = config::load_config(None);
                    agent::run(cfg.agent_port, &cfg);
                    0
                }
                "3" => {
                    let (cfg, _) = config::load_config(None);
                    let info = nic::scan_host(&cfg.ipv4_prefixes);
                    println!("{}", nic::format_nic_table("【本机】", &info));
                    0
                }
                _ => run_master(MasterOpts::default()),
            }
        }
        other => {
            eprintln!("未知模式: {other}\n");
            print_help();
            2
        }
    }
}

fn print_help() {
    println!(
        r#"CPE 子网测试工具 v{}

用法:
  cpe_test                    交互模式（双击运行就是这个）
  cpe_test agent              辅测机启动常驻服务 (默认端口 28801)
      --port N                指定监听端口
  cpe_test master             主控发起测试
      --agent-host IP         辅测机 IP
      --agent-port N          辅测机端口 (默认 28801)
      --config FILE           指定配置文件 (默认找 ./config.json)
      --auto                  免交互：按配置文件 tests 全部执行
      --resume                24小时内已 PASS 的任务跳过
      --no-open               结束后不自动打开报告
      --prefix A.,B.          临时指定 IPv4 前缀过滤
  cpe_test scan               查看本机网卡识别结果
      --prefix A.,B.

文件:
  config.json                 配置文件（可选，同目录）
  report_*.html               测试报告
  task_results.json           结果库（RESUME 用）
  iperf_outputs/              截图等输出
"#,
        env!("CARGO_PKG_VERSION")
    );
}

fn parse_flags(args: &[String]) -> std::collections::HashMap<String, String> {
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
