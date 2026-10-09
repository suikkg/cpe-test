//! 每个单元回收完资源后，仅截取该单元参与的电脑一次。
use super::{config::InnerConfig, remote::Remote, UnitRow};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Serialize;
use std::{io::Write, path::Path, time::Duration};

#[derive(Serialize)]
pub(super) struct Capture {
    pub host: String,
    pub path: String,
    pub error: Option<String>,
}

pub(super) fn capture(cfg: &InnerConfig, row: &UnitRow, dir: &Path) -> Capture {
    capture_with(&row.host, row.index, dir, || {
        if row.host == "master" {
            crate::screenshot::capture_png()
        } else {
            let agent = cfg
                .agents
                .iter()
                .find(|a| a.id == row.host)
                .ok_or_else(|| "参与电脑的辅测机配置缺失".to_string())?;
            let out: crate::protocol::ScreenshotOut = Remote::new(agent.clone())
                .post_with_timeout(
                    "/screenshot",
                    &crate::protocol::ScreenshotReq {
                        label: format!("inner_unit_{}", row.index),
                    },
                    Duration::from_secs(180),
                )?;
            STANDARD
                .decode(out.image_b64)
                .map_err(|e| format!("截图 base64 解码失败: {e}"))
        }
    })
}

fn capture_with(
    host: &str,
    index: usize,
    dir: &Path,
    take: impl FnOnce() -> Result<Vec<u8>, String>,
) -> Capture {
    let mut result = Capture {
        host: host.into(),
        path: String::new(),
        error: None,
    };
    let saved = (|| {
        let png = take()?;
        if !png.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Err("截图响应不是 PNG".into());
        }
        let path = dir.join(format!("inner_unit_{index}.png"));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("创建截图失败: {e}"))?;
        file.write_all(&png)
            .map_err(|e| format!("写入截图失败: {e}"))?;
        Ok(path.to_string_lossy().into_owned())
    })();
    match saved {
        Ok(path) => result.path = path,
        Err(error) => result.error = Some(error),
    }
    result
}

pub(super) fn render(capture: &Capture) -> String {
    let title = format!(
        "{} · 测试结束桌面截图",
        super::report::escape(&capture.host)
    );
    let content = if let Some(error) = &capture.error {
        format!(
            "<p class=\"notice\">截图失败：{}</p>",
            super::report::escape(error)
        )
    } else {
        match std::fs::read(&capture.path) {
            Ok(png) if png.starts_with(b"\x89PNG\r\n\x1a\n") => format!("<img class=\"unit-screenshot\" src=\"data:image/png;base64,{}\" alt=\"{}\" loading=\"lazy\">", STANDARD.encode(png), title),
            _ => "<p class=\"notice\">截图文件无法读取</p>".into(),
        }
    };
    format!("<details><summary>{title}</summary>{content}</details>")
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn screenshot_is_captured_once_saved_without_overwrite_and_embedded_offline() {
        let dir = std::env::temp_dir().join(format!("inner-shot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("inner_unit_1.png");
        let _ = std::fs::remove_file(&path);
        let captured = capture_with("agent1", 1, &dir, || {
            Ok(b"\x89PNG\r\n\x1a\nfixture".to_vec())
        });
        assert!(captured.error.is_none());
        assert!(render(&captured).contains("data:image/png;base64,"));
        let duplicate = capture_with("agent1", 1, &dir, || Ok(b"\x89PNG\r\n\x1a\nother".to_vec()));
        assert!(duplicate.error.is_some());
        assert!(std::fs::read(&path).unwrap().ends_with(b"fixture"));
        let failed = capture_with("master", 2, &dir, || Err("<权限失败>".into()));
        assert!(render(&failed).contains("&lt;权限失败&gt;"));
        assert!(!dir.join("inner_unit_2.png").exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn screenshot_is_outside_traffic_and_resume_execution() {
        let source = include_str!("mod.rs");
        let start = source.find("let mut row = run_unit(").unwrap();
        let shot = source
            .find("row.screenshot = Some(screenshot::capture")
            .unwrap();
        let save = source[shot..].find("report.units.push(row)").unwrap();
        assert!(start < shot && save > 0);
        assert!(source[start..shot].contains("!cancel.load(Ordering::SeqCst)"));
        assert!(source[..start].contains("report.units.push(row);"));
        assert!(super::super::resumed_row(
            &super::super::plan::build(
                &super::super::config::parse_config(include_str!("../../inner.example.json"))
                    .unwrap()
            )
            .unwrap()
            .units[0]
        )
        .screenshot
        .is_none());
    }
}
