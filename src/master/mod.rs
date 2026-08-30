//! 主控端：任务生成、调度执行、交互菜单

pub mod builder;
pub mod executor;
pub mod plan;
pub mod rate_window;
pub mod run_status;
pub mod ui;
pub mod webui;

pub use ui::replay_report;
