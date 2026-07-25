//! 平台无关的取消信号处理。
//!
//! Windows：使用 `SetConsoleCtrlHandler` 原生 API；
//! - 第一次 Ctrl+C：设置取消标志并返回 TRUE（阻止 cmd.exe "Terminate batch job?" 提示）
//! - 第二次 Ctrl+C：返回 FALSE 交由默认处理器强退
//!
//! 非 Windows：使用 `ctrlc` crate。

use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// 是否收到了取消信号（Ctrl+C）
pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// 注册 Ctrl+C 处理器。
///
/// 第一次按下：设置取消标志，主循环检测到后中断测试并生成报告。
/// 第二次按下：强退。
pub fn setup_cancel_handler() {
    #[cfg(windows)]
    {
        use std::sync::atomic::AtomicU32;
        use windows::Win32::Foundation::BOOL;
        use windows::Win32::System::Console::SetConsoleCtrlHandler;

        static PRESS_COUNT: AtomicU32 = AtomicU32::new(0);

        unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
            // CTRL_C_EVENT = 0
            if ctrl_type == 0 {
                CANCELLED.store(true, Ordering::SeqCst);
                let count = PRESS_COUNT.fetch_add(1, Ordering::SeqCst);
                if count == 0 {
                    // 第一次：吃掉信号，阻止 cmd.exe 弹出 "Terminate batch job?"
                    return BOOL::from(true);
                }
            }
            // 第二次或非 CTRL_C：交给默认处理器
            BOOL::from(false)
        }

        unsafe {
            let _ = SetConsoleCtrlHandler(Some(handler));
        }
    }

    #[cfg(not(windows))]
    {
        let _ = ctrlc::set_handler(move || {
            CANCELLED.store(true, Ordering::SeqCst);
        });
    }
}
