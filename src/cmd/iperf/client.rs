//! 同步执行 iperf3 client：瞬态连接错误自动重试、可取消，并上报结构化事件。

use super::args::{client_args, cmdline, supports_forceflush, supports_forceflush_with};
use super::parse::classify_live_line;
use crate::protocol::{IperfClientOut, IperfClientReq, IperfEventKind, IperfFlowEvent};
use crate::util::{run_streaming_controlled_timed_with, ProcessExecutor, SystemProcessExecutor};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

pub const CLIENT_RETRIES: u32 = 3;
pub const CLIENT_RETRY_DELAY: Duration = Duration::from_secs(1);
/// client 总超时 = duration + 该值
pub const CLIENT_EXTRA_TIMEOUT: Duration = Duration::from_secs(120);

pub(super) fn is_transient_error(out: &str) -> bool {
    let l = out.to_lowercase();
    l.contains("connection refused")
        || l.contains("unable to connect to server")
        || l.contains("server is busy running a test")
}

pub(super) fn append_attempt_output(history: &mut Vec<String>, attempt: u32, output: &str) {
    history.push(format!(
        "=== client attempt {attempt} ===\n{}",
        output.trim_end()
    ));
}

/// 将 runner 内部的事件 elapsed 对齐到外层 job/call epoch。
/// 首个回调反推固定 origin，后续事件必须复用该值，
/// 避免把轮询或 stdout 缓冲延迟反复加入时间轴。
pub(crate) fn align_event_to_epoch(
    event: &mut IperfFlowEvent,
    callback_elapsed_ms: u64,
    origin_ms: &mut Option<u64>,
) {
    let origin =
        *origin_ms.get_or_insert_with(|| callback_elapsed_ms.saturating_sub(event.elapsed_ms));
    event.elapsed_ms = event.elapsed_ms.saturating_add(origin);
}

fn wait_cancelable(duration: Duration, cancel: Option<&AtomicBool>) -> bool {
    let Some(deadline) = Instant::now().checked_add(duration) else {
        return false;
    };
    loop {
        if cancel
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            return false;
        }
        let now = Instant::now();
        if now >= deadline {
            return true;
        }
        std::thread::sleep((deadline - now).min(Duration::from_millis(50)));
    }
}

