//! 本地 Web 控制台：勾选执行 + 实时进度。
//!
//! 形态选择的理由写在 .ai/DESIGN-v4.3.0.md F3：界面主体是「配对 × 方向」的
//! 勾选矩阵、一张可编辑的门限/带宽表、一条实时进度流。这三样在 HTML 里都是
//! 原生控件，在 egui 或裸 Win32 里都要手搓；而 `tiny_http` 本来就是依赖
//! （agent 一直在用），单 exe 和三平台 CI 都不受影响。
//!
//! **这里不是第二条执行路径。** 「开始测试」做的事就是把界面状态序列化成一份
//! config，然后调用同一个 `run_master()`。CI 的 `--auto` 回归防线、既有的
//! configs.json 用法、resume 断点续跑全都不动，控制台只是 config 的图形编辑器
//! 加进度视图——多一条执行路径就多一处会和判定口径分叉的地方。

use crate::config::{load_config, Config, OneOrMany, TestSpec, UdpProfile, UiOrigin};
use crate::http_client;
use crate::master::builder::{self, build_units};
use crate::master::executor::{ResultDb, RESUME_MAX_AGE_HOURS};
use crate::master::run_status::{RunStatus, RunStatusRecorder};
use crate::master::ui::{run_master, MasterOpts};
use crate::protocol::{HealthOut, HostInfo, InfoReq, Resp};
use crate::util::{clear_log_mirror, lock_recover, log_tail_since};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tiny_http::{Header, Method, Request, Response, Server};

const PAGE: &str = include_str!("webui.html");
const MAX_BODY_BYTES: u64 = 1_048_576;

impl Default for UiU32Values {
    fn default() -> Self {
        Self::Many(Vec::new())
    }
}

/// 启动控制台，阻塞直到进程结束。
pub fn run(opts: UiOpts) -> i32 {
    let UiOpts {
        bind,
        port,
        config_path,
        agent_token,
        ui_token,
    } = opts;
    // 控制台能改配置、能启动测试、能下载 config——放到回环之外而不设口令，
    // 等于把这台机器的测试控制权交给同网段的任何人。这里直接不启动，
    // 而不是打印一行警告了事：警告会被划过去，开着的洞不会自己关上。
    if !bind_is_loopback(&bind) && ui_token.is_empty() {
        eprintln!("!! 拒绝在 {bind} 上启动无口令的控制台。");
        eprintln!("!! 控制台可以改配置并发起测试，暴露到网络上必须设访问口令：");
        eprintln!("!!   cpe_test ui --ui-bind {bind} --ui-token 你的口令");
        eprintln!("!! 或者用 SSH 转发，把控制台留在回环上：");
        eprintln!("!!   ssh -L {port}:127.0.0.1:{port} 你@这台机器");
        return 2;
    }
    let (mut cfg, _) = load_config(config_path.as_deref());
    if let Some(token) = agent_token {
        cfg.agent_token = token;
    }
    // 配置文件没写地址时回落到上次连上的那台：控制台每跑完一轮都会经由
    // run_master 把它记下来，只写不读的话等于每次打开都从零开始。
    let agent_host = if cfg.agent_host.trim().is_empty() {
        crate::master::ui::last_agent_host().unwrap_or_default()
    } else {
        cfg.agent_host.clone()
    };
    let console = Arc::new(Console {
        state: Mutex::new(UiState {
            cfg,
            agent_host,
            ..Default::default()
        }),
        running: AtomicBool::new(false),
        report: Mutex::new(String::new()),
        ui_token: ui_token.clone(),
        monitors: Mutex::new(HashMap::new()),
        run_status: Arc::new(RunStatusRecorder::new()),
    });

    // 默认仍只监听回环。放开要靠显式的 --ui-bind，且上面已经拦掉了
    // 「非回环 + 无口令」这种组合。
    let addr = listen_addr(&bind, port);
    let server = match Server::http(addr.as_str()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("!! 控制台无法监听 {addr}: {e}");
            eprintln!("!! 端口可能被占用，换一个：cpe_test ui --port 28900");
            return 2;
        }
    };
    // 口令放在 URL 的查询串里，页面加载后会把它从地址栏抹掉（见 webui.html）。
    // 这样「打开控制台」仍然是复制粘贴一个地址，不用先教人怎么加请求头。
    //
    // 地址用 `display_addr` 而不是监听地址原文：`0.0.0.0` 打不开（见那个函数
    // 的注释），而这一行同时是自动弹窗和「手动复制」两条路的唯一出处。
    let open_addr = display_addr(&bind, port);
    let query = if ui_token.is_empty() {
        String::new()
    } else {
        format!("?token={}", urlencode(&ui_token))
    };
    let url = format!("http://{open_addr}{query}");
    println!("控制台已启动: {url}");
    println!("（浏览器没自动弹出的话，手动复制上面这个地址打开）");
    if bind_is_wildcard(&bind) {
        // 通配绑定的用意基本都是「让别的电脑连过来」，所以把远端要用的写法
        // 一起给出来：上面那个回环地址只在本机有效，照抄到别的电脑上打不开。
        println!("从别的电脑访问：把上面地址里的主机名换成本机的测试网 IP，端口和 ?token= 照抄。");
    }
    if !bind_is_loopback(&bind) {
        println!("注意：控制台正监听在 {bind}，同网段能访问到它；口令泄露即等于测试控制权泄露。");
    }
    crate::console::open_url(&url);

    // 控制台自己就要装 Ctrl+C 处理器，不能等第一轮测试跑起来才由
    // `run_master()` 顺手装上。`cancel` 用 `Once` 注册且**永不撤销**，
    // 而非 Windows 分支的 handler 只置标志、不退出进程——一旦它在别处装好，
    // 下面这个循环又从不查取消标志，SIGINT 就被永久吃掉了：跑过一轮测试之后
    // Ctrl+C 再也关不掉控制台，只能另开终端 kill。
    crate::cancel::setup_cancel_handler();

    let server = Arc::new(server);
    let shutdown = Arc::new(AtomicBool::new(false));
    let workers: Vec<_> = (1..UI_WORKERS)
        .filter_map(|idx| {
            let server = Arc::clone(&server);
            let console = Arc::clone(&console);
            let shutdown = Arc::clone(&shutdown);
            std::thread::Builder::new()
                .name(format!("cpe-ui-http-{idx}"))
                .spawn(move || serve(&server, &console, &shutdown))
                .ok()
        })
        .collect();
    serve(&server, &console, &shutdown);
    // 让还堵在 recv_timeout 里的工作线程立刻收场，而不是各自再等一个超时。
    server.unblock();
    for worker in workers {
        let _ = worker.join();
    }
    // 退出前把监控会话收干净：辅测机侧那路要 POST /monitor/stop，
    // 否则它会一直占着对面的采样线程直到租约到期。
    stop_all_monitors(&console);
    println!("控制台已退出。");
    0
}

mod api;
mod http;
mod import;
mod model;
mod monitor;
mod plan;
mod runs;
mod state;
mod validate;

pub(crate) use http::{bind_is_loopback, bind_is_wildcard, display_addr, listen_addr};
pub(crate) use state::UiOpts;

use api::*;
use http::*;
use import::*;
use model::*;
use monitor::*;
use plan::*;
use state::*;
use validate::*;

#[cfg(test)]
mod tests;
