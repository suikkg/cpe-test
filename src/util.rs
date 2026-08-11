//! 公共工具：子进程执行(带超时/GBK解码)、日志、时间、iperf3 定位等

use std::fs::{File, OpenOptions};
use std::io::{BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
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
}

fn spawn_piped(prog: &str, args: &[&str]) -> Result<std::process::Child, CmdOut> {
    let mut command = Command::new(prog);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command.spawn().map_err(|e| CmdOut {
        stderr: format!("启动命令失败: {prog} ({e})"),
        ..Default::default()
    })
}

fn read_all<R: Read + Send + 'static>(mut reader: R) -> std::thread::JoinHandle<Vec<u8>> {
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let _ = reader.read_to_end(&mut bytes);
        bytes
    })
}

/// 执行命令，等待结束（超时强杀），返回解码后的输出
pub fn run_cmd(prog: &str, args: &[&str], timeout: Duration) -> CmdOut {
    let mut child = match spawn_piped(prog, args) {
        Ok(child) => child,
        Err(out) => return out,
    };
    let th_o = read_all(child.stdout.take().expect("stdout piped"));
    let th_e = read_all(child.stderr.take().expect("stderr piped"));
    let (ok, timed_out) = match child.wait_timeout(timeout) {
        Ok(Some(st)) => (st.success(), false),
        Ok(None) => {
            let _ = child.kill();
            let _ = child.wait();
            (false, true)
        }
        Err(_) => (false, false),
    };
    let stdout = decode_bytes(&th_o.join().unwrap_or_default());
    let stderr = decode_bytes(&th_e.join().unwrap_or_default());
    CmdOut {
        ok,
        timed_out,
        stdout,
        stderr,
    }
}

/// 执行命令并逐行回调 stdout（实时速率显示用），超时强杀
pub fn run_streaming<F: FnMut(&str)>(
    prog: &str,
    args: &[&str],
    timeout: Duration,
    mut on_line: F,
) -> CmdOut {
    let mut child = match spawn_piped(prog, args) {
        Ok(child) => child,
        Err(out) => return out,
    };
    let so = child.stdout.take().expect("stdout piped");
    let th_e = read_all(child.stderr.take().expect("stderr piped"));
    let (tx, rx) = mpsc::channel::<Vec<u8>>();
    std::thread::spawn(move || {
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
    });

    let deadline = Instant::now() + timeout;
    let mut collected = String::new();
    let mut timed_out = false;
    loop {
        let now = Instant::now();
        if now >= deadline {
            timed_out = true;
            let _ = child.kill();
            break;
        }
        let wait = std::cmp::min(deadline - now, Duration::from_millis(500));
        match rx.recv_timeout(wait) {
            Ok(bytes) => {
                let s = decode_bytes(&bytes);
                on_line(s.trim_end());
                collected.push_str(&s);
            }
            Err(mpsc::RecvTimeoutError::Timeout) => continue,
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
    // 清掉残余队列
    while let Ok(bytes) = rx.try_recv() {
        let s = decode_bytes(&bytes);
        on_line(s.trim_end());
        collected.push_str(&s);
    }
    let ok = match child.wait_timeout(Duration::from_secs(5)) {
        Ok(Some(st)) => st.success() && !timed_out,
        _ => {
            let _ = child.kill();
            let _ = child.wait();
            false
        }
    };
    let stderr = decode_bytes(&th_e.join().unwrap_or_default());
    CmdOut {
        ok,
        timed_out,
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

// ---------------- iperf3 定位 ----------------

static IPERF3: OnceLock<Option<String>> = OnceLock::new();

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
            if probe.ok || probe.merged().to_lowercase().contains("iperf") {
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

// ---------------- 交互输入 ----------------

pub fn ask(prompt: &str) -> String {
    print!("{prompt}");
    let _ = std::io::stdout().flush();
    let mut input = String::new();
    match std::io::stdin().read_line(&mut input) {
        Ok(0) | Err(_) => String::new(),
        Ok(_) => input.trim().to_string(),
    }
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
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn temp_file(name: &str) -> PathBuf {
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
        let (start, end) = if let Some((a, b)) = p.split_once('-') {
            (
                a.trim().parse().map_err(|_| format!("无效序号: {p}"))?,
                b.trim().parse().map_err(|_| format!("无效序号: {p}"))?,
            )
        } else {
            let i: usize = p.parse().map_err(|_| format!("无效序号: {p}"))?;
            (i, i)
        };
        if start == 0 || start > end || end > max {
            return Err(format!("序号超出范围(1-{max}): {p}"));
        }
        for i in start..=end {
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
        assert_eq!(parse_selection(" 1,1-3,,2 ", 5).unwrap(), vec![1, 2, 3]);
        assert_eq!(parse_selection("2", 5).unwrap(), vec![2]);
        assert!(parse_selection("6", 5).is_err());
        assert!(parse_selection("0", 5).is_err());
        assert!(parse_selection("3-2", 5).is_err());
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
    fn test_run_cmd_spawn_error() {
        let out = run_cmd(
            "cpe-test-command-that-does-not-exist",
            &[],
            Duration::from_secs(1),
        );
        assert!(!out.ok);
        assert!(!out.timed_out);
        assert!(out.stderr.starts_with("启动命令失败:"));
    }

    #[cfg(not(windows))]
    #[test]
    fn test_streaming_keeps_unterminated_output_and_stderr() {
        let mut lines = Vec::new();
        let out = run_streaming(
            "sh",
            &["-c", "printf 'one\\ntwo'; printf 'err' >&2"],
            Duration::from_secs(5),
            |line| lines.push(line.to_string()),
        );
        assert!(out.ok);
        assert_eq!(out.stdout, "one\ntwo");
        assert_eq!(out.stderr, "err");
        assert_eq!(lines, ["one", "two"]);
    }
}
