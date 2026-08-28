//! 终端交互：读一行、提问、解析序号选择、用系统默认程序打开文件。
//!
//! 这几个函数只服务"人坐在终端前面"这一种场景，与 agent 无关，也与任何
//! 测量逻辑无关。单独成模块是为了让 `util` 只保留真正跨领域的原语。

use std::io::Write;
use std::path::Path;
use std::process::Command;

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

/// 起一个「打开」子进程，并把它回收掉。
///
/// `Child` 被直接丢弃时 Rust **不会** wait：在 Unix 上每调用一次就留下一个
/// 僵尸进程，直到本进程退出。命令行模式一辈子只开一两次报告无所谓，但控制台
/// 和 agent 都是常驻进程，「打开报告」按钮点几十次就攒几十个。
/// 起一条短命线程去 join，既不阻塞调用方，也不把回收推给进程退出。
fn spawn_and_reap(mut command: Command) {
    let Ok(mut child) = command.spawn() else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("cpe-open-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
}

/// 用系统默认浏览器打开一个 URL。
///
/// 和 `open_path` 分开是因为 URL 不是路径：`Path` 在 Windows 上会把
/// `http://host` 里的斜杠正规化成反斜杠，start 就打不开了。
pub fn open_url(url: &str) {
    spawn_and_reap(opener_command(url));
}

/// 用系统默认程序打开文件（报告自动打开）
pub fn open_path(p: &Path) {
    spawn_and_reap(opener_command(&p.to_string_lossy()));
}

fn opener_command(target: &str) -> Command {
    if cfg!(windows) {
        let mut command = Command::new("cmd");
        command.args(["/C", "start", "", target]);
        command
    } else if cfg!(target_os = "macos") {
        let mut command = Command::new("open");
        command.arg(target);
        command
    } else {
        let mut command = Command::new("xdg-open");
        command.arg(target);
        command
    }
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
}
