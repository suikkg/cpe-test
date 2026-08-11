//! 系统命令封装与输出解析（纯 cmd，零 PowerShell）

pub mod ctstraffic;
#[cfg(any(windows, test))]
pub mod ipconfig;
pub mod iperf;
#[cfg(any(windows, test))]
pub mod netsh;
