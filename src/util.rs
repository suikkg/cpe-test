//! 公共工具：子进程执行(带超时/GBK解码)、日志、时间、iperf3 定位等

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

/// 给长生命周期外部命令设置“父进程死亡即回收”的边界。
///
/// Linux 的 `PR_SET_PDEATHSIG` 覆盖 agent 被 SIGKILL 等方式强杀的场景；
/// fork/exec 之间再检查一次父 PID，避免刚创建子进程时父线程已经退出却还没
/// 来得及安装死亡信号。Windows agent 通过 Job Object 在启动阶段绑定整个进程
/// 树；macOS/其他 Unix 由同包 watchdog 补上父进程死亡后的清理。
pub fn configure_managed_command(command: &mut Command) {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::process::CommandExt;

        let expected_parent = std::process::id() as libc::pid_t;
        // `pre_exec` 只调用 async-signal-safe 的 libc 原语；不要在这里分配、加锁
        // 或执行 Rust 级别的复杂逻辑，因为闭包运行于 fork 后、exec 前的窗口。
        unsafe {
            command.pre_exec(move || {
                if libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) != 0 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::getppid() != expected_parent {
                    // 父进程在 fork 与 prctl 之间退出：主动结束这个孤儿。
                    libc::kill(libc::getpid(), libc::SIGKILL);
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::Interrupted,
                        "父进程已退出",
                    ));
                }
                Ok(())
            });
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = command;
}

/// macOS/其他 Unix 没有 Linux 的 `PDEATHSIG`，用一个极小的同包 watchdog
/// 监听父进程持有的 pipe：父进程异常死亡时 pipe EOF，watchdog 杀掉目标工具。
/// Linux 由内核死亡信号负责，Windows 由 Job Object 负责，因此这两类不额外
/// 创建 watchdog。
pub struct ManagedChildWatchdog {
    process: Child,
    keepalive: Option<ChildStdin>,
}

impl ManagedChildWatchdog {
    /// 关闭父进程存活 pipe，并确认 watchdog 已退出。
    pub fn stop(&mut self) -> Result<(), String> {
        self.keepalive.take();
        match self.process.wait_timeout(Duration::from_secs(1)) {
            Ok(Some(_)) => Ok(()),
            Ok(None) => {
                self.process
                    .kill()
                    .map_err(|error| format!("停止子进程 watchdog 失败: {error}"))?;
                self.process
                    .wait()
                    .map(|_| ())
                    .map_err(|error| format!("回收子进程 watchdog 失败: {error}"))
            }
            Err(error) => Err(format!("等待子进程 watchdog 失败: {error}")),
        }
    }
}

/// 为目标子进程创建跨平台补偿 watchdog。
pub fn spawn_managed_watchdog(target_pid: u32) -> std::io::Result<Option<ManagedChildWatchdog>> {
    #[cfg(all(unix, not(target_os = "linux"), not(test)))]
    {
        let exe = std::env::current_exe()?;
        let mut command = Command::new(exe);
        command
            .args(["__cpe-watchdog", &target_pid.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut process = command.spawn()?;
        let Some(keepalive) = process.stdin.take() else {
            let _ = process.kill();
            let _ = process.wait();
            return Err(std::io::Error::other("watchdog stdin pipe 创建失败"));
        };
        Ok(Some(ManagedChildWatchdog {
            process,
            keepalive: Some(keepalive),
        }))
    }
    #[cfg(any(target_os = "linux", windows, test))]
    {
        let _ = target_pid;
        Ok(None)
    }
}

/// 隐藏的 watchdog 子命令入口。正常情况下只会在 macOS/其他 Unix 被调用。
pub fn run_process_watchdog(target_pid: u32) -> i32 {
    #[cfg(not(unix))]
    let _ = target_pid;
    let mut stdin = std::io::stdin().lock();
    let mut buf = [0u8; 1];
    loop {
        match stdin.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
    }
    #[cfg(unix)]
    unsafe {
        let _ = libc::kill(target_pid as libc::pid_t, libc::SIGKILL);
    }
    0
}

/// 为 agent 进程安装平台级的子进程容器。
///
/// Linux 子进程逐个绑定父死亡信号；Windows 则把 agent 自身加入带
/// `KILL_ON_JOB_CLOSE` 的 Job Object，因此 agent 异常退出时所有受管工具都会
/// 一起消失。非 Windows 平台没有额外初始化动作。
pub fn initialize_agent_process_lifetime() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::core::PCWSTR;
        use windows::Win32::System::JobObjects::{
            AssignProcessToJobObject, CreateJobObjectW, JobObjectExtendedLimitInformation,
            SetInformationJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
            JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;

        let job = unsafe {
            CreateJobObjectW(None, PCWSTR::null())
                .map_err(|error| format!("创建 agent Job Object 失败: {error}"))?
        };
        let mut limits = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectExtendedLimitInformation,
                (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
            )
            .map_err(|error| format!("配置 agent Job Object 失败: {error}"))?;
            AssignProcessToJobObject(job, GetCurrentProcess())
                .map_err(|error| format!("绑定 agent 到 Job Object 失败: {error}"))?;
        }
        // HANDLE 是原始 OS 句柄，离开这个 Rust 变量不会自动 CloseHandle；
        // 让它一直持有到 agent 进程结束，进程退出时 Windows 会关闭句柄并按
        // KILL_ON_JOB_CLOSE 回收所有成员。
        let _ = job;
    }
    Ok(())
}

/// 字节解码：优先 UTF-8，失败按 GBK（中文 Windows cmd 输出）
pub fn decode_bytes(b: &[u8]) -> String {
    match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => encoding_rs::GBK.decode(b).0.into_owned(),
    }
}

