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
/// 「停掉整轮」的**持久**意图。
///
/// 和 `RUN_CANCELLED` 分开是为了让「跳过当前单元」成立：跳过要复用同一套
/// 收尾路径（那套路径已经被证明能干净地停掉远端作业、回收端口、收日志），
/// 于是它也得设 `RUN_CANCELLED`；而单元边界上要把这个标志清掉才能继续下一个。
///
/// 只有一个标志的话，这里有一个必然出现的竞态：操作员刚点了「跳过」，
/// 紧接着又点「停止」，而单元边界的那次清零会把「停止」一起抹掉——
/// 屏幕上写着已请求停止，测试却继续跑完剩下的十个小时。
static STOP_REQUESTED: AtomicBool = AtomicBool::new(false);
/// 「跳过当前单元」的一次性请求，由执行循环在单元边界取走。
static SKIP_CURRENT_UNIT: AtomicBool = AtomicBool::new(false);
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
    STOP_REQUESTED.store(true, Ordering::SeqCst);
    RUN_CANCELLED.store(true, Ordering::SeqCst);
}

/// 整轮是不是真的被要求停下来（相对「只是跳过当前单元」）。
pub fn is_stop_requested() -> bool {
    STOP_REQUESTED.load(Ordering::SeqCst)
}

/// 请求**跳过当前正在跑的单元**，队列继续。
///
/// 它复用整轮取消那套收尾路径（停远端作业、回收端口、收日志），只是不设
/// [`STOP_REQUESTED`]——执行循环在单元边界看到跳过请求就把取消位清掉继续跑。
///
/// 设计文档 v4.3.0 把「暂停 / 跳过当前」记成「留待后续版本」，理由是当时只有
/// 整轮取消协议。**只做「跳过」不做「暂停」**是有意的：跳过不需要挂起远端作业，
/// 一个「取消当前单元并前进」的信号就够；而暂停要给每个远端作业加一套挂起/
/// 恢复状态，代价大得多，用处也小得多（11.5 小时的队列里，人要的是「这条别跑了」
/// 而不是「全世界停一下」）。
pub fn request_skip_unit() {
    SKIP_CURRENT_UNIT.store(true, Ordering::SeqCst);
    RUN_CANCELLED.store(true, Ordering::SeqCst);
}

/// 取走并清除「跳过当前单元」的请求。**只在单元边界调用**。
///
/// 返回 `true` 时调用方应当清掉取消位继续跑——但必须先确认没有人真的要停
/// （[`is_stop_requested`] / [`is_shutdown_requested`]）。
pub fn take_skip_unit() -> bool {
    SKIP_CURRENT_UNIT.swap(false, Ordering::SeqCst)
}

/// 单元边界上把「为了停掉这一个单元而设的取消位」清掉。
///
/// **停止和进程退出优先**：这两种意图一旦出现就不许被跳过请求抹掉。
/// 返回 `true` 表示确实清掉了、队列可以继续。
///
/// # 为什么要检查两遍
///
/// 「先看有没有人要停，再清取消位」是 check-then-store，中间有一个窗口：
/// HTTP 线程的 [`request_cancel`] 恰好落在这两步之间时，它设的
/// `RUN_CANCELLED=true` 会被下面那次 `store(false)` 抹掉，而
/// `STOP_REQUESTED` 在执行循环里**没有第二个读者**（循环只看
/// [`is_cancelled`]）——于是屏幕上写着「已请求停止」，队列却把剩下的
/// 单元全跑完。这正是把两个标志拆开要防的那件事，只拆标志还不够。
///
/// 清完再看一遍就够了：`request_cancel` 先设 `STOP_REQUESTED` 后设
/// `RUN_CANCELLED`，两者都是 `SeqCst`。所以若它的写入排在我们的 store 之前，
/// 第二次检查必定看得见 `STOP_REQUESTED`；若排在之后，它设的
/// `RUN_CANCELLED=true` 本来就不会被我们抹掉。
pub fn resume_after_skip() -> bool {
    resume_after_skip_with(|| {})
}

