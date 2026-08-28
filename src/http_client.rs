//! 极简 HTTP/1.1 客户端（零第三方依赖）。
//!
//! 网络访问通过 [`Transport`] 隔离。生产代码使用 [`TcpTransport`]，测试可以使用
//! [`ScriptedTransport`] 注入丢包、延迟和损坏响应，而不需要启动真实 agent。
//! 对端是我们自己的 tiny_http agent。

// 故障注入脚手架（ScriptedTransport 一族）只在测试里编译，它用到的这些
// 依赖也跟着关进 cfg(test)：生产构建里只剩下真正发 HTTP 的那一百来行。
#[cfg(test)]
use crate::clock::{ManualClock, MonotonicClock};
#[cfg(test)]
use std::collections::{HashMap, VecDeque};
#[cfg(test)]
use std::fmt;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
#[cfg(test)]
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 一次响应最多读多少字节。
///
/// 和 agent 的 `MAX_BODY` 对齐。**服务端两侧本来都有这道闸，客户端一侧一个都没有**：
/// `Content-Length` 出来的 `usize` 被直接拿去 `Vec::resize`，对端说 1TB 就申请 1TB。
/// 而辅测机 IP 是人手敲进去的——敲到一台跑着别的服务的机器上，现在的表现是进程
/// 被 OOM 掉，而不是一句「这不是 agent」。
const MAX_RESPONSE_BYTES: usize = 100 * 1024 * 1024;

/// 一次请求从连上到读完的总时限。
///
/// `set_read_timeout` 管的是**单次读**：对端每隔「略小于 timeout」吐一个字节
/// 就能把一次请求无限拖住，无 `Content-Length` 的 `read_to_end` 分支尤其。
/// 总时限按调用方给的 timeout 放宽一倍再兜一层，正常请求碰不到它。
fn overall_deadline(timeout: Duration) -> Instant {
    Instant::now() + timeout.saturating_mul(2) + Duration::from_secs(5)
}

/// 读取过程中的两道闸：总时限和总字节数。
struct ReadLimits {
    deadline: Instant,
    max_bytes: usize,
}

impl ReadLimits {
    fn check_deadline(&self) -> Result<(), String> {
        if Instant::now() > self.deadline {
            return Err("读响应超时：对端在总时限内没有发完".into());
        }
        Ok(())
    }
}

/// 发送给 agent 的一次 HTTP 请求。
///
/// 这个结构只包含业务请求所需的字段；`TcpTransport` 会将其编码成 HTTP/1.1，
/// 测试 transport 则可以直接检查这些字段，不必解析 wire format。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpRequest {
    pub method: String,
    pub host: String,
    pub port: u16,
    pub path: String,
    pub body: String,
    pub token: Option<String>,
}

impl HttpRequest {
    /// 创建请求。空 body/token 会按现有客户端行为发送为空字符串/不带认证头。
    pub fn new(
        method: &str,
        host: &str,
        port: u16,
        path: &str,
        body: Option<&str>,
        token: Option<&str>,
    ) -> Self {
        Self {
            method: method.to_string(),
            host: host.to_string(),
            port,
            path: path.to_string(),
            body: body.unwrap_or_default().to_string(),
            token: token.filter(|value| !value.is_empty()).map(str::to_string),
        }
    }

    fn wire_bytes(&self) -> Vec<u8> {
        let auth_header = self
            .token
            .as_deref()
            .map(|token| format!("\r\nAuthorization: Bearer {token}"))
            .unwrap_or_default();
        format!(
            "{} {} HTTP/1.1\r\nHost: {}\r\nContent-Type: application/json; charset=utf-8\r\nContent-Length: {}\r\nConnection: close{}\r\n\r\n{}",
            self.method,
            self.path,
            self.host,
            self.body.len(),
            auth_header,
            self.body
        )
        .into_bytes()
    }
}

/// 已解析的 HTTP 响应。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
}

impl HttpResponse {
    pub fn new(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            body: body.into(),
        }
    }
}

/// HTTP 传输边界。
///
/// 实现必须在请求完成后返回已解析的响应；网络/协议错误用字符串返回，以保持旧
/// `post_json*` API 的错误类型兼容。trait 是 object-safe 的，后续主控可以持有
/// `Arc<dyn Transport>`，测试则可以传入 [`ScriptedTransport`]。
pub trait Transport: Send + Sync {
    fn send(&self, request: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String>;
}

/// 默认的真实 TCP/HTTP 实现。
#[derive(Clone, Copy, Debug, Default)]
pub struct TcpTransport;

impl Transport for TcpTransport {
    fn send(&self, request: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String> {
        tcp_request(request, timeout)
    }
}

#[cfg(test)]
/// 可注入的一次脚本交换。
///
/// `request_delay` 在“请求送达”之前等待，`response_delay` 在生成响应之前等待，
/// 因此可以表达非对称延迟。延迟使用真实 `sleep`，默认只在测试中使用短时值；生产
/// 主流程仍使用 [`TcpTransport`]。
#[derive(Clone, Debug)]
pub struct ScriptedExchange {
    pub request_delay: Duration,
    pub response_delay: Duration,
    pub outcome: ScriptedOutcome,
    response_gate: Option<Arc<ScriptedGate>>,
}

#[cfg(test)]
impl ScriptedExchange {
    pub fn response(status: u16, body: impl Into<String>) -> Self {
        Self {
            request_delay: Duration::ZERO,
            response_delay: Duration::ZERO,
            outcome: ScriptedOutcome::Response(HttpResponse::new(status, body)),
            response_gate: None,
        }
    }