#[derive(Debug, Default)]
pub struct CmdOut {
    pub ok: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub stdout: String,
    pub stderr: String,
}

impl CmdOut {
    pub fn merged(&self) -> String {
        if self.stderr.trim().is_empty() {
            self.stdout.clone()
        } else if self.stdout.trim().is_empty() {
            self.stderr.clone()
        } else {
            format!("{}\n{}", self.stdout, self.stderr)
        }
    }

    /// 命令是否已经成功 spawn。deadline 无法表示或 Command::spawn 失败时为 false。
    pub fn process_started(&self) -> bool {
        !self.stderr.contains("命令超时时间过大，无法执行")
            && !self.stderr.contains("启动命令失败:")
    }

    /// 返回前是否确认完成 wait/reap。kill 本身报错可能只是进程恰好已退出；
    /// 只有最终回收失败才表示禁止复用同一端口开始下一轮。
    pub fn cleanup_confirmed(&self) -> bool {
        !self.stderr.contains("回收子进程失败")
    }
}

/// 可执行命令的描述。业务层只需要描述命令，不必直接持有 `std::process::Child`。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProcessSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl ProcessSpec {
    pub fn new(program: impl Into<String>, args: &[&str]) -> Self {
        Self {
            program: program.into(),
            args: args.iter().map(|arg| (*arg).to_string()).collect(),
        }
    }
}

/// 子进程执行边界。
///
/// 生产环境使用 `SystemProcessExecutor`，测试可以注入脚本化实现来制造
/// 输出截断、取消、超时和拒绝回收，而不需要启动 iperf3/ctsTraffic。
pub trait ProcessExecutor: Send + Sync {
    fn run(&self, spec: &ProcessSpec, timeout: Duration) -> CmdOut;

    fn run_streaming(
        &self,
        spec: &ProcessSpec,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        on_line: &mut dyn FnMut(&str, Instant),
    ) -> CmdOut;
}

#[derive(Debug, Clone, Copy, Default)]
pub struct SystemProcessExecutor;

impl ProcessExecutor for SystemProcessExecutor {
    fn run(&self, spec: &ProcessSpec, timeout: Duration) -> CmdOut {
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        run_cmd_system(&spec.program, &args, timeout)
    }

    fn run_streaming(
        &self,
        spec: &ProcessSpec,
        timeout: Duration,
        cancel: Option<&AtomicBool>,
        on_line: &mut dyn FnMut(&str, Instant),
    ) -> CmdOut {
        let args: Vec<&str> = spec.args.iter().map(String::as_str).collect();
        run_streaming_system(&spec.program, &args, timeout, cancel, on_line)
    }
}

static SYSTEM_PROCESS_EXECUTOR: SystemProcessExecutor = SystemProcessExecutor;

/// 通过指定执行器运行一次命令；正式代码通常使用 `run_cmd` 兼容包装。
pub fn run_cmd_with_executor<E: ProcessExecutor + ?Sized>(
    executor: &E,
    prog: &str,
    args: &[&str],
    timeout: Duration,
) -> CmdOut {
    executor.run(&ProcessSpec::new(prog, args), timeout)
}

/// 通过指定执行器运行流式命令；便于 manager/解析器测试注入 fake。
pub fn run_streaming_controlled_timed_with<E: ProcessExecutor + ?Sized, F: FnMut(&str, Instant)>(
    executor: &E,
    prog: &str,
    args: &[&str],
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    mut on_line: F,
) -> CmdOut {
    executor.run_streaming(&ProcessSpec::new(prog, args), timeout, cancel, &mut on_line)
}

fn terminate_and_reap(child: &mut Child) -> Vec<String> {
    let mut errors = Vec::new();
    if let Err(error) = child.kill() {
        errors.push(format!("终止子进程失败: {error}"));
    }
    if let Err(error) = child.wait() {
        errors.push(format!("回收子进程失败: {error}"));
    }
    errors
}

fn append_errors(mut stderr: String, errors: &[String]) -> String {
    if errors.is_empty() {
        return stderr;
    }
    if !stderr.is_empty() && !stderr.ends_with('\n') {
        stderr.push('\n');
    }
    stderr.push_str(&errors.join("\n"));
    stderr
}

fn stop_watchdog(watchdog: &mut Option<ManagedChildWatchdog>, errors: &mut Vec<String>) {
    if let Some(mut watchdog) = watchdog.take() {
        if let Err(error) = watchdog.stop() {
            errors.push(error);
        }
    }
}

