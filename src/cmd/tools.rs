//! 外部流量工具的定位与版本探测。
//!
//! iperf3 与 ctsTraffic 都是随发布包一起分发的外置二进制，不是本程序的一部分。
//! "在哪里找、找到没有、版本能不能用"这三件事横跨 agent、主控 UI 和执行器，
//! 但都属于**外部命令域**，因此放在 `cmd` 下而不是通用工具箱里。
//!
//! 前置检查的错误提示要能直接指导用户（"请把 iperf3 放到程序同目录"），
//! 所以这里的返回值刻意区分"没找到"与"找到但平台不支持"。

use crate::util::{run_cmd, CmdOut};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

#[cfg(any(windows, test))]
fn windows_ver_supports_ctstraffic(output: &str) -> bool {
    crate::util::windows_major_from_ver_output(output).is_some_and(|major| major >= 10)
}

/// iperf3 的查找结果带**过期时间**，不是一次性缓存。
///
/// agent 和控制台都是常驻进程，而状态页上「iperf3 未找到」是一条会每分钟
/// 刷新一次、看起来实时的告警。用 `OnceLock` 的话，这条完全正常的排障流程
/// 走不通：页面说缺 → 用户把 iperf3.exe 拷到程序同目录 → 页面永远还是说缺，
/// 只能重启 agent，而页面上没有任何地方提示要重启。
///
/// TTL 取 30 秒：足够挡住每 1.5 秒一次的活动轮询把 `--version` 打满，
/// 又短到「拷完文件回头看一眼」就能看到变化。
static IPERF3: Mutex<Option<(Instant, Option<String>)>> = Mutex::new(None);

const TOOL_LOOKUP_TTL: Duration = Duration::from_secs(30);

/// 读一份带 TTL 的查找缓存；过期或没有就现查一次。
fn cached_lookup(
    cache: &Mutex<Option<(Instant, Option<String>)>>,
    lookup: impl FnOnce() -> Option<String>,
) -> Option<String> {
    let mut slot = cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some((at, value)) = slot.as_ref() {
        if at.elapsed() < TOOL_LOOKUP_TTL {
            return value.clone();
        }
    }
    let found = lookup();
    *slot = Some((Instant::now(), found.clone()));
    found
}

fn iperf_probe_succeeded(probe: &CmdOut) -> bool {
    probe.ok && !probe.timed_out && !probe.cancelled
}

static CTS_TRAFFIC: OnceLock<Option<String>> = OnceLock::new();

/// 找 iperf3：优先程序同目录，其次 PATH
pub fn find_iperf3() -> Option<String> {
    cached_lookup(&IPERF3, || {
        {
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
        }
    })
}

pub fn iperf3_version() -> Option<String> {
    let bin = find_iperf3()?;
    let out = run_cmd(&bin, &["--version"], Duration::from_secs(8));
    out.merged().lines().next().map(|s| s.trim().to_string())
}

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

/// ctsTraffic 平台门槛：仅真实系统版本为 Windows 10 或更高时返回 true。
///
/// 不能只看 Rust 编译目标：Windows 7/8 同样会满足 `cfg!(windows)`。版本命令
/// 执行失败或输出无法可靠解析时采取 fail-closed 策略，避免声明能力或启动 CTS。
pub fn ctstraffic_platform_supported() -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(detect_ctstraffic_platform_support)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::util::windows_major_from_ver_output;

    #[cfg(not(windows))]
    #[test]
    fn non_windows_platform_never_supports_ctstraffic() {
        assert!(!ctstraffic_platform_supported());
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
}
