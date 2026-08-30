//! 截图：Windows 走 GDI Win32 API（零 PowerShell、零第三方截图库），
//! macOS 走系统自带 screencapture 命令。输出 PNG 字节。

/// 截取主屏，返回 PNG 数据
pub fn capture_png() -> Result<Vec<u8>, String> {
    #[cfg(windows)]
    {
        capture_windows()
    }
    #[cfg(target_os = "macos")]
    {
        capture_macos()
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        Err("此平台不支持截图".into())
    }
}

#[cfg(target_os = "macos")]
fn capture_macos() -> Result<Vec<u8>, String> {
    use crate::util::{now_compact, run_cmd, temp_file};
    use std::time::Duration;
    let tmp = temp_file(&format!("cpe_shot_{}.png", now_compact()));
    let tmp_s = tmp.to_string_lossy().into_owned();
    let out = run_cmd(
        "screencapture",
        &["-x", "-t", "png", &tmp_s],
        Duration::from_secs(20),
    );
    if !tmp.exists() {
        return Err(format!(
            "screencapture 失败（可能需要在 系统设置->隐私与安全性->屏幕录制 里授权）: {}",
            out.merged().trim()
        ));
    }
    let data = std::fs::read(&tmp).map_err(|e| format!("读截图文件失败: {e}"))?;
    let _ = std::fs::remove_file(&tmp);
    if data.is_empty() {
        return Err("截图文件为空".into());
    }
    Ok(data)
}

#[cfg(windows)]
fn capture_windows() -> Result<Vec<u8>, String> {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        SRCCOPY,
    };
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};

    unsafe {
        let w = GetSystemMetrics(SM_CXSCREEN);
        let h = GetSystemMetrics(SM_CYSCREEN);
        if w <= 0 || h <= 0 {
            return Err("获取屏幕尺寸失败".into());
        }
        // 缓冲区大小在**申请任何 GDI 句柄之前**算完：这样尺寸不合理时直接返回，
        // 不存在“已经拿了句柄又要在中途还回去”的清理路径。
        //
        // 用 checked_mul 而不是直接乘：一旦回绕，下面 GetDIBits 会照着
        // BITMAPINFO 里的宽高往一个偏小的缓冲区里写——那是一次真实的越界写，
        // 而裸指针那一侧没有任何边界检查会拦住它。当前 GetSystemMetrics 的
        // 取值范围下算不出溢出，但这个前提写在别人的文档里，不写在这段代码里。
        let Some(buf_len) = (w as usize)
            .checked_mul(h as usize)
            .and_then(|pixels| pixels.checked_mul(4))
        else {
            return Err(format!("屏幕尺寸 {w}x{h} 过大，无法分配截图缓冲区"));
        };
        let hdc_screen = GetDC(None);
        if hdc_screen.is_invalid() {
            return Err("GetDC 失败".into());
        }
        // 这两步失败时**不在这里 return**：从拿到 hdc_screen 到下面那段统一
        // 清理之间不允许有提前返回，否则每失败一次就漏一个 DC/位图，而控制台
        // 是常驻进程、截图开关一开就是每个单元一次。句柄无效的后果由
        // BitBlt/GetDIBits 的返回值兜住（NULL DC 会让它们失败），清理完再按
        // 哪一级挂了给出具体错误。
        let hdc_mem = CreateCompatibleDC(hdc_screen);
        let hbm = CreateCompatibleBitmap(hdc_screen, w, h);
        let old = SelectObject(hdc_mem, hbm);

        let blt = BitBlt(hdc_mem, 0, 0, w, h, hdc_screen, 0, 0, SRCCOPY);

        let mut buf: Vec<u8> = vec![0u8; buf_len];
        let mut bi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: w,
                biHeight: -h, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let got = GetDIBits(
            hdc_mem,
            hbm,
            0,
            h as u32,
            Some(buf.as_mut_ptr() as *mut _),
            &mut bi,
            DIB_RGB_COLORS,
        );

        SelectObject(hdc_mem, old);
        let _ = DeleteObject(hbm);
        let _ = DeleteDC(hdc_mem);
        ReleaseDC(None, hdc_screen);

        // 句柄和抓取分级报错。原来这里只有一句 "BitBlt/GetDIBits 失败"：
        // CreateCompatibleDC / CreateCompatibleBitmap 返回 NULL 时也会走到这句，
        // 于是「内存 DC 建不出来」和「屏幕抓取被拒」在日志里长得一模一样。
        // 截图是给人排查环境用的，错在哪一级必须说出来。
        if hdc_mem.is_invalid() {
            return Err("CreateCompatibleDC 失败：无法创建内存 DC".into());
        }
        if hbm.is_invalid() {
            return Err(format!(
                "CreateCompatibleBitmap 失败：无法创建 {w}x{h} 位图"
            ));
        }
        if let Err(error) = blt {
            return Err(format!("BitBlt 失败：{error}"));
        }
        if got == 0 {
            return Err("GetDIBits 失败：未能读出位图像素".into());
        }
        // BGRA -> RGBA
        for px in buf.chunks_exact_mut(4) {
            px.swap(0, 2);
            px[3] = 255;
        }
        encode_png(&buf, w as u32, h as u32)
    }
}

#[cfg(any(windows, test))]
fn encode_png(rgba: &[u8], w: u32, h: u32) -> Result<Vec<u8>, String> {
    let mut out: Vec<u8> = Vec::new();
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(png::ColorType::Rgba);
        enc.set_depth(png::BitDepth::Eight);
        let mut writer = enc
            .write_header()
            .map_err(|e| format!("PNG 编码失败: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("PNG 写入失败: {e}"))?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_encode_png() {
        let rgba = vec![255u8; 4 * 4];
        let png = super::encode_png(&rgba, 2, 2).unwrap();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }
}