/// 执行命令，等待结束（超时强杀），返回解码后的输出
fn run_cmd_system(prog: &str, args: &[&str], timeout: Duration) -> CmdOut {
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_managed_command(&mut c);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr: format!("启动命令失败: {prog} ({e})"),
            }
        }
    };
    let mut watchdog = match spawn_managed_watchdog(child.id()) {
        Ok(watchdog) => watchdog,
        Err(error) => {
            let cleanup_errors = terminate_and_reap(&mut child);
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr: append_errors(format!("创建命令 watchdog 失败: {error}"), &cleanup_errors),
            };
        }
    };
    let so = child.stdout.take().expect("stdout piped");
    let se = child.stderr.take().expect("stderr piped");
    let th_o = match std::thread::Builder::new()
        .name(format!("cmd-{prog}-stdout"))
        .spawn(move || {
            let mut so = so;
            let mut v = Vec::new();
            let _ = so.read_to_end(&mut v);
            v
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let mut cleanup_errors = terminate_and_reap(&mut child);
            stop_watchdog(&mut watchdog, &mut cleanup_errors);
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr: append_errors(
                    format!("创建命令 stdout reader 失败: {error}"),
                    &cleanup_errors,
                ),
            };
        }
    };
    let th_e = match std::thread::Builder::new()
        .name(format!("cmd-{prog}-stderr"))
        .spawn(move || {
            let mut se = se;
            let mut v = Vec::new();
            let _ = se.read_to_end(&mut v);
            v
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let mut cleanup_errors = terminate_and_reap(&mut child);
            stop_watchdog(&mut watchdog, &mut cleanup_errors);
            let stdout = decode_bytes(&th_o.join().unwrap_or_default());
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout,
                stderr: append_errors(
                    format!("创建命令 stderr reader 失败: {error}"),
                    &cleanup_errors,
                ),
            };
        }
    };
    let mut process_errors = Vec::new();
    let (ok, timed_out) = match child.wait_timeout(timeout) {
        Ok(Some(st)) => (st.success(), false),
        Ok(None) => {
            process_errors.extend(terminate_and_reap(&mut child));
            (false, true)
        }
        Err(error) => {
            process_errors.push(format!("等待子进程失败: {error}"));
            process_errors.extend(terminate_and_reap(&mut child));
            (false, false)
        }
    };
    let stdout = decode_bytes(&th_o.join().unwrap_or_default());
    stop_watchdog(&mut watchdog, &mut process_errors);
    let stderr = append_errors(
        decode_bytes(&th_e.join().unwrap_or_default()),
        &process_errors,
    );
    CmdOut {
        ok,
        timed_out,
        cancelled: false,
        stdout,
        stderr,
    }
}

/// 使用生产系统执行器的兼容入口。
pub fn run_cmd(prog: &str, args: &[&str], timeout: Duration) -> CmdOut {
    run_cmd_with_executor(&SYSTEM_PROCESS_EXECUTOR, prog, args, timeout)
}

/// 执行命令并逐行回调；cancel=true 时主动终止子进程。
/// 异步 agent job 和主控本地 job 共用这一实现，避免 HTTP handler
/// 被长时间 iperf3 进程占住。
#[cfg(test)]
pub fn run_streaming_controlled<F: FnMut(&str)>(
    prog: &str,
    args: &[&str],
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    mut on_line: F,
) -> CmdOut {
    run_streaming_controlled_timed(prog, args, timeout, cancel, move |line, _observed_at| {
        on_line(line)
    })
}

