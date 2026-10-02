//! iperf3 命令行构造，以及 `extra` 不许覆盖的受控参数。

use crate::protocol::{IperfClientReq, IperfServerStartReq};
use crate::util::{run_cmd_with_executor, ProcessExecutor, SystemProcessExecutor};
use std::sync::OnceLock;
use std::time::Duration;

pub fn server_args(req: &IperfServerStartReq) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-s".into(),
        "-B".into(),
        req.bind_ip.clone(),
        "-p".into(),
        req.port.to_string(),
        "-i".into(),
        "1".into(),
        "-f".into(),
        "m".into(),
    ];
    a.push(if req.v6 { "-6".into() } else { "-4".into() });
    a
}

/// `extra` 里不许出现的 iperf3 参数，以及它们为什么是承重的。
///
/// `client_args` 把 `extra` 原样接在自己拼好的参数后面，而 iperf3 对重复参数是
/// **后者覆盖前者**——也就是说 `extra` 能悄悄改掉下面这些位，而调用方拿到的
/// 输出看起来一切正常：
///
/// - `-f`：解析器按 `-f m` 的输出形状读速率。换成 `-f M`（Byte）会走进另一条
///   进制分支，`-f k`/`-f g` 则直接换了量级。
/// - `-t`：执行侧的有效窗口、覆盖率门槛、`est_secs` 全按下发的 duration 算。
/// - `-i`：1 秒一行是逐样本时间线（`raws`、截图对齐）的前提。
/// - `-p` / `-B`：端口与绑定地址是资源租约和端点身份的一部分，改了它们，
///   agent 侧记的那份 owner/lease 就对不上真实进程了。
/// - `-c` / `-u` / `-4` / `-6`：直接改的是「这次测的到底是什么」。
///
/// 今天没有任何调用方会送这些进来（`master/builder.rs` 只从有类型的配置字段拼出
/// `-w`/`-P`/`-b`/`-l`，配置里没有原样透传参数的口子），所以这条约束是**免费**的；
/// 它挡的是以后有人加一个「自定义参数」输入框，或者直接调 agent 的
/// `/iperf/client` 接口时，把测量口径改掉却毫无提示。照 ADR-10 的先例记为协议
/// 不变量。详见 .ai/DESIGN-v6.0-architecture.md §4.3 R-a。
pub const RESERVED_CLIENT_FLAGS: &[(&str, &str)] = &[
    ("-c", "--client"),
    ("-B", "--bind"),
    ("-p", "--port"),
    ("-t", "--time"),
    ("-i", "--interval"),
    ("-f", "--format"),
    ("-u", "--udp"),
    ("-4", "--version4"),
    ("-6", "--version6"),
];

/// 找出 `extra` 里踩了 [`RESERVED_CLIENT_FLAGS`] 的参数。
///
/// 三种写法都要认，否则黑名单只是个摆设：分开写（`-t 5`）、粘着写（`-t5`）、
/// 长参数带等号（`--time=5`）。大小写不折叠——iperf3 的 `-b`（速率）和 `-B`
/// （绑定地址）是两个不同的参数，折叠了会把合法的 `-b` 一起挡掉。
pub fn reserved_flags_in_extra(extra: &[String]) -> Vec<String> {
    let mut hits = Vec::new();
    for arg in extra {
        let arg = arg.trim();
        for (short, long) in RESERVED_CLIENT_FLAGS {
            let glued_short = arg.len() > short.len()
                && arg.starts_with(short)
                && !arg[short.len()..].starts_with('-');
            let hit = arg == *short
                || arg == *long
                || glued_short
                || arg.starts_with(&format!("{long}="));
            if hit {
                hits.push(format!("{arg}（覆盖了 {short}/{long}）"));
                break;
            }
        }
    }
    hits
}

/// 请求里的 `extra` 是否安全。不安全时给出可以直接回给调用方的报错。
pub fn check_client_extra(req: &IperfClientReq) -> Result<(), String> {
    let hits = reserved_flags_in_extra(&req.extra);
    if hits.is_empty() {
        return Ok(());
    }
    Err(format!(
        "iperf3 client 的 extra 覆盖了受控参数：{}。\
         这些位决定了输出格式、时长、采样间隔与端点身份，测量口径要靠它们；\
         请改用请求里的 duration/port/bind_ip/udp/v6 字段，不要从 extra 绕过去。",
        hits.join("、")
    ))
}

pub fn client_args(req: &IperfClientReq) -> Vec<String> {
    let mut a: Vec<String> = vec![
        "-c".into(),
        req.dst.clone(),
        "-B".into(),
        req.bind_ip.clone(),
        "-p".into(),
        req.port.to_string(),
        "-t".into(),
        req.duration.to_string(),
        "-i".into(),
        "1".into(),
        "-f".into(),
        "m".into(),
    ];
    a.push(if req.v6 { "-6".into() } else { "-4".into() });
    if req.udp {
        a.push("-u".into());
    }
    a.extend(req.extra.iter().cloned());
    a
}

pub(super) fn cmdline(bin: &str, args: &[String]) -> String {
    format!("{} {}", bin, args.join(" "))
}

pub(super) fn supports_forceflush(bin: &str) -> bool {
    static SUPPORTED: OnceLock<bool> = OnceLock::new();
    *SUPPORTED.get_or_init(|| supports_forceflush_with(&SystemProcessExecutor, bin))
}

pub(crate) fn supports_forceflush_with<E: ProcessExecutor + ?Sized>(
    executor: &E,
    bin: &str,
) -> bool {
    run_cmd_with_executor(executor, bin, &["--help"], Duration::from_secs(8))
        .merged()
        .contains("--forceflush")
}