    /// 请求交给注入的 handler 处理，并把 handler 返回的响应交给调用方。
    pub fn handler_response() -> Self {
        Self {
            request_delay: Duration::ZERO,
            response_delay: Duration::ZERO,
            outcome: ScriptedOutcome::HandlerResponse,
            response_gate: None,
        }
    }

    /// transport 自身失败（连不上、写不出去）。
    ///
    /// 目前没有用例脚本它——真实的连接失败在 `TcpTransport` 那侧，
    /// 而资源幂等测试关心的是「请求送没送达」，用 `drop_request` /
    /// `drop_response` 表达更准。保留是因为它和另外几种故障是同一套模型的
    /// 一部分，删掉会让这套模型缺一角；补一条用例比重新写回来便宜。
    #[allow(dead_code)]
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            request_delay: Duration::ZERO,
            response_delay: Duration::ZERO,
            outcome: ScriptedOutcome::Error(message.into()),
            response_gate: None,
        }
    }

    /// 模拟请求在网络中丢失：服务端不会看到请求，也不会产生响应。
    pub fn drop_request() -> Self {
        Self {
            request_delay: Duration::ZERO,
            response_delay: Duration::ZERO,
            outcome: ScriptedOutcome::DropRequest,
            response_gate: None,
        }
    }

    /// 模拟服务端已收到并处理请求，但响应在返回路径丢失。
    pub fn drop_response() -> Self {
        Self {
            request_delay: Duration::ZERO,
            response_delay: Duration::ZERO,
            outcome: ScriptedOutcome::DropResponse,
            response_gate: None,
        }
    }

    /// 模拟带 Content-Length 的不完整响应。客户端会把它作为协议错误拒绝。
    pub fn truncated(status: u16, body: impl Into<String>, expected_len: usize) -> Self {
        let body = body.into();
        Self {
            request_delay: Duration::ZERO,
            response_delay: Duration::ZERO,
            outcome: ScriptedOutcome::Truncated {
                status,
                expected_len: expected_len.max(body.len().saturating_add(1)),
                body,
            },
            response_gate: None,
        }
    }

    /// 给已有脚本加上请求方向和响应方向的独立延迟。
    pub fn with_delays(
        request_delay: Duration,
        response_delay: Duration,
        outcome: ScriptedOutcome,
    ) -> Self {
        Self {
            request_delay,
            response_delay,
            outcome,
            response_gate: None,
        }
    }

    /// 在响应交付前等待测试显式放行，用于不依赖 OS 调度地构造乱序。
    pub fn with_response_gate(mut self, gate: Arc<ScriptedGate>) -> Self {
        self.response_gate = Some(gate);
        self
    }
}

#[cfg(test)]
/// [`ScriptedExchange`] 产生的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)] // Error 分支见 ScriptedExchange::error
pub enum ScriptedOutcome {
    Response(HttpResponse),
    HandlerResponse,
    Error(String),
    DropRequest,
    DropResponse,
    Truncated {
        status: u16,
        body: String,
        expected_len: usize,
    },
}

#[cfg(test)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScriptedPhase {
    Request,
    Response,
}

#[cfg(test)]
/// 显式响应门。工作线程到达后阻塞，测试线程按指定顺序调用 `release`。
#[derive(Debug, Default)]
pub struct ScriptedGate {
    state: Mutex<ScriptedGateState>,
    wake: Condvar,
}

#[cfg(test)]
#[derive(Debug, Default)]
struct ScriptedGateState {
    reached: bool,
    released: bool,
}

#[cfg(test)]
impl ScriptedGate {
    pub fn new() -> Self {
        Self::default()
    }

    fn reach_and_wait(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.reached = true;
        self.wake.notify_all();
        while !state.released {
            state = self
                .wake
                .wait(state)
                .unwrap_or_else(|poisoned| poisoned.into_inner());
        }
    }

    pub fn wait_until_reached(&self, timeout: Duration) -> bool {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if state.reached {
            return true;
        }
        self.wake
            .wait_timeout_while(state, timeout, |state| !state.reached)
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .0
            .reached
    }

    pub fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.released = true;
        drop(state);
        self.wake.notify_all();
    }
}

#[cfg(test)]
/// 可观测的脚本 transport 事件。事件日志用于验证“请求是否送达”和“响应是否送达”，
/// 这两者在调用方看来都可能只是超时，但资源幂等测试需要区分它们。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ScriptedEvent {
    PhaseDelayed {
        request: HttpRequest,
        phase: ScriptedPhase,
        duration: Duration,
    },
    RequestSent(HttpRequest),
    RequestDropped(HttpRequest),
    ResponseDelivered {
        request: HttpRequest,
        response: HttpResponse,
    },
    ResponseDropped(HttpRequest),
    ResponseTruncated {
        request: HttpRequest,
        expected_len: usize,
        actual_len: usize,
    },
    TimedOut {
        request: HttpRequest,
        phase: ScriptedPhase,
        waited: Duration,
    },
    Failed {
        request: HttpRequest,
        message: String,
    },
}

