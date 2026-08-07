//! 平台无关的取消信号处理。
//!
//! Windows：使用 `SetConsoleCtrlHandler` 原生 API；
//! - 第一次 Ctrl+C：设置取消标志并返回 TRUE（本进程存活，主循环检测后优雅收尾）
//! - 第二次 Ctrl+C：返回 FALSE 交由默认处理器强退
//!
//! 注意：返回 TRUE 只让**本进程**不被默认处理器终止。若经 cmd.exe 批处理
//! （start_*.bat）启动，cmd.exe 是独立进程，会另行弹出 "Terminate batch job (Y/N)?"，
//! 此时请按 N 让批处理等待本进程优雅退出；直接运行 exe 则无此提示。
//!
//! 非 Windows：使用 `ctrlc` crate。

use std::sync::atomic::{AtomicBool, Ordering};

static CANCELLED: AtomicBool = AtomicBool::new(false);

/// 是否收到了取消信号（Ctrl+C）
pub fn is_cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

/// 返回取消标志的原子引用，供底层受控命令轮询（与全局标志同源）。
pub fn cancel_flag() -> &'static AtomicBool {
    &CANCELLED
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
            let _ = SetConsoleCtrlHandler(Some(handler), true);
        }
    }

    #[cfg(not(windows))]
    {
        let _ = ctrlc::set_handler(move || {
            CANCELLED.store(true, Ordering::SeqCst);
        });
    }
}
