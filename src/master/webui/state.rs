//! 控制台的进程内状态：当前运行、控制台缓冲、监视会话。
//!
//! 会话是有主的、会过期的：浏览器关掉了、页面刷新了，谁来停那条还在跑的
//! 采样？答案是租约——[`MonitorSession`] 带心跳，[`reap_dead_monitors`] 负责
//! 收尸。没有这一层，一次误关页面就会在辅测端留下一个永远不停的采样线程。

use super::*;

#[derive(Default)]
pub(super) struct UiState {
    pub(super) cfg: Config,
    pub(super) agent_host: String,
    pub(super) master: HostInfo,
    pub(super) agent: HostInfo,
}

pub(super) struct Console {
    pub(super) state: Mutex<UiState>,
    pub(super) running: AtomicBool,
    /// 串行化新一轮的取消状态 reset 与停止请求，避免二者竞态覆盖。
    pub(super) run_gate: Mutex<()>,
    pub(super) report: Mutex<String>,
    /// 空串 = 不启用认证（只监听回环时的默认）。
    ///
    /// **有意不进 config.json**：控制台自己就提供「下载 config.json」，
    /// 把它的访问口令写进那份可下载、可传阅的文件里等于当场泄露。
    pub(super) ui_token: String,
    /// 速率监控会话。**不受 `running` 约束**：监控和一轮测试是正交的两件事，
    /// 边跑边看正是它最有用的场景。
    pub(super) monitors: Mutex<HashMap<String, MonitorSession>>,
    /// 结构化运行状态（ADR-2）。
    ///
    /// 与 `report` 那个字段的关系：报告路径现在由 executor 的回调直接写进这里，
    /// `report` 保留是为了兼容——`/api/open-report` 和旧前端都还在读它，
    /// 两边由 `api_progress` 保持同步。
    pub(super) run_status: Arc<RunStatusRecorder>,
}

/// 环形缓冲上限：1 秒一个点约等于 2 小时。再长就该用 `cpe_test monitor --csv`，
/// 网页不该变成一个无上限的数据库。
pub(super) const MONITOR_MAX_POINTS: usize = 7200;

#[derive(Debug, Clone, Serialize)]
pub(super) struct MonitorPoint {
    /// 会话开始后的秒数。用相对时间而不是墙钟：主控和辅测机的系统时钟
    /// 不保证同步，两条曲线放在一起看时相对时间才对得上。
    pub(super) t: f64,
    pub(super) rx_mbps: f64,
    pub(super) tx_mbps: f64,
}

#[derive(Default)]
pub(super) struct MonitorData {
    pub(super) points: std::collections::VecDeque<MonitorPoint>,
    /// 已经被挤出缓冲的点数，游标是绝对序号，靠它换算。
    pub(super) dropped: usize,
    pub(super) error: Option<String>,
    pub(super) running: bool,
    /// 页面最后一次来取样本的时刻；`None` = 一次都没来过。
    ///
    /// 采样线程靠它自己收摊。关掉浏览器标签页不会通知服务端，没有这道
    /// 兜底的话，本机那条线程会一直读计数器，辅测机那条还会一直占着
    /// agent 上的 monitor 资源直到租约到期。
    pub(super) last_poll: Option<std::time::Instant>,
}

/// 页面静默多久之后采样线程自行收摊。页面每秒来取一次，90 秒没动静
/// 只能是标签页已经不在了。
pub(super) const MONITOR_IDLE_TIMEOUT: Duration = Duration::from_secs(90);

/// 同时存在的监控会话上限。
///
/// 页面一次只开一路，这里的余量是留给「多开了几个标签页」和还没轮到回收的
/// 旧会话的。撞上限只可能是有人在刷接口——控制台一旦 `--ui-bind` 出去，
/// 一个拿到口令的客户端循环调 /api/monitor/start 就能一路撑起线程和
/// 辅测机侧的 monitor 资源。
///
/// 这是个软上限：查上限和插表之间隔着起线程那一步，几条工作线程同时进来时
/// 最多会多出 `UI_WORKERS - 1` 条。这不影响它要挡的事——表满之后每一次
/// start 都会被拒，涨不上去。
pub(super) const MONITOR_MAX_SESSIONS: usize = 8;

/// 辅测机侧监控的租约秒数。
///
/// **这是一个心跳周期，不是监控时长上限**：`/monitor/status` 每次都会给
/// agent 那边续期。所以它要回答的问题只有一个——「控制台没了之后，
/// 辅测机最多替它白跑多久」。180 秒足够扛过最大轮询间隔（60s）加网络抖动，
/// 又不会让一个孤儿监控在对面挂上小时级。
pub(super) const UI_MONITOR_LEASE_SECS: u64 = 180;

pub(super) struct MonitorSession {
    pub(super) side: String,
    pub(super) iface: String,
    pub(super) stop: Arc<AtomicBool>,
    pub(super) data: Arc<Mutex<MonitorData>>,
    /// 会话建立的时刻。回收要用它：页面刚开就被关掉的会话一次样本都没取过，
    /// 没有 `last_poll` 可看。
    pub(super) started: std::time::Instant,
}

/// 回收已经收摊、页面也不再来取的会话。
///
/// 采样线程自己会在 `MONITOR_IDLE_TIMEOUT` 之后停掉（见 `monitor_abandoned`），
/// 但停掉的只是线程：会话连同它那个最多 `MONITOR_MAX_POINTS` 点的缓冲还留在
/// 表里。关掉浏览器标签页不会通知服务端，所以「显式 /api/monitor/stop」不能
/// 是唯一的出口——否则每刷新一次页面就多留一条，永远不掉。
pub(super) fn reap_dead_monitors(monitors: &mut HashMap<String, MonitorSession>) {
    monitors.retain(|_, session| {
        let (running, last_poll) = {
            let data = lock_recover(&session.data);
            (data.running, data.last_poll)
        };
        if running {
            return true;
        }
        // 线程已停：再留一个空闲窗口，让还在的页面把「已停止」读走、正常收尾。
        // 过了这个窗口无论它还在不在，这条都该消失。
        match last_poll {
            Some(seen) => seen.elapsed() <= MONITOR_IDLE_TIMEOUT,
            None => session.started.elapsed() <= MONITOR_IDLE_TIMEOUT,
        }
    });
}

/// 页面已经不在了吗？两条采样线程共用同一判据。
pub(super) fn monitor_abandoned(
    data: &Arc<Mutex<MonitorData>>,
    started: std::time::Instant,
) -> bool {
    let last_poll = lock_recover(data).last_poll;
    match last_poll {
        Some(seen) => seen.elapsed() > MONITOR_IDLE_TIMEOUT,
        None => started.elapsed() > MONITOR_IDLE_TIMEOUT,
    }
}

impl MonitorData {
    pub(super) fn push(&mut self, point: MonitorPoint) {
        self.points.push_back(point);
        while self.points.len() > MONITOR_MAX_POINTS {
            self.points.pop_front();
            self.dropped += 1;
        }
    }
}

/// 控制台启动参数。
pub struct UiOpts {
    pub bind: String,
    pub port: u16,
    pub config_path: Option<String>,
    /// 主控访问辅测机用的共享口令；`None` = 沿用配置文件里的值。
    pub agent_token: Option<String>,
    /// 浏览器访问控制台需要的口令；空串 = 不认证。
    ///
    /// 和 `agent_token` 是两件事：前者是主控→辅测机，后者是浏览器→控制台。
    pub ui_token: String,
}