#[cfg(test)]
#[derive(Default)]
struct ScriptedState {
    queue: VecDeque<ScriptedExchange>,
    by_path: HashMap<String, VecDeque<ScriptedExchange>>,
    by_request: HashMap<(String, String), VecDeque<ScriptedExchange>>,
    events: Vec<ScriptedEvent>,
}

#[cfg(test)]
type ScriptedHandler = dyn Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync + 'static;

#[cfg(test)]
/// 一个线程安全、可复制的确定性 transport。
///
/// 脚本按先进先出消费。使用 [`ScriptedTransport::push_for_path`] 可以让并发测试按
/// URL 路径分配脚本，从而稳定地构造响应乱序，而不依赖线程启动顺序。
#[derive(Clone)]
pub struct ScriptedTransport {
    state: Arc<Mutex<ScriptedState>>,
    clock: Arc<dyn MonotonicClock>,
    handler: Option<Arc<ScriptedHandler>>,
}

#[cfg(test)]
impl Default for ScriptedTransport {
    fn default() -> Self {
        Self::with_clock(Arc::new(ManualClock::new()))
    }
}

#[cfg(test)]
impl fmt::Debug for ScriptedTransport {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        formatter
            .debug_struct("ScriptedTransport")
            .field("queued", &state.queue.len())
            .field("path_queues", &state.by_path.len())
            .field("request_queues", &state.by_request.len())
            .field("events", &state.events.len())
            .finish()
    }
}

#[cfg(test)]
impl ScriptedTransport {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_clock(clock: Arc<dyn MonotonicClock>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ScriptedState::default())),
            clock,
            handler: None,
        }
    }

    pub fn with_handler<F>(clock: Arc<dyn MonotonicClock>, handler: F) -> Self
    where
        F: Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync + 'static,
    {
        Self {
            state: Arc::new(Mutex::new(ScriptedState::default())),
            clock,
            handler: Some(Arc::new(handler)),
        }
    }

    /// 将脚本追加到全局 FIFO 队列。
    pub fn push(&self, exchange: ScriptedExchange) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.queue.push_back(exchange);
    }

    /// 将脚本追加到指定 path 的 FIFO 队列。适合并发乱序测试。
    pub fn push_for_path(&self, path: impl Into<String>, exchange: ScriptedExchange) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .by_path
            .entry(path.into())
            .or_default()
            .push_back(exchange);
    }

    /// 为同一路径上的具体 request_id 分配脚本，避免并发线程争抢 FIFO。
    pub fn push_for_request(
        &self,
        path: impl Into<String>,
        request_id: impl Into<String>,
        exchange: ScriptedExchange,
    ) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state
            .by_request
            .entry((path.into(), request_id.into()))
            .or_default()
            .push_back(exchange);
    }

    pub fn remaining(&self) -> usize {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.queue.len()
            + state.by_path.values().map(VecDeque::len).sum::<usize>()
            + state.by_request.values().map(VecDeque::len).sum::<usize>()
    }

    /// 返回已记录事件的快照，不持有内部锁。
    pub fn events(&self) -> Vec<ScriptedEvent> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .events
            .clone()
    }

    /// 返回所有尝试发送的请求（包括被脚本丢弃的请求）。
    pub fn requests(&self) -> Vec<HttpRequest> {
        self.events()
            .into_iter()
            .filter_map(|event| match event {
                ScriptedEvent::RequestSent(request) | ScriptedEvent::RequestDropped(request) => {
                    Some(request)
                }
                _ => None,
            })
            .collect()
    }

    fn take_exchange(&self, request: &HttpRequest) -> Option<ScriptedExchange> {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let request_id = serde_json::from_str::<serde_json::Value>(&request.body)
            .ok()
            .and_then(|value| value.get("request_id")?.as_str().map(str::to_string));
        if let Some(request_id) = request_id {
            if let Some(exchange) = state
                .by_request
                .get_mut(&(request.path.clone(), request_id))
                .and_then(VecDeque::pop_front)
            {
                return Some(exchange);
            }
        }
        state
            .by_path
            .get_mut(&request.path)
            .and_then(VecDeque::pop_front)
            .or_else(|| state.queue.pop_front())
    }

    fn record(&self, event: ScriptedEvent) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .events
            .push(event);
    }

    fn invoke_handler(&self, request: &HttpRequest) -> Result<HttpResponse, String> {
        let handler = self
            .handler
            .as_ref()
            .ok_or_else(|| "scripted transport 没有配置请求 handler".to_string())?;
        handler(request)
    }

    fn apply_delay(
        &self,
        request: &HttpRequest,
        phase: ScriptedPhase,
        requested: Duration,
        remaining: &mut Duration,
    ) -> Result<(), String> {
        let waited = requested.min(*remaining);
        if !waited.is_zero() {
            self.clock.sleep(waited);
            self.record(ScriptedEvent::PhaseDelayed {
                request: request.clone(),
                phase,
                duration: waited,
            });
            *remaining = remaining.saturating_sub(waited);
        }
        if waited < requested {
            self.record(ScriptedEvent::TimedOut {
                request: request.clone(),
                phase,
                waited,
            });
            return Err(format!("scripted {phase:?} timed out after {waited:?}"));
        }
        Ok(())
    }

    fn exhaust_timeout(
        &self,
        request: &HttpRequest,
        phase: ScriptedPhase,
        remaining: &mut Duration,
    ) -> String {
        let waited = *remaining;
        if !waited.is_zero() {
            self.clock.sleep(waited);
            *remaining = Duration::ZERO;
        }
        self.record(ScriptedEvent::TimedOut {
            request: request.clone(),
            phase,
            waited,
        });
        format!("scripted {phase:?} timed out after {waited:?}")
    }
}

