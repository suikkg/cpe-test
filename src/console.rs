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

/// 用系统默认浏览器打开一个 URL。
///
/// 和 `open_path` 分开是因为 URL 不是路径：`Path` 在 Windows 上会把
/// `http://host` 里的斜杠正规化成反斜杠，start 就打不开了。
pub fn open_url(url: &str) {
    if cfg!(windows) {
        let _ = Command::new("cmd").args(["/C", "start", "", url]).spawn();
    } else if cfg!(target_os = "macos") {
        let _ = Command::new("open").arg(url).spawn();
    } else {
        let _ = Command::new("xdg-open").arg(url).spawn();
    }
}

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
