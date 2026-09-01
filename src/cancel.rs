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
use std::sync::Once;

static RUN_CANCELLED: AtomicBool = AtomicBool::new(false);
static PROCESS_SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);
static HANDLER_SETUP: Once = Once::new();

#[cfg(windows)]
use std::sync::atomic::AtomicU32;
#[cfg(windows)]
static PRESS_COUNT: AtomicU32 = AtomicU32::new(0);

/// 是否请求结束当前测试。
pub fn is_cancelled() -> bool {
    RUN_CANCELLED.load(Ordering::SeqCst)
}

/// 是否请求退出当前常驻进程（Ctrl+C 语义）。
pub fn is_shutdown_requested() -> bool {
    PROCESS_SHUTDOWN_REQUESTED.load(Ordering::SeqCst)
}

/// 返回当前测试取消标志的原子引用，供底层受控命令轮询。
pub fn cancel_flag() -> &'static AtomicBool {
    &RUN_CANCELLED
}

/// 请求当前测试优雅结束。Web 控制台的“停止”和 Ctrl+C 共用这一层信号。
pub fn request_cancel() {
    RUN_CANCELLED.store(true, Ordering::SeqCst);
}

/// 请求常驻进程退出，并先让当前测试完成收尾。
pub fn request_shutdown() {
    PROCESS_SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    request_cancel();
}

/// 开始新一轮长驻进程内测试前只重置当前测试取消状态。
///
/// 进程退出请求是单向状态，不能被新一轮测试清除。
pub fn reset() {
    RUN_CANCELLED.store(false, Ordering::SeqCst);
}

/// 注册 Ctrl+C 处理器。
///
/// 第一次按下：设置取消标志，主循环检测到后中断测试并生成报告。
/// 第二次按下：强退。
pub fn setup_cancel_handler() {
    HANDLER_SETUP.call_once(|| {
        #[cfg(windows)]
        {
            use windows::Win32::Foundation::BOOL;
            use windows::Win32::System::Console::SetConsoleCtrlHandler;

            unsafe extern "system" fn handler(ctrl_type: u32) -> BOOL {
                // CTRL_C_EVENT = 0
                if ctrl_type == 0 {
                    request_shutdown();
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
            let _ = ctrlc::set_handler(request_shutdown);
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_cancel_and_process_shutdown_are_independent_until_shutdown_is_requested() {
        RUN_CANCELLED.store(false, Ordering::SeqCst);
        PROCESS_SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);

        request_cancel();
        assert!(is_cancelled());
        assert!(!is_shutdown_requested());

        request_shutdown();
        assert!(is_cancelled());
        assert!(is_shutdown_requested());

        // 进程退出状态没有公开 reset；测试清理它，避免影响同一测试二进制中的
        // 其他取消/退出用例。
        reset();
        PROCESS_SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
    }
}