#[cfg(test)]
impl Transport for ScriptedTransport {
    fn send(&self, request: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String> {
        let Some(exchange) = self.take_exchange(request) else {
            let message = format!(
                "scripted transport 没有为 {} {} 准备响应",
                request.method, request.path
            );
            self.record(ScriptedEvent::Failed {
                request: request.clone(),
                message: message.clone(),
            });
            return Err(message);
        };

        let mut remaining = timeout;
        self.apply_delay(
            request,
            ScriptedPhase::Request,
            exchange.request_delay,
            &mut remaining,
        )?;

        if matches!(exchange.outcome, ScriptedOutcome::DropRequest) {
            self.record(ScriptedEvent::RequestDropped(request.clone()));
            return Err(self.exhaust_timeout(request, ScriptedPhase::Request, &mut remaining));
        }

        self.record(ScriptedEvent::RequestSent(request.clone()));

        enum PreparedOutcome {
            Response(HttpResponse),
            Error(String),
            DropResponse,
            Truncated {
                status: u16,
                body: String,
                expected_len: usize,
            },
        }

        let prepared = match exchange.outcome {
            ScriptedOutcome::Response(response) => PreparedOutcome::Response(response),
            ScriptedOutcome::HandlerResponse => {
                PreparedOutcome::Response(self.invoke_handler(request)?)
            }
            ScriptedOutcome::Error(message) => PreparedOutcome::Error(message),
            ScriptedOutcome::DropResponse => {
                if self.handler.is_some() {
                    if let Err(message) = self.invoke_handler(request) {
                        self.record(ScriptedEvent::Failed {
                            request: request.clone(),
                            message,
                        });
                    }
                }
                PreparedOutcome::DropResponse
            }
            ScriptedOutcome::Truncated {
                status,
                body,
                expected_len,
            } => {
                if self.handler.is_some() {
                    if let Err(message) = self.invoke_handler(request) {
                        self.record(ScriptedEvent::Failed {
                            request: request.clone(),
                            message,
                        });
                    }
                }
                PreparedOutcome::Truncated {
                    status,
                    body,
                    expected_len,
                }
            }
            ScriptedOutcome::DropRequest => unreachable!("handled above"),
        };

        self.apply_delay(
            request,
            ScriptedPhase::Response,
            exchange.response_delay,
            &mut remaining,
        )?;
        if let Some(gate) = exchange.response_gate {
            gate.reach_and_wait();
        }

        match prepared {
            PreparedOutcome::Response(response) => {
                self.record(ScriptedEvent::ResponseDelivered {
                    request: request.clone(),
                    response: response.clone(),
                });
                Ok(response)
            }
            PreparedOutcome::Error(message) => {
                self.record(ScriptedEvent::Failed {
                    request: request.clone(),
                    message: message.clone(),
                });
                Err(message)
            }
            PreparedOutcome::DropResponse => {
                self.record(ScriptedEvent::ResponseDropped(request.clone()));
                Err(self.exhaust_timeout(request, ScriptedPhase::Response, &mut remaining))
            }
            PreparedOutcome::Truncated {
                status,
                body,
                expected_len,
            } => {
                let actual_len = body.len();
                self.record(ScriptedEvent::ResponseTruncated {
                    request: request.clone(),
                    expected_len,
                    actual_len,
                });
                let raw = format!(
                    "HTTP/1.1 {status} Scripted\r\nContent-Length: {expected_len}\r\n\r\n{body}"
                );
                let mut reader = BufReader::new(std::io::Cursor::new(raw.into_bytes()));
                read_http_response(&mut reader)
                    .map_err(|error| format!("scripted response truncated: {error}"))
            }
        }
    }
}

/// POST JSON 并携带 `Authorization: Bearer <token>`（token 为空时不带头）。
pub fn post_json_auth(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    token: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    post_json_auth_with_transport(&TcpTransport, host, port, path, body, token, timeout)
}

/// 使用指定 transport 发送 POST JSON。主控注入 fake transport 时使用此函数。
pub fn post_json_auth_with_transport<T: Transport + ?Sized>(
    transport: &T,
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    token: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    request_with_transport(
        transport,
        "POST",
        host,
        port,
        path,
        Some(body),
        Some(token),
        timeout,
    )
}

/// GET 并携带 `Authorization: Bearer <token>`（token 为空时不带头）。
pub fn get_auth(
    host: &str,
    port: u16,
    path: &str,
    token: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    get_auth_with_transport(&TcpTransport, host, port, path, token, timeout)
}

pub fn get_auth_with_transport<T: Transport + ?Sized>(
    transport: &T,
    host: &str,
    port: u16,
    path: &str,
    token: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    request_with_transport(
        transport,
        "GET",
        host,
        port,
        path,
        None,
        Some(token),
        timeout,
    )
}

/// 兼容 v3 及更早版本的 POST 包装（不发送认证头）。
#[allow(dead_code)]
pub fn post_json(
    host: &str,
    port: u16,
    path: &str,
    body: &str,
    timeout: Duration,
) -> Result<(u16, String), String> {
    post_json_auth(host, port, path, body, "", timeout)
}

/// 兼容 v3 及更早版本的 GET 包装（不发送认证头）。
#[allow(dead_code)]
pub fn get(host: &str, port: u16, path: &str, timeout: Duration) -> Result<(u16, String), String> {
    get_auth(host, port, path, "", timeout)
}

/// 使用指定 transport 发送任意 HTTP 方法。返回旧 API 使用的 `(status, body)`。
#[allow(clippy::too_many_arguments)]
pub fn request_with_transport<T: Transport + ?Sized>(
    transport: &T,
    method: &str,
    host: &str,
    port: u16,
    path: &str,
    body: Option<&str>,
    token: Option<&str>,
    timeout: Duration,
) -> Result<(u16, String), String> {
    let request = HttpRequest::new(method, host, port, path, body, token);
    let response = transport.send(&request, timeout)?;
    Ok((response.status, response.body))
}

fn tcp_request(request: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String> {
    let addr_str = format!("{}:{}", request.host, request.port);
    let addrs: Vec<_> = addr_str
        .to_socket_addrs()
        .map_err(|e| format!("解析地址 {addr_str} 失败: {e}"))?
        .collect();
    if addrs.is_empty() {
        return Err(format!("地址 {addr_str} 无法解析"));
    }
    // 逐个试，不是只试第一个。主机名同时有 AAAA 和 A 记录时，解析器常把 IPv6
    // 排在前面；那条不通的话，只试第一个等于直接失败，而 IPv4 明明是通的。
    let mut last_error = String::new();
    let mut stream = None;
    for addr in &addrs {
        match TcpStream::connect_timeout(addr, CONNECT_TIMEOUT) {
            Ok(connected) => {
                stream = Some(connected);
                break;
            }
            Err(error) => last_error = format!("{addr}: {error}"),
        }
    }
    let Some(mut stream) = stream else {
        return Err(format!("连接 {addr_str} 失败: {last_error}"));
    };
    stream
        .set_read_timeout(Some(timeout))
        .map_err(|e| e.to_string())?;
    stream
        .set_write_timeout(Some(Duration::from_secs(30)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&request.wire_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    read_tcp_response(&mut stream, timeout)
}

/// 读出一个完整 HTTP 响应后交给公共解析器。按 Content-Length/chunked 提前停止，
/// 避免服务端没有及时关闭连接时把一次成功请求误报成超时。
fn read_tcp_response(stream: &mut TcpStream, timeout: Duration) -> Result<HttpResponse, String> {
    let mut reader = BufReader::new(stream);
    read_http_response_limited(
        &mut reader,
        &ReadLimits {
            deadline: overall_deadline(timeout),
            max_bytes: MAX_RESPONSE_BYTES,
        },
    )
}

#[cfg(test)]
fn read_http_response<R: BufRead>(reader: &mut R) -> Result<HttpResponse, String> {
    read_http_response_limited(
        reader,
        &ReadLimits {
            deadline: Instant::now() + Duration::from_secs(30),
            max_bytes: MAX_RESPONSE_BYTES,
        },
    )
}

fn read_http_response_limited<R: BufRead>(
    reader: &mut R,
    limits: &ReadLimits,
) -> Result<HttpResponse, String> {
    let mut head_lines: Vec<String> = Vec::new();
    let mut line_buf = String::new();
    loop {
        limits.check_deadline()?;
        line_buf.clear();
        let n = reader
            .read_line(&mut line_buf)
            .map_err(|e| format!("读头失败: {e}"))?;
        if n == 0 {
            return Err("连接意外关闭".into());
        }
        let line = line_buf.trim_end().to_string();
        if line.is_empty() {
            break;
        }
        head_lines.push(line);
    }
    let head_text = head_lines.join("\r\n");
    let status = parse_status(head_lines.first().map(String::as_str).unwrap_or(""))?;
    let is_chunked = head_text
        .to_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if is_chunked {
        read_chunked_body_limited(reader, limits)?
    } else {
        let cl = parse_content_length(&head_text);
        let mut buf = Vec::new();
        if let Some(len) = cl {
            // 先判上限**再**申请。反过来的话，判断永远来不及执行——
            // `resize` 就是那个会把进程打死的调用。
            if len > limits.max_bytes {
                return Err(format!(
                    "响应体声明 {len} 字节，超过上限 {} 字节；对端多半不是 cpe_test agent",
                    limits.max_bytes
                ));
            }
            buf.resize(len, 0);
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("读响应体失败: {e}"))?;
        } else {
            // 没有 Content-Length 时同样要封顶：读到上限 +1 字节就能判断是不是超了。
            let mut limited = reader.take(limits.max_bytes as u64 + 1);
            limited
                .read_to_end(&mut buf)
                .map_err(|e| format!("读响应体失败: {e}"))?;
            if buf.len() > limits.max_bytes {
                return Err(format!(
                    "响应体超过上限 {} 字节；对端多半不是 cpe_test agent",
                    limits.max_bytes
                ));
            }
        }
        String::from_utf8_lossy(&buf).into_owned()
    };
    Ok(HttpResponse::new(status, body))
}

/// 解码 chunked transfer encoding。
///
/// `pub(crate)` 以便 parser property/fuzz 测试入口（`parser_properties.rs`）
/// 直接覆盖该纯函数。
#[cfg(test)]
pub(crate) fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<String, String> {
    read_chunked_body_limited(
        reader,
        &ReadLimits {
            deadline: Instant::now() + Duration::from_secs(30),
            max_bytes: MAX_RESPONSE_BYTES,
        },
    )
}

fn read_chunked_body_limited<R: BufRead>(
    reader: &mut R,
    limits: &ReadLimits,
) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut size_buf = String::new();
    loop {
        limits.check_deadline()?;
        size_buf.clear();
        reader
            .read_line(&mut size_buf)
            .map_err(|e| format!("读 chunk 大小失败: {e}"))?;
        if size_buf.is_empty() {
            return Err("读 chunk 大小失败: 连接意外关闭".into());
        }
        let size_str = size_buf.trim();
        let size_str = size_str.split(';').next().unwrap_or(size_str);
        let size = usize::from_str_radix(size_str, 16)
            .map_err(|e| format!("chunk 大小解析失败 [{size_str}]: {e}"))?;
        // 单个 chunk 和累计总量共用同一份预算：只卡单个 chunk 的话，
        // 一万个 1MB 的 chunk 照样能把内存吃光。
        if size > limits.max_bytes || out.len().saturating_add(size) > limits.max_bytes {
            return Err(format!(
                "chunked 响应体超过上限 {} 字节；对端多半不是 cpe_test agent",
                limits.max_bytes
            ));
        }
        if size == 0 {
            // 最后的空 chunk，读掉尾部 CRLF；尾部 trailer 对本客户端没有意义。
            let _ = reader.read_line(&mut String::new());
            break;
        }
        let mut chunk_data = vec![0u8; size];
        reader
            .read_exact(&mut chunk_data)
            .map_err(|e| format!("读 chunk 数据失败: {e}"))?;
        out.extend_from_slice(&chunk_data);
        let mut crlf = String::new();
        reader
            .read_line(&mut crlf)
            .map_err(|e| format!("读 chunk 尾部失败: {e}"))?;
        if crlf.trim_end_matches(['\r', '\n']).is_empty() {
            continue;
        }
        return Err("chunk 数据缺少 CRLF 尾部".into());
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

fn parse_content_length(head: &str) -> Option<usize> {
    for line in head.lines() {
        let l = line.to_lowercase();
        if let Some(v) = l.strip_prefix("content-length:") {
            return v.trim().parse().ok();
        }
    }
    None
}

fn parse_status(line: &str) -> Result<u16, String> {
    let mut parts = line.split_whitespace();
    let _ver = parts.next();
    parts
        .next()
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("无法解析状态行: {line}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 对端说多大就申请多大，是这个客户端唯一能把进程直接打死的地方。
    ///
    /// 辅测机 IP 是人手敲进去的。敲到一台跑着别的服务的机器上时，正确的表现是
    /// 一句可读的错误，而不是 `Vec::resize` 照着对端声明的长度申请并写满、
    /// 然后被 OOM killer 带走。服务端两侧本来就都有 MAX_BODY，客户端这侧
    /// 一个 cap 都没有。
    #[test]
    fn an_absurd_content_length_is_refused_before_anything_is_allocated() {
        let head = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n",
            MAX_RESPONSE_BYTES + 1
        );
        let mut reader = Cursor::new(head.into_bytes());
        let error = read_http_response(&mut reader).expect_err("超限必须被拒");
        assert!(error.contains("超过上限"), "{error}");
        assert!(
            error.contains("agent"),
            "错误要提示对端可能不是 agent：{error}"
        );

        // 正常大小照旧放行。
        let ok = "HTTP/1.1 200 OK\r\nContent-Length: 2\r\n\r\nhi";
        let mut reader = Cursor::new(ok.as_bytes().to_vec());
        let response = read_http_response(&mut reader).expect("正常响应不能被误伤");
        assert_eq!((response.status, response.body.as_str()), (200, "hi"));
    }

    /// 没有 Content-Length 时同样要封顶，否则 `read_to_end` 一样是无界的。
    #[test]
    fn a_bodyless_header_still_caps_how_much_gets_read() {
        let mut body = b"HTTP/1.1 200 OK\r\n\r\n".to_vec();
        body.extend(std::iter::repeat_n(b'x', MAX_RESPONSE_BYTES + 8));
        let mut reader = Cursor::new(body);
        let error = read_http_response(&mut reader).expect_err("无 Content-Length 也要封顶");
        assert!(error.contains("超过上限"), "{error}");
    }

    /// chunked 的预算是**累计**的：只卡单个 chunk 的话，
    /// 一万个 1MB 的 chunk 照样能把内存吃光。
    #[test]
    fn chunked_bodies_share_one_cumulative_budget() {
        let limits = ReadLimits {
            deadline: Instant::now() + Duration::from_secs(30),
            max_bytes: 64,
        };
        // 三个 32 字节的 chunk = 96 字节，每个单独看都在 64 以内。
        let chunk = format!("20\r\n{}\r\n", "y".repeat(32));
        let body = format!("{chunk}{chunk}{chunk}0\r\n\r\n");
        let mut reader = Cursor::new(body.into_bytes());
        let error = read_chunked_body_limited(&mut reader, &limits).expect_err("累计超限必须被拒");
        assert!(error.contains("超过上限"), "{error}");

        // 预算之内的正常 chunked 响应不能被误伤。
        let mut reader = Cursor::new(b"4\r\nabcd\r\n0\r\n\r\n".to_vec());
        assert_eq!(
            read_chunked_body_limited(&mut reader, &limits).unwrap(),
            "abcd"
        );
    }

    /// 总时限和单次读超时是两件事：对端每隔「略小于 timeout」吐一个字节，
    /// 单次读永远不超时，整个请求却可以被无限拖住。
    #[test]
    fn a_blown_overall_deadline_stops_the_read() {
        let limits = ReadLimits {
            deadline: Instant::now() - Duration::from_secs(1),
            max_bytes: MAX_RESPONSE_BYTES,
        };
        let mut reader = Cursor::new(b"HTTP/1.1 200 OK\r\n\r\n".to_vec());
        let error = read_http_response_limited(&mut reader, &limits).expect_err("过了总时限要停");
        assert!(error.contains("总时限"), "{error}");
    }
    use std::io::Cursor;
    use std::sync::mpsc;
    use std::thread;

    fn call_with_id(
        transport: &ScriptedTransport,
        path: &str,
        request_id: &str,
    ) -> Result<(u16, String), String> {
        let body = serde_json::json!({ "request_id": request_id }).to_string();
        request_with_transport(
            transport,
            "POST",
            "agent.test",
            1234,
            path,
            Some(&body),
            Some("secret"),
            Duration::from_secs(1),
        )
    }

    fn call(transport: &ScriptedTransport, path: &str) -> Result<(u16, String), String> {
        call_with_id(transport, path, "r-1")
    }

    #[test]
    fn test_helpers() {
        assert_eq!(
            parse_content_length("HTTP/1.1 200 OK\r\ncontent-length: 12\r\n"),
            Some(12)
        );
        assert_eq!(parse_status("HTTP/1.1 200 OK").unwrap(), 200);
    }

    #[test]
    fn test_roundtrip_with_tiny_http() {
        let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
        let port = server.server_addr().to_ip().unwrap().port();
        std::thread::spawn(move || {
            for mut rq in server.incoming_requests() {
                let mut body = String::new();
                let _ = rq.as_reader().read_to_string(&mut body);
                let resp = tiny_http::Response::from_string(format!("echo:{body}"));
                let _ = rq.respond(resp);
            }
        });
        let (st, body) = post_json_auth(
            "127.0.0.1",
            port,
            "/test",
            "{\"a\":1}",
            "",
            Duration::from_secs(5),
        )
        .unwrap();
        assert_eq!(st, 200);
        assert_eq!(body, "echo:{\"a\":1}");
    }

    #[test]
    fn scripted_drop_request_and_drop_response_are_distinguishable() {
        let transport = ScriptedTransport::new();
        transport.push_for_path("/drop-request", ScriptedExchange::drop_request());
        transport.push_for_path("/drop-response", ScriptedExchange::drop_response());

        assert!(call(&transport, "/drop-request")
            .unwrap_err()
            .contains("Request timed out"));
        assert!(call(&transport, "/drop-response")
            .unwrap_err()
            .contains("Response timed out"));

        let events = transport.events();
        assert!(matches!(events[0], ScriptedEvent::RequestDropped(_)));
        assert!(
            matches!(
                events[1],
                ScriptedEvent::TimedOut {
                    phase: ScriptedPhase::Request,
                    ..
                }
            ),
            "丢请求会耗尽请求方向超时"
        );
        assert!(matches!(events[2], ScriptedEvent::RequestSent(_)));
        assert!(matches!(events[3], ScriptedEvent::ResponseDropped(_)));
        assert!(
            matches!(
                events[4],
                ScriptedEvent::TimedOut {
                    phase: ScriptedPhase::Response,
                    ..
                }
            ),
            "丢响应会耗尽响应方向超时"
        );
        assert_eq!(transport.requests().len(), 2);
    }

    #[test]
    fn scripted_asymmetric_delay_is_applied_in_two_phases() {
        let clock = Arc::new(ManualClock::new());
        let transport = ScriptedTransport::with_clock(clock.clone());
        let exchange = ScriptedExchange::with_delays(
            Duration::from_millis(8),
            Duration::from_millis(12),
            ScriptedOutcome::Response(HttpResponse::new(200, "ok")),
        );
        transport.push(exchange);
        assert_eq!(call(&transport, "/delayed").unwrap(), (200, "ok".into()));
        assert_eq!(clock.elapsed(), Duration::from_millis(20));
        let events = transport.events();
        assert!(matches!(
            events[0],
            ScriptedEvent::PhaseDelayed {
                phase: ScriptedPhase::Request,
                duration,
                ..
            } if duration == Duration::from_millis(8)
        ));
        assert!(matches!(events[1], ScriptedEvent::RequestSent(_)));
        assert!(matches!(
            events[2],
            ScriptedEvent::PhaseDelayed {
                phase: ScriptedPhase::Response,
                duration,
                ..
            } if duration == Duration::from_millis(12)
        ));
        assert!(matches!(events[3], ScriptedEvent::ResponseDelivered { .. }));
    }

    #[test]
    fn scripted_request_matchers_and_gates_make_same_path_reordering_reproducible() {
        let transport = ScriptedTransport::new();
        let slow_gate = Arc::new(ScriptedGate::new());
        let fast_gate = Arc::new(ScriptedGate::new());
        transport.push_for_request(
            "/status",
            "slow-request",
            ScriptedExchange::response(200, "slow").with_response_gate(Arc::clone(&slow_gate)),
        );
        transport.push_for_request(
            "/status",
            "fast-request",
            ScriptedExchange::response(200, "fast").with_response_gate(Arc::clone(&fast_gate)),
        );

        let (tx, rx) = mpsc::channel();
        let mut handles = Vec::new();
        for request_id in ["slow-request", "fast-request"] {
            let tx = tx.clone();
            let transport = transport.clone();
            handles.push(thread::spawn(move || {
                let result = call_with_id(&transport, "/status", request_id).unwrap();
                tx.send((request_id, result.1)).unwrap();
            }));
        }
        assert!(slow_gate.wait_until_reached(Duration::from_secs(1)));
        assert!(fast_gate.wait_until_reached(Duration::from_secs(1)));
        fast_gate.release();
        let first = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(first, ("fast-request", "fast".to_string()));
        slow_gate.release();
        let second = rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert_eq!(second, ("slow-request", "slow".to_string()));
        for handle in handles {
            handle.join().unwrap();
        }
        let response_bodies: Vec<_> = transport
            .events()
            .into_iter()
            .filter_map(|event| match event {
                ScriptedEvent::ResponseDelivered { response, .. } => Some(response.body),
                _ => None,
            })
            .collect();
        assert_eq!(response_bodies, vec!["fast", "slow"]);
    }

    #[test]
    fn scripted_delay_obeys_timeout_without_wall_clock_waiting() {
        let clock = Arc::new(ManualClock::new());
        let transport = ScriptedTransport::with_clock(clock.clone());
        transport.push(ScriptedExchange::with_delays(
            Duration::from_secs(2),
            Duration::ZERO,
            ScriptedOutcome::Response(HttpResponse::new(200, "too-late")),
        ));

        let error = call(&transport, "/timeout").unwrap_err();
        assert!(error.contains("Request timed out"));
        assert_eq!(clock.elapsed(), Duration::from_secs(1));
        assert!(!transport
            .events()
            .iter()
            .any(|event| matches!(event, ScriptedEvent::RequestSent(_))));
    }

    #[test]
    fn scripted_truncated_response_is_rejected_without_false_success() {
        let transport = ScriptedTransport::new();
        transport.push(ScriptedExchange::truncated(200, "{\"ok\":", 32));
        let error = call(&transport, "/truncated").unwrap_err();
        assert!(error.contains("truncated"));
        assert!(transport
            .events()
            .iter()
            .any(|event| matches!(event, ScriptedEvent::ResponseTruncated { .. })));
    }

    #[test]
    fn real_response_parser_rejects_truncated_content_length_body() {
        let raw = b"HTTP/1.1 200 OK\r\nContent-Length: 16\r\n\r\n{\"ok\":";
        let mut reader = BufReader::new(Cursor::new(raw));
        let error = read_http_response(&mut reader).unwrap_err();
        assert!(error.contains("读响应体失败"));
    }

    #[test]
    fn request_wire_format_keeps_auth_and_byte_content_length() {
        let request = HttpRequest::new(
            "POST",
            "example.test",
            1234,
            "/info",
            Some("中文"),
            Some("token"),
        );
        let wire = String::from_utf8(request.wire_bytes()).unwrap();
        assert!(wire.contains("Authorization: Bearer token\r\n"));
        assert!(wire.contains("Content-Length: 6\r\n"));
        assert!(wire.ends_with("\r\n\r\n中文"));
    }
}