/// 执行 iperf3 client，逐行回调并上报结构化事件。
/// cancel 用于异步 job 主动终止，瞬态连接错误仍保留原有自动重试。
pub(crate) fn run_client_controlled_inner<P, F, E>(
    executor: &P,
    forceflush_supported: bool,
    bin: &str,
    req: &IperfClientReq,
    cancel: Option<&AtomicBool>,
    mut on_line: F,
    mut on_event: E,
) -> IperfClientOut
where
    P: ProcessExecutor + ?Sized,
    F: FnMut(&str),
    E: FnMut(IperfFlowEvent),
{
    let mut args = client_args(req);
    // stdout 接到 pipe 后部分 iperf3 会块缓冲，几十秒后才吐 interval，
    // 事件时间线会被整体推迟。只在当前二进制明确支持时开启逐 interval flush，
    // 保持对更老 Windows 版本的兼容。
    if forceflush_supported {
        args.push("--forceflush".into());
    }
    let args_ref: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    let cmd_str = cmdline(bin, &args);
    let Some(timeout) = Duration::from_secs(req.duration).checked_add(CLIENT_EXTRA_TIMEOUT) else {
        return IperfClientOut {
            ok: false,
            timed_out: false,
            cancelled: false,
            process_started: Some(false),
            cleanup_confirmed: Some(true),
            cmd: cmd_str,
            output: format!("iperf3 client duration={} 秒过大，无法执行", req.duration),
        };
    };
    let started = Instant::now();
    on_event(IperfFlowEvent {
        kind: IperfEventKind::Started,
        elapsed_ms: 0,
        mbps: None,
        line: cmd_str.clone(),
    });

    let mut last = IperfClientOut {
        ok: false,
        timed_out: false,
        cancelled: false,
        process_started: Some(false),
        cleanup_confirmed: Some(true),
        cmd: cmd_str.clone(),
        output: String::new(),
    };
    let mut attempt_history = Vec::new();
    for attempt in 1..=CLIENT_RETRIES {
        let out = run_streaming_controlled_timed_with(
            executor,
            bin,
            &args_ref,
            timeout,
            cancel,
            |line, observed_at| {
                on_line(line);
                let elapsed_ms = observed_at
                    .saturating_duration_since(started)
                    .as_millis()
                    .min(u64::MAX as u128) as u64;
                if let Some(event) = classify_live_line(line, elapsed_ms) {
                    on_event(event);
                }
            },
        );
        let merged = out.merged();
        append_attempt_output(&mut attempt_history, attempt, &merged);
        last = IperfClientOut {
            ok: out.ok,
            timed_out: out.timed_out,
            cancelled: out.cancelled,
            process_started: Some(out.process_started()),
            cleanup_confirmed: Some(out.cleanup_confirmed()),
            cmd: cmd_str.clone(),
            output: merged.clone(),
        };
        if out.ok || out.timed_out || out.cancelled || !out.cleanup_confirmed() {
            break;
        }
        if attempt < CLIENT_RETRIES && is_transient_error(&merged) {
            let message = format!(
                "(第 {attempt} 次连接失败，{}s 后重试...)",
                CLIENT_RETRY_DELAY.as_secs()
            );
            on_line(&message);
            on_event(IperfFlowEvent {
                kind: IperfEventKind::Retry,
                elapsed_ms: started.elapsed().as_millis() as u64,
                mbps: None,
                line: message,
            });
            if !wait_cancelable(CLIENT_RETRY_DELAY, cancel) {
                last.cancelled = true;
                attempt_history.push("iperf3 client cancelled before retry".into());
                break;
            }
            continue;
        }
        break;
    }
    last.output = attempt_history.join("\n");
    on_event(IperfFlowEvent {
        kind: IperfEventKind::Ended,
        elapsed_ms: started.elapsed().as_millis() as u64,
        mbps: None,
        line: if last.ok {
            "iperf3 client completed".into()
        } else if last.cancelled {
            "iperf3 client cancelled".into()
        } else if last.timed_out {
            "iperf3 client timed out".into()
        } else {
            "iperf3 client failed".into()
        },
    });
    last
}

/// 可注入进程执行器的 iperf client 入口。测试可在不启动 iperf3 的情况下
/// 驱动完整参数、事件解析、取消与 cleanup evidence 路径。
#[cfg_attr(not(test), allow(dead_code))]
pub fn run_client_controlled_with_executor<P, F, E>(
    executor: &P,
    bin: &str,
    req: &IperfClientReq,
    cancel: Option<&AtomicBool>,
    on_line: F,
    on_event: E,
) -> IperfClientOut
where
    P: ProcessExecutor + ?Sized,
    F: FnMut(&str),
    E: FnMut(IperfFlowEvent),
{
    let forceflush_supported = supports_forceflush_with(executor, bin);
    run_client_controlled_inner(
        executor,
        forceflush_supported,
        bin,
        req,
        cancel,
        on_line,
        on_event,
    )
}

pub fn run_client_controlled<F, E>(
    bin: &str,
    req: &IperfClientReq,
    cancel: Option<&AtomicBool>,
    on_line: F,
    on_event: E,
) -> IperfClientOut
where
    F: FnMut(&str),
    E: FnMut(IperfFlowEvent),
{
    run_client_controlled_inner(
        &SystemProcessExecutor,
        supports_forceflush(bin),
        bin,
        req,
        cancel,
        on_line,
        on_event,
    )
}

/// 兼容旧调用点：同步运行，无取消信号，仅保留逐行回调。
pub fn run_client<F: FnMut(&str)>(bin: &str, req: &IperfClientReq, on_line: F) -> IperfClientOut {
    run_client_controlled(bin, req, None, on_line, |_| {})
}
