//! 公共工具：子进程执行(带超时/GBK解码)、日志、时间、iperf3 定位等

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::panic::{catch_unwind, resume_unwind, AssertUnwindSafe};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use wait_timeout::ChildExt;

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

/// 执行命令，等待结束（超时强杀），返回解码后的输出
pub fn run_cmd(prog: &str, args: &[&str], timeout: Duration) -> CmdOut {
    let mut c = Command::new(prog);
    c.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
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
            let cleanup_errors = terminate_and_reap(&mut child);
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
            let cleanup_errors = terminate_and_reap(&mut child);
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

/// 执行命令并逐行回调；cancel=true 时主动终止子进程。
/// 异步 agent job 和主控本地 job 共用这一实现，避免 HTTP handler
/// 被长时间 iperf3 进程占住。
pub fn run_streaming_controlled<F: FnMut(&str)>(
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
    let so = child.stdout.take().expect("stdout piped");
    let se = child.stderr.take().expect("stderr piped");
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    let th_o = match std::thread::Builder::new()
        .name("streaming-command-stdout".into())
        .spawn(move || {
            let mut r = BufReader::new(so);
            loop {
                let mut line = Vec::new();
                match std::io::BufRead::read_until(&mut r, b'\n', &mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        if tx.send(line).is_err() {
                            break;
                        }
                    }
                }
            }
        }) {
        Ok(handle) => handle,
        Err(error) => {
            let cleanup_errors = terminate_and_reap(&mut child);
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
            let cleanup_errors = terminate_and_reap(&mut child);
            let _ = th_o.join();
            let mut stdout = String::new();
            while let Ok(bytes) = rx.try_recv() {
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
            Ok(bytes) => {
                let s = decode_bytes(&bytes);
                collected.push_str(&s);
                if let Err(payload) = catch_unwind(AssertUnwindSafe(|| on_line(s.trim_end()))) {
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
    while let Ok(bytes) = rx.try_recv() {
        let s = decode_bytes(&bytes);
        collected.push_str(&s);
        if callback_panic.is_none() {
            if let Err(payload) = catch_unwind(AssertUnwindSafe(|| on_line(s.trim_end()))) {
                callback_panic = Some(payload);
            }
        }
    }
    let ok = status
        .map(|status| status.success() && !timed_out && !cancelled)
        .unwrap_or(false);
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

// ---------------- 日志 ----------------

static LOG_FILE: OnceLock<Mutex<File>> = OnceLock::new();

/// 主控模式下开启文件日志（控制台 + 文件双写）
pub fn log_to_file(path: &Path) {
    if let Ok(f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = LOG_FILE.set(Mutex::new(f));
    }
}

/// 打印并写日志文件
pub fn logln(s: &str) {
    println!("{s}");
    if let Some(m) = LOG_FILE.get() {
        if let Ok(mut f) = m.lock() {
            let _ = writeln!(f, "{s}");
        }
    }
}

pub fn now_full() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
}

pub fn now_compact() -> String {
    chrono::Local::now().format("%Y%m%d_%H%M%S").to_string()
}

pub fn now_hms() -> String {
    chrono::Local::now().format("%H:%M:%S").to_string()
}

/// 文件名安全化
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
fn windows_major_from_ver_output(output: &str) -> Option<u32> {
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

#[cfg(any(windows, test))]
fn windows_ver_supports_ctstraffic(output: &str) -> bool {
    windows_major_from_ver_output(output).is_some_and(|major| major >= 10)
}

#[cfg(windows)]
fn detect_ctstraffic_platform_support() -> bool {
    // /D 禁止 AutoRun 注册表脚本，避免额外输出或命令替换影响版本门槛。
    let out = run_cmd("cmd", &["/D", "/C", "ver"], Duration::from_secs(5));
    out.ok && !out.timed_out && !out.cancelled && windows_ver_supports_ctstraffic(&out.merged())
}

#[cfg(not(windows))]
fn detect_ctstraffic_platform_support() -> bool {
    false
}

/// ctsTraffic 平台门槛：仅真实系统版本为 Windows 10 或更高时返回 true。
///
/// 不能只看 Rust 编译目标：Windows 7/8 同样会满足 `cfg!(windows)`。版本命令
/// 执行失败或输出无法可靠解析时采取 fail-closed 策略，避免声明能力或启动 CTS。
pub fn ctstraffic_platform_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(detect_ctstraffic_platform_support)
}

// ---------------- 外部灌包工具定位 ----------------

static IPERF3: OnceLock<Option<String>> = OnceLock::new();

fn iperf_probe_succeeded(probe: &CmdOut) -> bool {
    probe.ok && !probe.timed_out && !probe.cancelled
}

/// 找 iperf3：优先程序同目录，其次 PATH
pub fn find_iperf3() -> Option<String> {
    IPERF3
        .get_or_init(|| {
            let fname = if cfg!(windows) {
                "iperf3.exe"
            } else {
                "iperf3"
            };
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    let p = dir.join(fname);
                    if p.exists() {
                        return Some(p.to_string_lossy().into_owned());
                    }
                }
            }
            let probe = run_cmd("iperf3", &["--version"], Duration::from_secs(8));
            // “启动命令失败: iperf3 ...”本身也包含 iperf，不能只靠文字
            // 命中判断存在；只有 --version 真正成功退出才算可执行。
            if iperf_probe_succeeded(&probe) {
                Some("iperf3".into())
            } else {
                None
            }
        })
        .clone()
}

pub fn iperf3_version() -> Option<String> {
    let bin = find_iperf3()?;
    let out = run_cmd(&bin, &["--version"], Duration::from_secs(8));
    out.merged().lines().next().map(|s| s.trim().to_string())
}

static CTS_TRAFFIC: OnceLock<Option<String>> = OnceLock::new();

/// 找 ctsTraffic：仅 Windows 支持；优先程序同目录，其次 PATH。
pub fn find_ctstraffic() -> Option<String> {
    CTS_TRAFFIC
        .get_or_init(|| {
            if !ctstraffic_platform_supported() {
                return None;
            }
            if let Ok(exe) = std::env::current_exe() {
                if let Some(dir) = exe.parent() {
                    let p = dir.join("ctsTraffic.exe");
                    if p.exists() {
                        return Some(p.to_string_lossy().into_owned());
                    }
                }
            }
            // ctsTraffic 的 -Help 会打印帮助后返回非零，因此不能按退出码探测；
            // 只要进程确实启动且输出了官方帮助标识，即可确认 PATH 中可用。
            let probe = run_cmd("ctsTraffic.exe", &["-Help"], Duration::from_secs(8));
            let text = probe.merged().to_ascii_lowercase();
            (!text.contains("启动命令失败") && text.contains("ctstraffic"))
                .then(|| "ctsTraffic.exe".into())
        })
        .clone()
}

pub fn ctstraffic_version() -> Option<String> {
    let bin = find_ctstraffic()?;
    // 官方 CLI 当前没有独立 --version；健康检查报告可执行文件位置和可用性，
    // 精确文件版本可在 Windows 文件属性中查看。
    Some(format!("ctsTraffic 可用 ({bin})"))
}

// ---------------- 交互输入 ----------------

/// 读一行（EOF 返回 None，用于 --auto/管道场景不卡死）
pub fn read_line_trim() -> Option<String> {
    let mut s = String::new();
    match std::io::stdin().read_line(&mut s) {
        Ok(0) => None,
        Ok(_) => Some(s.trim().to_string()),
        Err(_) => None,
    }
}

pub fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    read_line_trim().unwrap_or_default()
}

// ---------------- 其它 ----------------

/// 用系统默认程序打开文件（报告自动打开）
pub fn open_path(p: &Path) {
    let s = p.to_string_lossy().into_owned();
    if cfg!(windows) {
        let _ = Command::new("cmd").args(["/C", "start", "", &s]).spawn();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg(&s).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(&s).spawn();
    }
}

pub fn md5_hex(s: &str) -> String {
    format!("{:x}", md5::compute(s.as_bytes()))
}

/// 临时目录里的文件路径
#[cfg(target_os = "macos")]
pub fn temp_file(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(name)
}

/// 解析 "1-5,8,10" 之类的序号（1 起），空串 => 全部
pub fn parse_selection(input: &str, max: usize) -> Result<Vec<usize>, String> {
    let t = input.trim();
    if t.is_empty() {
        return Ok((1..=max).collect());
    }
    let mut out: Vec<usize> = Vec::new();
    for part in t.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Some((a, b)) = p.split_once('-') {
            let a: usize = a.trim().parse().map_err(|_| format!("无效序号: {p}"))?;
            let b: usize = b.trim().parse().map_err(|_| format!("无效序号: {p}"))?;
            if a == 0 || b == 0 || a > b || b > max {
                return Err(format!("序号超出范围(1-{max}): {p}"));
            }
            for i in a..=b {
                if !out.contains(&i) {
                    out.push(i);
                }
            }
        } else {
            let i: usize = p.parse().map_err(|_| format!("无效序号: {p}"))?;
            if i == 0 || i > max {
                return Err(format!("序号超出范围(1-{max}): {p}"));
            }
            if !out.contains(&i) {
                out.push(i);
            }
        }
    }
    if out.is_empty() {
        return Ok((1..=max).collect());
    }
    Ok(out)
}

/// 判断两个 IPv4 是否同 /24
pub fn same_slash24(a: &str, b: &str) -> bool {
    let pa: Vec<&str> = a.split('.').collect();
    let pb: Vec<&str> = b.split('.').collect();
    pa.len() == 4 && pb.len() == 4 && pa[..3] == pb[..3]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_selection() {
        assert_eq!(parse_selection("", 5).unwrap(), vec![1, 2, 3, 4, 5]);
        assert_eq!(parse_selection("1-3,5", 5).unwrap(), vec![1, 2, 3, 5]);
        assert_eq!(parse_selection("2", 5).unwrap(), vec![2]);
        assert!(parse_selection("6", 5).is_err());
        assert!(parse_selection("0", 5).is_err());
        assert!(parse_selection("abc", 5).is_err());
    }

    #[test]
    fn test_same_slash24() {
        assert!(same_slash24("192.168.1.2", "192.168.1.200"));
        assert!(!same_slash24("192.168.1.2", "192.168.2.2"));
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
    fn missing_iperf_error_text_is_not_a_successful_probe() {
        let missing = CmdOut {
            ok: false,
            stderr: "启动命令失败: iperf3 (No such file or directory)".into(),
            ..Default::default()
        };
        assert!(
            !iperf_probe_succeeded(&missing),
            "错误文本包含 iperf 也不能视为探测成功"
        );

        let found = CmdOut {
            ok: true,
            stdout: "iperf 3.18".into(),
            ..Default::default()
        };
        assert!(iperf_probe_succeeded(&found));
    }

    #[test]
    fn windows_ver_parser_accepts_windows_10_and_11() {
        assert!(windows_ver_supports_ctstraffic(
            "Microsoft Windows [Version 10.0.19045.4651]"
        ));
        assert!(windows_ver_supports_ctstraffic(
            "Microsoft Windows [版本 10.0.22631.4602]"
        ));
        assert_eq!(
            windows_major_from_ver_output("\r\nMicrosoft Windows [Version 10.0.26100.2894]\r\n"),
            Some(10)
        );
    }

    #[test]
    fn windows_ver_parser_ignores_untrusted_numbers_outside_brackets() {
        let output = concat!(
            "Copyright 2026 AutoRun probe 10.0.0.1 1.2.3 999.1.1\r\n",
            "Microsoft Windows [Version 10.0.26100.2894]\r\n",
            "trailing 6.1.7601"
        );
        assert_eq!(windows_major_from_ver_output(output), Some(10));
        assert!(windows_ver_supports_ctstraffic(output));

        let windows_7 = concat!(
            "AutoRun probe 192.168.1.1 10.0.0 88.77.66\r\n",
            "Microsoft Windows [Version 6.1.7601]\r\n"
        );
        assert_eq!(windows_major_from_ver_output(windows_7), Some(6));
        assert!(!windows_ver_supports_ctstraffic(windows_7));
    }

    #[test]
    fn windows_ver_parser_allows_conservative_future_major_versions() {
        assert_eq!(
            windows_major_from_ver_output("Microsoft Windows [Version 11.0.100]"),
            Some(11)
        );
        assert!(windows_ver_supports_ctstraffic(
            "Microsoft Windows [Version 99.1.2.3.4]"
        ));
    }

    #[test]
    fn windows_ver_parser_rejects_windows_7_and_8() {
        assert!(!windows_ver_supports_ctstraffic(
            "Microsoft Windows [Version 6.1.7601]"
        ));
        assert!(!windows_ver_supports_ctstraffic(
            "Microsoft Windows [Version 6.2.9200]"
        ));
        assert!(!windows_ver_supports_ctstraffic(
            "Microsoft Windows [Version 6.3.9600]"
        ));
    }

    #[test]
    fn windows_ver_parser_fails_closed_for_malformed_output() {
        for output in [
            "",
            "Microsoft Windows",
            "Microsoft Windows Version 10.0.19045.4651",
            "Microsoft Windows [Version unknown]",
            "Microsoft Windows [Version 10]",
            "Microsoft Windows [Version 10.0]",
            "Microsoft Windows [Version 10..19045]",
            "Microsoft Windows [Version .10.0.19045]",
            "Microsoft Windows [Version 10.0.19045.]",
            "Microsoft Windows [Version 10.0.x]",
            "Microsoft Windows [Version v10.0.19045]",
            "Microsoft Windows [Version 10.0.19045-beta]",
            "Microsoft Windows [Version 999.1.1]",
            "Microsoft Windows [Version 0.1.2]",
            "Microsoft Windows [Version 10.0.19045",
            "Microsoft Windows Version 10.0.19045]",
            "Microsoft Windows [[Version 10.0.19045]]",
            "Copyright 2026 Microsoft Corporation",
        ] {
            assert!(
                !windows_ver_supports_ctstraffic(output),
                "malformed output must be rejected: {output:?}"
            );
        }
    }

    #[test]
    fn windows_ver_parser_rejects_multiple_or_conflicting_bracket_candidates() {
        for output in [
            "Microsoft Windows [Version 10.0.19045 10.0.19045]",
            "Microsoft Windows [Version 6.1.7601 10.0.19045]",
            "Microsoft Windows [Version 10.0.19045] [Build 10.0.19045]",
            "Microsoft Windows [Version 10.0.19045 10..19045]",
            "probe [10.0.0.1] Microsoft Windows [Version 10.0.19045]",
        ] {
            assert_eq!(
                windows_major_from_ver_output(output),
                None,
                "ambiguous output must be rejected: {output:?}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn non_windows_platform_never_supports_ctstraffic() {
        assert!(!ctstraffic_platform_supported());
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
}