/// 执行命令并逐行回调，同时提供 stdout reader 真正读到该行的单调时钟时间。
/// 即使主线程在 child wait 或 reader join 后才消费 channel，调用方也不会
/// 把这段排队时间误认为输出产生时间。
fn run_streaming_system<F: FnMut(&str, Instant)>(
    prog: &str,
    args: &[&str],
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    mut on_line: F,
) -> CmdOut {
    let Some(deadline) = Instant::now().checked_add(timeout) else {
        return CmdOut {
            ok: false,
            timed_out: false,
            cancelled: false,
            stdout: String::new(),
            stderr: format!("命令超时时间过大，无法执行: {} 秒", timeout.as_secs()),
        };
    };
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    configure_managed_command(&mut c);
    let mut child = match c.spawn() {
        Ok(c) => c,
        Err(e) => {
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr: format!("启动命令失败: {prog} ({e})"),
            }
        }
    };
    let mut watchdog = match spawn_managed_watchdog(child.id()) {
        Ok(watchdog) => watchdog,
        Err(error) => {
            let cleanup_errors = terminate_and_reap(&mut child);
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr: append_errors(format!("创建命令 watchdog 失败: {error}"), &cleanup_errors),
            };
        }
    };
    let so = child.stdout.take().expect("stdout piped");
    let se = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel::<(Vec<u8>, Instant)>();
    let th_o = match std::thread::Builder::new()
        .name("streaming-command-stdout".into())
        .spawn(move || {
            let mut r = BufReader::new(so);
            loop {
                let mut line = Vec::new();
                match std::io::BufRead::read_until(&mut r, b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        let observed_at = Instant::now();
                        if tx.send((line, observed_at)).is_err() {
                            break;
                        }
                    }
                }
            }
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let mut cleanup_errors = terminate_and_reap(&mut child);
            stop_watchdog(&mut watchdog, &mut cleanup_errors);
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout: String::new(),
                stderr: append_errors(
                    format!("创建流式命令 stdout reader 失败: {error}"),
                    &cleanup_errors,
                ),
            };
        }
    };
    let th_e = match std::thread::Builder::new()
        .name("streaming-command-stderr".into())
        .spawn(move || {
            let mut se = se;
            let mut v = Vec::new();
            let _ = se.read_to_end(&mut v);
            v
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let mut cleanup_errors = terminate_and_reap(&mut child);
            stop_watchdog(&mut watchdog, &mut cleanup_errors);
            let _ = th_o.join();
            let mut stdout = String::new();
            while let Ok((bytes, _observed_at)) = rx.try_recv() {
                stdout.push_str(&decode_bytes(&bytes));
            }
            return CmdOut {
                ok: false,
                timed_out: false,
                cancelled: false,
                stdout,
                stderr: append_errors(
                    format!("创建流式命令 stderr reader 失败: {error}"),
                    &cleanup_errors,
                ),
            };
        }
    };

    let mut collected = String::new();
    let mut timed_out = false;
    let mut cancelled = false;
    let mut callback_panic = None;
    let mut observed_status = None;
    let mut process_errors = Vec::new();
    loop {
        // 先观察 OS 进程状态，再处理 controller 的 cancel。这样即使进程已经
        // 自然异常退出、但 stdout 后代仍持有 pipe 或 worker 尚未发布结果，
        // 也不会被稍后到达的 cancel 覆盖成“正常停止”。
        if let Ok(Some(status)) = child.try_wait() {
            observed_status = Some(status);
            break;
        }
        if cancel
            .map(|flag| flag.load(Ordering::SeqCst))
            .unwrap_or(false)
        {
            // cancel 观察点之后再检查一次，关闭 try_wait 与 cancel load 之间
            // 的竞争窗口。此处仍在运行时，才把本轮线性化为 controller stop。
            if let Ok(Some(status)) = child.try_wait() {
                observed_status = Some(status);
            } else {
                cancelled = true;
            }
            break;
        }
        let now = Instant::now();
        if now >= deadline {
            if let Ok(Some(status)) = child.try_wait() {
                observed_status = Some(status);
            } else {
                timed_out = true;
            }
            break;
        }
        // 控制轮询保持在 100ms 内，使同步 stop 不必额外等半秒才开始 kill。
        let wait = std::cmp::min(deadline - now, Duration::from_millis(100));
        match rx.recv_timeout(wait) {
            Ok((bytes, observed_at)) => {
                let s = decode_bytes(&bytes);
                collected.push_str(&s);
                if let Err(payload) =
                    catch_unwind(AssertUnwindSafe(|| on_line(s.trim_end(), observed_at)))
                {
                    callback_panic = Some(payload);
                    // 先走完整 kill/wait/join，再把 panic 交回上层隔离器。
                    cancelled = true;
                    break;
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }

    // 只有 wait 完成后才允许返回；上层同步 stop 以此作为“子进程已回收”的依据。
    let status = if observed_status.is_some() {
        observed_status
    } else if timed_out || cancelled {
        if let Err(e) = child.kill() {
            // 进程可能恰好自然退出，后续 wait 仍可正常确认；保留错误供异常时诊断。
            process_errors.push(format!("终止子进程时返回错误: {e}"));
        }
        match child.wait() {
            Ok(status) => Some(status),
            Err(e) => {
                process_errors.push(format!("回收子进程失败: {e}"));
                None
            }
        }
    } else {
        match child.wait_timeout(Duration::from_secs(5)) {
            Ok(Some(status)) => Some(status),
            Ok(None) => {
                timed_out = true;
                if let Err(e) = child.kill() {
                    process_errors.push(format!("超时后终止子进程失败: {e}"));
                }
                match child.wait() {
                    Ok(status) => Some(status),
                    Err(e) => {
                        process_errors.push(format!("超时后回收子进程失败: {e}"));
                        None
                    }
                }
            }
            Err(e) => {
                process_errors.push(format!("等待子进程失败: {e}"));
                if let Err(kill_error) = child.kill() {
                    process_errors.push(format!("等待失败后终止子进程失败: {kill_error}"));
                }
                match child.wait() {
                    Ok(status) => Some(status),
                    Err(wait_error) => {
                        process_errors.push(format!("等待失败后回收子进程失败: {wait_error}"));
                        None
                    }
                }
            }
        }
    };

    // 子进程退出后 pipe 已关闭；join stdout reader，确保没有后台读取线程和尾部输出残留。
    let _ = th_o.join();
    // reader 退出后 channel 不再产生新数据，此时排空才不会漏掉最后几行。
    while let Ok((bytes, observed_at)) = rx.try_recv() {
        let s = decode_bytes(&bytes);
        collected.push_str(&s);
        if callback_panic.is_none() {
            if let Err(payload) =
                catch_unwind(AssertUnwindSafe(|| on_line(s.trim_end(), observed_at)))
            {
                callback_panic = Some(payload);
            }
        }
    }
    let ok = status
        .map(|status| status.success() && !timed_out && !cancelled)
        .unwrap_or(false);
    stop_watchdog(&mut watchdog, &mut process_errors);
    let stderr = append_errors(
        decode_bytes(&th_e.join().unwrap_or_default()),
        &process_errors,
    );
    if let Some(payload) = callback_panic {
        resume_unwind(payload);
    }
    CmdOut {
        ok,
        timed_out,
        cancelled,
        stdout: collected,
        stderr,
    }
}

/// 使用生产系统执行器的兼容入口。
pub fn run_streaming_controlled_timed<F: FnMut(&str, Instant)>(
    prog: &str,
    args: &[&str],
    timeout: Duration,
    cancel: Option<&AtomicBool>,
    on_line: F,
) -> CmdOut {
    run_streaming_controlled_timed_with(
        &SYSTEM_PROCESS_EXECUTOR,
        prog,
        args,
        timeout,
        cancel,
        on_line,
    )
}

// ---------------- 日志 ----------------

// Web 控制台会在同一进程内连续跑多轮，每轮都有自己的 master.log。
// `OnceLock<Mutex<File>>` 只能记住第一轮的句柄，第二轮以后会继续把日志写进
// 第一轮目录；用可替换的 Option 才能在每轮开始时原子切换目标文件。
static LOG_FILE: Mutex<Option<File>> = Mutex::new(None);

/// 主控模式下开启文件日志（控制台 + 文件双写）
pub fn log_to_file(path: &Path) {
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        *lock_recover(&LOG_FILE) = Some(f);
    }
}

/// Web 控制台的内存日志镜像：只保留最近若干行，供前端轮询。
///
/// 之所以不让控制台去 tail `master.log`：那份文件的路径由 `run_master`
/// 在运行开始时自己创建，界面在点下「开始测试」的那一刻还不知道它叫什么；
/// 而且 tail 一个正在被追加的文件要处理编码和截断，得不偿失。
/// 用 `VecDeque` 而不是 `Vec`：镜像封顶之后每写一行都要丢掉最早的一行，
/// `Vec::drain(..1)` 每次要搬 4000 个 `String`（约 96 KB memmove），
/// 三万行就是 3 GB 的无谓拷贝。`pop_front()` 是 O(1)。
static LOG_MIRROR: Mutex<VecDeque<String>> = Mutex::new(VecDeque::new());
/// 已经产生过的总行数（含被裁掉的）。
///
/// 必须和镜像长度分开记：镜像被 `LOG_MIRROR_MAX_LINES` 封顶，用它当游标的话
/// 一旦写满就永远停在 4000，前端每次都拿到「没有新行」，进度视图从此不再更新。
/// 一次 120 单元的运行会打出三万多行，这不是边界情况而是常态。
static LOG_MIRROR_TOTAL: Mutex<usize> = Mutex::new(0);
const LOG_MIRROR_MAX_LINES: usize = 4000;

/// 打印并写日志文件
pub fn logln(s: &str) {
    println!("{s}");
    if let Ok(mut target) = LOG_FILE.lock() {
        if let Some(f) = target.as_mut() {
            let _ = writeln!(f, "{s}");
        }
    }
    if let Ok(mut mirror) = LOG_MIRROR.lock() {
        mirror.push_back(s.to_string());
        while mirror.len() > LOG_MIRROR_MAX_LINES {
            mirror.pop_front();
        }
        if let Ok(mut total) = LOG_MIRROR_TOTAL.lock() {
            *total += 1;
        }
    }
}

/// 取回 `from` 之后新增的日志行，以及镜像里当前的总行数。
///
/// 返回的行号是**镜像被裁剪前**的绝对序号，前端据此判断有没有漏读；
/// 长时间运行会丢掉最早的行，这在进度视图里是可接受的。
pub fn log_tail_since(from: usize) -> (usize, Vec<String>) {
    let (Ok(mirror), Ok(total)) = (LOG_MIRROR.lock(), LOG_MIRROR_TOTAL.lock()) else {
        return (from, Vec::new());
    };
    let total = *total;
    // 镜像里第一条的绝对序号。被裁掉的行数 = 总行数 - 当前镜像长度。
    let first_kept = total.saturating_sub(mirror.len());
    // 请求的位置早于镜像起点，说明中间那段已经被裁掉了：从现存最早一行给起，
    // 前端能从返回的绝对序号跳变看出漏了多少。
    let start = from.saturating_sub(first_kept).min(mirror.len());
    (total, mirror.iter().skip(start).cloned().collect())
}

/// 清空内存日志镜像。控制台每次开跑前调用，避免上一轮的输出混进来。
pub fn clear_log_mirror() {
    if let Ok(mut mirror) = LOG_MIRROR.lock() {
        mirror.clear();
    }
    if let Ok(mut total) = LOG_MIRROR_TOTAL.lock() {
        *total = 0;
    }
}

pub fn now_full() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

/// Unix 秒 → 本地时区的 `YYYY-MM-DD HH:MM:SS`。
///
/// 与 [`now_full`] 同一个格式：历史运行列表里的时间和报告里的时间要长一个样，
/// 否则用户得在两种写法之间自己换算。
pub fn format_unix_seconds(secs: u64) -> String {
    use chrono::TimeZone;
    match chrono::Local.timestamp_opt(secs as i64, 0).single() {
        Some(time) => time.format("%Y-%m-%d %H:%M:%S").to_string(),
        None => String::new(),
    }
}

pub fn now_compact() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 文件名安全化
/// 取互斥锁，并在锁被"毒化"（持锁线程 panic 展开）后继续复用里面的数据。
///
/// 本工具刻意用 `catch_unwind` 隔离单元/流线程的 panic 并继续跑完剩余测试
/// （见 `execute_unit_safely` 与 `UNIT_PANIC`）。如果某次 panic 恰好在持锁期间
/// 展开，裸 `lock().unwrap()` 会让之后每一次取锁都 panic —— 包括写报告前的
/// 最后一次，等于把整轮已经跑完的结果全部丢掉。被中断的那份数据可能不完整，
/// 但保留它永远好过丢掉整份报告。
pub fn lock_recover<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

pub fn sanitize(label: &str) -> String {
    label
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

pub fn hostname() -> String {
    if let Ok(h) = std::env::var("COMPUTERNAME") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }
    if let Ok(h) = std::env::var("HOSTNAME") {
        if !h.trim().is_empty() {
            return h.trim().to_string();
        }
    }
    let out = run_cmd("hostname", &[], Duration::from_secs(5));
    let h = out.stdout.trim().to_string();
    if h.is_empty() {
        "UNKNOWN-PC".into()
    } else {
        h
    }
}

pub fn os_name() -> String {
    if cfg!(windows) {
        "windows".into()
    } else if cfg!(target_os = "macos") {
        "macos".into()
    } else {
        "linux".into()
    }
}

#[cfg(any(windows, test))]
enum BracketVersionToken {
    Ordinary,
    Major(u32),
    Malformed,
}

/// 判断括号内的空白分隔 token 是否像 Windows dotted numeric 版本。
#[cfg(any(windows, test))]
fn classify_bracket_version_token(token: &str) -> BracketVersionToken {
    let bytes = token.as_bytes();
    let dot_count = bytes.iter().filter(|byte| **byte == b'.').count();
    let has_ascii_digit = bytes.iter().any(u8::is_ascii_digit);
    if dot_count < 2 || !has_ascii_digit {
        return BracketVersionToken::Ordinary;
    }
    if !bytes
        .iter()
        .all(|byte| byte.is_ascii_digit() || *byte == b'.')
    {
        return BracketVersionToken::Malformed;
    }

    let mut component_count = 0;
    let mut major = None;
    for component in token.split('.') {
        let Ok(value) = component.parse::<u32>() else {
            return BracketVersionToken::Malformed;
        };
        if major.is_none() {
            major = Some(value);
        }
        component_count += 1;
    }
    if component_count < 3 {
        return BracketVersionToken::Malformed;
    }
    let Some(major) = major else {
        return BracketVersionToken::Malformed;
    };
    // Windows 10/11 均报告 major 10。保留两位 major 给合理的未来版本，
    // 但拒绝 0 或 999.1.1 之类明显不是可信 `ver` 输出的值。
    if !(1..=99).contains(&major) {
        return BracketVersionToken::Malformed;
    }
    BracketVersionToken::Major(major)
}

/// 从 `cmd /D /C ver` 输出中提取唯一可信的 Windows major 版本。
///
/// 英文和中文 Windows 通常分别输出 `[Version 10.0.19045.4651]`、
/// `[版本 10.0.22631.4602]`。本地化标签不能作为锚点，因此只检查成对方括号
/// 内的空白分隔 token；方括号外的 IPv4 或三段数字完全忽略。括号不平衡、
/// token 畸形、候选超过一个（即使值相同）或 major 超出保守范围时均拒绝。
#[cfg(any(windows, test))]
pub(crate) fn windows_major_from_ver_output(output: &str) -> Option<u32> {
    let mut bracket_start = None;
    let mut major = None;

    for (index, character) in output.char_indices() {
        match character {
            '[' if bracket_start.is_some() => return None,
            '[' => bracket_start = Some(index + character.len_utf8()),
            ']' => {
                let start = bracket_start.take()?;
                let content = &output[start..index];
                for token in content.split_whitespace() {
                    match classify_bracket_version_token(token) {
                        BracketVersionToken::Ordinary => {}
                        BracketVersionToken::Major(candidate) if major.is_none() => {
                            major = Some(candidate)
                        }
                        BracketVersionToken::Major(_) | BracketVersionToken::Malformed => {
                            return None
                        }
                    }
                }
            }
            _ => {}
        }
    }

    if bracket_start.is_some() {
        return None;
    }
    major
}

// ---------------- 外部灌包工具定位 ----------------

// ---------------- 交互输入 ----------------

// ---------------- 其它 ----------------

pub fn md5_hex(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

/// 临时目录里的文件路径
#[cfg(target_os = "macos")]
pub fn temp_file(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    enum ScriptedAction {
        Emit {
            lines: Vec<String>,
            ok: bool,
            stderr: String,
        },
        Truncated {
            output: String,
        },
        RefuseReap,
    }

    struct ScriptedExecutor {
        actions: Mutex<VecDeque<ScriptedAction>>,
        kill_count: AtomicUsize,
        reap_count: AtomicUsize,
    }

    impl ScriptedExecutor {
        fn new(actions: Vec<ScriptedAction>) -> Self {
            Self {
                actions: Mutex::new(actions.into()),
                kill_count: AtomicUsize::new(0),
                reap_count: AtomicUsize::new(0),
            }
        }

        fn next(&self) -> ScriptedAction {
            self.actions
                .lock()
                .unwrap()
                .pop_front()
                .expect("scripted process action exhausted")
        }
    }

    impl ProcessExecutor for ScriptedExecutor {
        fn run(&self, _spec: &ProcessSpec, _timeout: Duration) -> CmdOut {
            match self.next() {
                ScriptedAction::Emit { lines, ok, stderr } => CmdOut {
                    ok,
                    stdout: lines.join("\n"),
                    stderr,
                    ..Default::default()
                },
                ScriptedAction::Truncated { output } => CmdOut {
                    stdout: output,
                    stderr: "输出被截断".into(),
                    ..Default::default()
                },
                ScriptedAction::RefuseReap => CmdOut {
                    stderr: "回收子进程失败: fake process refuses exit".into(),
                    ..Default::default()
                },
            }
        }

        fn run_streaming(
            &self,
            _spec: &ProcessSpec,
            _timeout: Duration,
            cancel: Option<&AtomicBool>,
            on_line: &mut dyn FnMut(&str, Instant),
        ) -> CmdOut {
            let base = Instant::now();
            match self.next() {
                ScriptedAction::Emit { lines, ok, stderr } => {
                    let mut stdout = String::new();
                    for (index, line) in lines.iter().enumerate() {
                        if index > 0 {
                            stdout.push('\n');
                        }
                        stdout.push_str(line);
                        on_line(line, base + Duration::from_millis(index as u64 * 10));
                    }
                    CmdOut {
                        ok,
                        stdout,
                        stderr,
                        ..Default::default()
                    }
                }
                ScriptedAction::Truncated { output } => CmdOut {
                    stdout: output,
                    stderr: "输出被截断".into(),
                    ..Default::default()
                },
                ScriptedAction::RefuseReap => {
                    if cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
                        self.kill_count.fetch_add(1, Ordering::SeqCst);
                        self.reap_count.fetch_add(1, Ordering::SeqCst);
                    }
                    CmdOut {
                        cancelled: cancel.is_some_and(|flag| flag.load(Ordering::SeqCst)),
                        stderr: "回收子进程失败: fake process refuses exit".into(),
                        ..Default::default()
                    }
                }
            }
        }
    }

    /// 游标必须是「产生过的总行数」，不能是被封顶的镜像长度。
    ///
    /// 用 `mirror.len()` 当游标时，一旦写满 4000 行游标就永远停在 4000，
    /// 前端每次轮询都拿到空列表，进度视图从此不再更新——而一次 120 单元的
    /// 运行会打出三万多行，这是常态不是边界。
    #[test]
    fn log_tail_cursor_keeps_counting_past_the_mirror_cap() {
        let _guard = log_mirror_test_lock();
        clear_log_mirror();
        for i in 0..LOG_MIRROR_MAX_LINES {
            logln(&format!("line-{i}"));
        }
        let (cursor, lines) = log_tail_since(0);
        assert_eq!(cursor, LOG_MIRROR_MAX_LINES);
        assert_eq!(lines.len(), LOG_MIRROR_MAX_LINES);

        // 再打 100 行：镜像会裁掉最早的 100 行，但游标必须继续往前走，
        // 而且这 100 行必须真的被取到。
        for i in 0..100 {
            logln(&format!("extra-{i}"));
        }
        let (cursor2, lines2) = log_tail_since(cursor);
        assert_eq!(cursor2, LOG_MIRROR_MAX_LINES + 100, "游标不能停在封顶值");
        assert_eq!(lines2.len(), 100, "新行必须取得到");
        assert_eq!(lines2.first().map(String::as_str), Some("extra-0"));
        assert_eq!(lines2.last().map(String::as_str), Some("extra-99"));

        // 请求的位置早于镜像起点（那一段已被裁掉）时，从现存最早一行给起，
        // 不能 panic，也不能返回空。
        let (cursor3, lines3) = log_tail_since(0);
        assert_eq!(cursor3, LOG_MIRROR_MAX_LINES + 100);
        assert_eq!(lines3.len(), LOG_MIRROR_MAX_LINES);
        assert_eq!(lines3.first().map(String::as_str), Some("line-100"));
        clear_log_mirror();
    }

    /// 日志镜像是进程级全局状态，两个用例并行跑会互相干扰。
    fn log_mirror_test_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[test]
    fn clearing_the_mirror_resets_the_cursor() {
        let _guard = log_mirror_test_lock();
        clear_log_mirror();
        logln("a");
        logln("b");
        assert_eq!(log_tail_since(0).0, 2);
        clear_log_mirror();
        assert_eq!(log_tail_since(0), (0, Vec::new()));
    }

    #[test]
    fn test_sanitize() {
        assert_eq!(sanitize("a b/c:d"), "a_b_c_d");
    }

    #[test]
    fn test_run_cmd_basic() {
        let out = run_cmd(
            if cfg!(windows) { "cmd" } else { "sh" },
            if cfg!(windows) {
                &["/C", "echo hi"]
            } else {
                &["-c", "echo hi"]
            },
            Duration::from_secs(10),
        );
        assert!(out.ok);
        assert!(out.stdout.contains("hi"));
    }

    #[test]
    fn process_executor_keeps_system_wrapper_and_scripted_boundary_separate() {
        let spec = ProcessSpec::new("echo", &["hello"]);
        assert_eq!(spec.program, "echo");
        assert_eq!(spec.args, vec!["hello"]);

        let fake = ScriptedExecutor::new(vec![ScriptedAction::Emit {
            lines: vec!["first event".into(), "second event".into()],
            ok: true,
            stderr: String::new(),
        }]);
        let mut observed = Vec::new();
        let out = run_streaming_controlled_timed_with(
            &fake,
            "fake-tool",
            &[],
            Duration::from_secs(30),
            None,
            |line, at| observed.push((line.to_string(), at)),
        );
        assert!(out.ok);
        assert_eq!(observed.len(), 2);
        assert_eq!(observed[0].0, "first event");
        assert!(observed[1].1 >= observed[0].1);
    }

    #[test]
    fn scripted_process_can_model_truncation_and_refused_reap_without_a_child() {
        let truncated = ScriptedExecutor::new(vec![ScriptedAction::Truncated {
            output: "partial summary".into(),
        }]);
        let out = run_streaming_controlled_timed_with(
            &truncated,
            "fake-tool",
            &[],
            Duration::from_secs(1),
            None,
            |_line, _at| {},
        );
        assert!(!out.ok);
        assert!(out.stdout.contains("partial summary"));
        assert!(out.cleanup_confirmed(), "截断输出本身不等于回收失败");

        let cancel = AtomicBool::new(true);
        let refusing = ScriptedExecutor::new(vec![ScriptedAction::RefuseReap]);
        let out = run_streaming_controlled_timed_with(
            &refusing,
            "fake-tool",
            &[],
            Duration::from_secs(1),
            Some(&cancel),
            |_line, _at| {},
        );
        assert!(out.cancelled);
        assert!(!out.cleanup_confirmed());
        assert_eq!(refusing.kill_count.load(Ordering::SeqCst), 1);
        assert_eq!(refusing.reap_count.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn streaming_command_rejects_unrepresentable_deadline_before_spawn() {
        let out = run_streaming_controlled(
            "this-program-must-not-be-spawned",
            &[],
            Duration::MAX,
            None,
            |_| {},
        );
        assert!(!out.ok);
        assert!(out.stderr.contains("超时时间过大"));
        assert!(!out.stderr.contains("启动命令失败"));
        assert!(!out.process_started());
        assert!(out.cleanup_confirmed());
    }

    #[cfg(unix)]
    #[test]
    fn streaming_natural_exit_wins_over_late_cancel_while_descendant_holds_pipe() {
        let cancel = std::sync::Arc::new(AtomicBool::new(false));
        let setter = std::sync::Arc::clone(&cancel);
        let cancel_thread = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            setter.store(true, Ordering::SeqCst);
        });

        // 父 shell 立即以 code 7 自然退出；后台 sleep 只负责继续持有继承的
        // stdout pipe，使旧实现无法靠 channel disconnect 及时发现父进程退出。
        let out = run_streaming_controlled(
            "sh",
            &["-c", "(sleep 0.3) & exit 7"],
            Duration::from_secs(2),
            Some(cancel.as_ref()),
            |_| {},
        );
        cancel_thread.join().unwrap();

        assert!(!out.ok);
        assert!(!out.timed_out);
        assert!(!out.cancelled, "stop/cancel 不能覆盖已经发生的自然异常退出");
        assert!(out.process_started());
        assert!(out.cleanup_confirmed());
    }

    #[test]
    fn streaming_timed_callback_preserves_reader_time_for_queued_tail_lines() {
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            (
                "cmd",
                vec![
                    "/C",
                    "(for /L %i in (1,1,256) do @echo line%i) & echo marker",
                ],
            )
        } else {
            (
                "sh",
                vec![
                    "-c",
                    "i=1; while [ $i -le 256 ]; do printf 'line%s\\n' \"$i\"; i=$((i+1)); done; printf 'marker\\n'",
                ],
            )
        };
        let mut line_count = 0usize;
        let mut marker_queue_time = None;
        let out = run_streaming_controlled_timed(
            program,
            &args,
            Duration::from_secs(10),
            None,
            |line, observed_at| {
                if line.starts_with("line") {
                    line_count += 1;
                    if line_count == 1 {
                        // reader 继续把尾部行送入无界 channel，控制线程则停在回调中。
                        std::thread::sleep(Duration::from_millis(200));
                    }
                } else if line == "marker" {
                    marker_queue_time = Some(Instant::now().saturating_duration_since(observed_at));
                }
            },
        );

        assert!(out.ok, "helper command failed: {}", out.stderr);
        assert_eq!(line_count, 256);
        assert!(
            marker_queue_time.is_some_and(|delay| delay >= Duration::from_millis(100)),
            "marker 必须保留 reader 入队时间，而不是晚到的 drain 回调时间: {marker_queue_time:?}"
        );
    }

    #[test]
    fn command_lifecycle_helpers_distinguish_spawn_and_reap_failures() {
        let spawn_failed = CmdOut {
            stderr: "启动命令失败: ctsTraffic.exe (not found)".into(),
            ..Default::default()
        };
        assert!(!spawn_failed.process_started());
        assert!(spawn_failed.cleanup_confirmed());

        let reap_failed = CmdOut {
            stderr: "超时后回收子进程失败: synthetic".into(),
            ..Default::default()
        };
        assert!(reap_failed.process_started());
        assert!(!reap_failed.cleanup_confirmed());
    }

    #[test]
    fn streaming_callback_panic_reaps_child_before_resuming_unwind() {
        let (program, args): (&str, Vec<&str>) = if cfg!(windows) {
            ("ping", vec!["-n", "60", "127.0.0.1"])
        } else {
            ("sh", vec!["-c", "printf 'ready\\n'; exec sleep 60"])
        };
        let started = Instant::now();
        let result = catch_unwind(AssertUnwindSafe(|| {
            run_streaming_controlled(program, &args, Duration::from_secs(30), None, |_| {
                panic!("synthetic streaming callback panic")
            })
        }));
        assert!(result.is_err());
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "回调 panic 后必须立即终止并回收 60 秒 helper，而不是等待自然退出"
        );
    }

    /// 子进程由另一个测试进程托管；这个 helper 只在父死亡集成测试的子进程中运行。
    #[cfg(target_os = "linux")]
    #[test]
    fn helper_spawns_a_parent_bound_child() {
        if std::env::var("CPE_TEST_PDEATH_HELPER").as_deref() != Ok("1") {
            return;
        }
        let marker = std::env::var("CPE_TEST_PDEATH_MARKER").expect("pdeath marker");
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec sleep 60"])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        configure_managed_command(&mut command);
        let child = command.spawn().expect("受管 helper child 必须能启动");
        std::fs::write(&marker, child.id().to_string()).expect("写 pdeath marker");
        std::thread::sleep(Duration::from_secs(60));
    }

    /// Linux agent 被 SIGKILL 时，已启动的外部工具不能留下来继续占端口。
    /// 这里不依赖 iperf3，用一个真实的 `sleep` 子进程验证同一父死亡边界。
    #[cfg(target_os = "linux")]
    #[test]
    fn managed_child_dies_when_its_parent_process_is_killed() {
        let marker =
            std::env::temp_dir().join(format!("cpe_test_pdeath_{}_marker", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut parent = Command::new(std::env::current_exe().unwrap());
        parent
            .args([
                "--exact",
                "util::tests::helper_spawns_a_parent_bound_child",
                "--nocapture",
            ])
            .env("CPE_TEST_PDEATH_HELPER", "1")
            .env("CPE_TEST_PDEATH_MARKER", &marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let mut parent = parent.spawn().expect("pdeath parent helper 必须能启动");

        let deadline = Instant::now() + Duration::from_secs(3);
        let child_pid = loop {
            if let Ok(text) = std::fs::read_to_string(&marker) {
                if let Ok(pid) = text.trim().parse::<libc::pid_t>() {
                    break pid;
                }
            }
            assert!(Instant::now() < deadline, "受管 child 未及时启动");
            std::thread::sleep(Duration::from_millis(10));
        };

        let result = unsafe { libc::kill(parent.id() as libc::pid_t, libc::SIGKILL) };
        assert_eq!(result, 0, "必须能强杀 pdeath parent helper");
        let _ = parent.wait();

        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            let alive = unsafe { libc::kill(child_pid, 0) } == 0;
            if !alive {
                break;
            }
            assert!(Instant::now() < deadline, "父进程死亡后 child 仍然存活");
            std::thread::sleep(Duration::from_millis(10));
        }
        let _ = std::fs::remove_file(marker);
    }
}