/// [`resume_after_skip`] 的本体。`between` 在「检查有没有人要停」和「清取消位」
/// 之间运行，也就是危险窗口本身——把它变成可以逐字复现的东西，不然这条竞态
/// 只能靠压测碰运气，而这些标志是全局静态，压测会把同一个测试二进制里别的
/// 用例一起拖下水。
fn resume_after_skip_with(between: impl FnOnce()) -> bool {
    if is_stop_requested() || is_shutdown_requested() {
        return false;
    }
    between();
    RUN_CANCELLED.store(false, Ordering::SeqCst);
    if is_stop_requested() || is_shutdown_requested() {
        RUN_CANCELLED.store(true, Ordering::SeqCst);
        return false;
    }
    true
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
    STOP_REQUESTED.store(false, Ordering::SeqCst);
    SKIP_CURRENT_UNIT.store(false, Ordering::SeqCst);
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

/// 读或写进程级取消位的测试互斥：调用后**一直持有到这个测试线程结束**。
///
/// 取消位是全局静态，同一个测试二进制里的用例并发跑。只让「改标志的」测试
/// 互斥不够——读它们的测试同样会被别人的写入带偏：
/// - 一条测试 `request_skip_unit()` 还没 `take_skip_unit()`，另一条测试经
///   `api_run` 走到 `reset()`，跳过请求凭空消失；
/// - 一条测试在验证 Ctrl+C 时把 `PROCESS_SHUTDOWN_REQUESTED` 临时置位，并发的
///   `api_run` 测试拿到的就是「控制台正在退出」而不是它要断言的那句错误；
/// - 执行器测试的单元循环读 `is_cancelled()`，别人留下的取消位会让它第一格就
///   break，`take_skip_unit()` 还会把别人的跳过请求吞掉。
///
/// 所以执行器测试的公共夹具（`isolated_ctx`）自动调用它，走 `api_run` /
/// `api_skip_unit` / 组合场景启停的测试显式调用它；漏调由
/// `every_test_touching_cancel_flags_takes_the_guard` 拦下。
///
/// 持有到线程结束而不是交回一个 guard 值，是为了**可重入**：测试先调了它、
/// 再经夹具调一次不会自锁；libtest 每个用例一个线程，线程退出时 TLS 析构
/// 释放锁（用例 panic 也一样，`lock_recover` 兜住中毒）。
#[cfg(test)]
pub(crate) fn test_guard() {
    use std::cell::RefCell;
    use std::sync::{Mutex, MutexGuard};
    static LOCK: Mutex<()> = Mutex::new(());
    thread_local! {
        static HELD: RefCell<Option<MutexGuard<'static, ()>>> = const { RefCell::new(None) };
    }
    HELD.with(|held| {
        let mut held = held.borrow_mut();
        if held.is_none() {
            *held = Some(crate::util::lock_recover(&LOCK));
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_cancel_and_process_shutdown_are_independent_until_shutdown_is_requested() {
        test_guard();
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

#[cfg(test)]
mod skip_tests {
    use super::*;

    /// **跳过和停止不能互相吃掉。**
    ///
    /// 操作员刚点「跳过当前单元」、紧接着点「停止」，而单元边界上要清取消位
    /// 才能继续下一个——只有一个标志的话，那次清零会把「停止」一起抹掉，
    /// 屏幕上写着已请求停止，测试却继续跑完剩下的十个小时。
    #[test]
    fn a_stop_that_lands_during_a_skip_is_never_cleared() {
        test_guard();
        reset();
        request_skip_unit();
        assert!(is_cancelled(), "跳过要复用整轮取消那套收尾路径");
        assert!(!is_stop_requested());

        // 停止在单元边界之前到达。
        request_cancel();
        assert!(take_skip_unit(), "跳过请求仍然在，队列会去问能不能继续");
        assert!(!resume_after_skip(), "但有人真的要停，不许继续");
        assert!(is_cancelled(), "取消位必须原样留着");
        reset();
    }

    /// 同一件事的**竞态版**：停止请求恰好挤在「查了没人要停」和「清取消位」之间。
    ///
    /// 只检查一遍的话，这次 `store(false)` 会把 `request_cancel` 刚设上的取消位
    /// 抹掉；而 `STOP_REQUESTED` 在执行循环里没有第二个读者（循环只看
    /// `is_cancelled`），于是「已请求停止」只停在屏幕上，队列照跑。
    #[test]
    fn a_stop_that_lands_inside_the_clear_window_still_stops_the_run() {
        test_guard();
        reset();
        request_skip_unit();
        assert!(take_skip_unit());

        // `between` 就是那个窗口：HTTP 线程在这一刻调了 request_cancel。
        let carry_on = resume_after_skip_with(request_cancel);

        assert!(!carry_on, "有人要停，不许继续下一个单元");
        assert!(is_cancelled(), "取消位必须留着——执行循环只认它");
        assert!(is_stop_requested());
        reset();
    }

    #[test]
    fn a_plain_skip_lets_the_queue_carry_on() {
        test_guard();
        reset();
        request_skip_unit();
        assert!(take_skip_unit());
        assert!(resume_after_skip());
        assert!(!is_cancelled(), "清掉取消位，下一个单元照跑");
        // 一次性：同一个请求不会被取走第二次。
        assert!(!take_skip_unit());
        reset();
    }

    #[test]
    fn ctrl_c_also_outranks_a_pending_skip() {
        test_guard();
        reset();
        request_skip_unit();
        request_shutdown();
        assert!(take_skip_unit());
        assert!(!resume_after_skip(), "进程退出请求优先于跳过");
        PROCESS_SHUTDOWN_REQUESTED.store(false, Ordering::SeqCst);
        reset();
    }
}

#[cfg(test)]
mod guard_coverage {
    use std::path::Path;

    /// 读写进程级取消位的入口。测试体里出现任何一个，就必须持有 `test_guard`。
    ///
    /// 除了直接读写，还有经由下面这些入口间接读写的：
    /// - `api_run*` 过了互斥检查会 `reset()`，开头还读进程退出位；
    /// - `api_stop` / 组合场景的 `stop` 会 `request_cancel()`；
    /// - `api_skip_unit` / `RunStatusRecorder::request_skip` 会 `request_skip_unit()`；
    /// - 造了执行器 `Ctx` 的用例跑单元时会读取消位、吞跳过请求。
    const TOUCHES_FLAGS: &[&str] = &[
        "cancel::reset(",
        "cancel::request_cancel(",
        "cancel::request_skip_unit(",
        "cancel::request_shutdown(",
        "cancel::take_skip_unit(",
        "cancel::resume_after_skip(",
        "cancel::is_cancelled(",
        "cancel::is_stop_requested(",
        "cancel::is_shutdown_requested(",
        "api_run(",
        "api_run_impl(",
        "api_run_for_scenario(",
        "api_stop(",
        "api_skip_unit(",
        ".scenario.start(",
        ".scenario.stop(",
        ".request_skip(",
        "Ctx {",
    ];
    /// 本模块里只读源码、不碰标志的用例。
    const SCANNER_TESTS: &[&str] = &[
        "every_test_touching_cancel_flags_takes_the_guard",
        "the_source_scanner_ignores_literals_and_comments",
    ];
    /// 自带 `test_guard()` 的夹具。
    const GUARDED_FIXTURES: &[&str] = &["test_guard()", "isolated_ctx("];

    /// 去掉注释和字符串/字符字面量，只留代码本身，免得大括号计数和关键字匹配
    /// 被字面量里的 `{` 或文档注释里的函数名带偏。
    fn strip(source: &str) -> String {
        let bytes = source.as_bytes();
        let mut out = String::with_capacity(source.len());
        let mut i = 0;
        while i < bytes.len() {
            let rest = &source[i..];
            if rest.starts_with("//") {
                let end = rest.find('\n').unwrap_or(rest.len());
                i += end;
            } else if rest.starts_with("/*") {
                let end = rest.find("*/").map_or(rest.len(), |at| at + 2);
                i += end;
            } else if let Some(hashes) = raw_string_hashes(rest) {
                let open = 2 + hashes;
                let close = format!("\"{}", "#".repeat(hashes));
                let end = rest[open..]
                    .find(&close)
                    .map_or(rest.len(), |at| open + at + close.len());
                out.push_str("\"\"");
                i += end;
            } else if rest.starts_with('"') {
                let mut j = 1;
                while j < rest.len() {
                    match rest.as_bytes()[j] {
                        b'\\' => j += 2,
                        b'"' => break,
                        _ => j += 1,
                    }
                }
                out.push_str("\"\"");
                i += (j + 1).min(rest.len());
            } else if rest.starts_with("'") && char_literal_len(rest).is_some() {
                out.push_str("' '");
                i += char_literal_len(rest).unwrap();
            } else {
                let ch = rest.chars().next().unwrap();
                out.push(ch);
                i += ch.len_utf8();
            }
        }
        out
    }

    fn raw_string_hashes(rest: &str) -> Option<usize> {
        let tail = rest.strip_prefix('r')?;
        let hashes = tail.chars().take_while(|c| *c == '#').count();
        tail[hashes..].starts_with('"').then_some(hashes)
    }

    /// `'a'`、`'\n'`、`'{'` 这类字符字面量的长度；生命周期 `'a` 返回 `None`。
    fn char_literal_len(rest: &str) -> Option<usize> {
        let mut chars = rest.char_indices().skip(1);
        let (_, first) = chars.next()?;
        if first == '\\' {
            let close = rest[2..].find('\'')?;
            return Some(2 + close + 1);
        }
        let (at, next) = chars.next()?;
        (next == '\'').then_some(at + 1)
    }

    /// 每个 `#[test]` 函数的 (名字, 函数体)。
    fn test_bodies(code: &str) -> Vec<(String, String)> {
        let mut out = Vec::new();
        let mut from = 0;
        while let Some(at) = code[from..].find("#[test]") {
            let start = from + at;
            let Some(fn_at) = code[start..].find("fn ") else {
                break;
            };
            let name_start = start + fn_at + 3;
            let name: String = code[name_start..]
                .chars()
                .take_while(|c| c.is_alphanumeric() || *c == '_')
                .collect();
            let Some(open) = code[name_start..].find('{') else {
                break;
            };
            let open = name_start + open;
            let mut depth = 0usize;
            let mut end = code.len();
            for (offset, ch) in code[open..].char_indices() {
                match ch {
                    '{' => depth += 1,
                    '}' => {
                        depth -= 1;
                        if depth == 0 {
                            end = open + offset + 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            out.push((name, code[open..end].to_string()));
            from = end;
        }
        out
    }

    fn rust_files(dir: &Path, out: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// 漏了 guard 的后果不是这条用例红，而是**别的**用例偶发红：一条
    /// `request_skip_unit()` 的测试被并发的 `reset()` 抹掉请求，或者执行器测试
    /// 撞上别人临时置位的取消位提前 break。这种失败复现不了、也指不到真凶，
    /// 所以在源码层面拦。
    #[test]
    fn every_test_touching_cancel_flags_takes_the_guard() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_files(&root, &mut files);
        let mut missing = Vec::new();
        for file in files {
            let code = strip(&std::fs::read_to_string(&file).unwrap());
            let in_cancel_module = file.ends_with("cancel.rs");
            for (name, body) in test_bodies(&code) {
                let touches = TOUCHES_FLAGS.iter().any(|needle| body.contains(needle))
                    // 本模块的用例直接调 `reset()` / `request_cancel()`，不带路径前缀。
                    || (in_cancel_module && !SCANNER_TESTS.contains(&name.as_str()));
                let guarded = GUARDED_FIXTURES.iter().any(|needle| body.contains(needle));
                if touches && !guarded {
                    let shown = file
                        .strip_prefix(&root)
                        .unwrap_or(&file)
                        .display()
                        .to_string();
                    missing.push(format!("{shown}::{name}"));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "这些测试读写了进程级取消位却没有调用 crate::cancel::test_guard()：\n{}",
            missing.join("\n")
        );
    }

    #[test]
    fn the_source_scanner_ignores_literals_and_comments() {
        let code = strip("#[test]\nfn a() { let s = \"api_run(\"; // api_stop(\n let c = '{'; }\n#[test]\nfn b() { api_stop(&x); }");
        let bodies = test_bodies(&code);
        assert_eq!(bodies.len(), 2);
        assert!(!TOUCHES_FLAGS
            .iter()
            .any(|needle| bodies[0].1.contains(needle)));
        assert!(bodies[1].1.contains("api_stop("));
    }
}
