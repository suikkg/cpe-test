//! 极简 HTTP/1.1 客户端（零第三方依赖）。
//!
//! 网络访问通过 [`Transport`] 隔离。生产代码使用 [`TcpTransport`]，测试可以使用
//! [`ScriptedTransport`] 注入丢包、延迟和损坏响应，而不需要启动真实 agent。
//! 对端是我们自己的 tiny_http agent。

use crate::clock::{ManualClock, MonotonicClock};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

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

/// 可注入的一次脚本交换。
///
/// `request_delay` 在“请求送达”之前等待，`response_delay` 在生成响应之前等待，
/// 因此可以表达非对称延迟。延迟使用真实 `sleep`，默认只在测试中使用短时值；生产
/// 主流程仍使用 [`TcpTransport`]。
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct ScriptedExchange {
    pub request_delay: Duration,
    pub response_delay: Duration,
    pub outcome: ScriptedOutcome,
    response_gate: Option<Arc<ScriptedGate>>,
}

#[allow(dead_code)]
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

/// [`ScriptedExchange`] 产生的结果。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub enum ScriptedPhase {
    Request,
    Response,
}

/// 显式响应门。工作线程到达后阻塞，测试线程按指定顺序调用 `release`。
#[derive(Debug, Default)]
#[allow(dead_code)]
pub struct ScriptedGate {
    state: Mutex<ScriptedGateState>,
    wake: Condvar,
}

#[derive(Debug, Default)]
struct ScriptedGateState {
    reached: bool,
    released: bool,
}

#[allow(dead_code)]
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

/// 可观测的脚本 transport 事件。事件日志用于验证“请求是否送达”和“响应是否送达”，
/// 这两者在调用方看来都可能只是超时，但资源幂等测试需要区分它们。
#[derive(Clone, Debug, PartialEq, Eq)]
#[allow(dead_code)]
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

#[derive(Default)]
#[allow(dead_code)]
struct ScriptedState {
    queue: VecDeque<ScriptedExchange>,
    by_path: HashMap<String, VecDeque<ScriptedExchange>>,
    by_request: HashMap<(String, String), VecDeque<ScriptedExchange>>,
    events: Vec<ScriptedEvent>,
}

type ScriptedHandler = dyn Fn(&HttpRequest) -> Result<HttpResponse, String> + Send + Sync + 'static;

/// 一个线程安全、可复制的确定性 transport。
///
/// 脚本按先进先出消费。使用 [`ScriptedTransport::push_for_path`] 可以让并发测试按
/// URL 路径分配脚本，从而稳定地构造响应乱序，而不依赖线程启动顺序。
#[derive(Clone)]
#[allow(dead_code)]
pub struct ScriptedTransport {
    state: Arc<Mutex<ScriptedState>>,
    clock: Arc<dyn MonotonicClock>,
    handler: Option<Arc<ScriptedHandler>>,
}

impl Default for ScriptedTransport {
    fn default() -> Self {
        Self::with_clock(Arc::new(ManualClock::new()))
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

    pub fn clear(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.queue.clear();
        state.by_path.clear();
        state.by_request.clear();
        state.events.clear();
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

#[allow(dead_code)]
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
        .next()
        .ok_or_else(|| format!("地址 {addr_str} 无法解析"))?;

    let mut stream = TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT)
        .map_err(|e| format!("连接 {addr_str} 失败: {e}"))?;
    stream
        .set_read_timeout(Some(timeout))
        .and_then(|_| stream.set_write_timeout(Some(Duration::from_secs(30))))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(&request.wire_bytes())
        .map_err(|e| format!("发送请求失败: {e}"))?;

    read_tcp_response(&mut stream)
}

/// 读出一个完整 HTTP 响应后交给公共解析器。按 Content-Length/chunked 提前停止，
/// 避免服务端没有及时关闭连接时把一次成功请求误报成超时。
fn read_tcp_response(stream: &mut TcpStream) -> Result<HttpResponse, String> {
    let mut reader = BufReader::new(stream);
    read_http_response(&mut reader)
}

fn read_http_response<R: BufRead>(reader: &mut R) -> Result<HttpResponse, String> {
    let mut head_lines: Vec<String> = Vec::new();
    let mut line_buf = String::new();
    loop {
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
        read_chunked_body(reader)?
    } else {
        let mut buf = Vec::new();
        if let Some(len) = parse_content_length(&head) {
            buf.resize(len, 0);
            reader
                .read_exact(&mut buf)
                .map_err(|e| format!("读响应体失败: {e}"))?;
        } else {
            reader
                .read_to_end(&mut buf)
                .map_err(|e| format!("读响应体失败: {e}"))?;
        }
        String::from_utf8_lossy(&buf).into_owned()
    };
    Ok(HttpResponse::new(status, body))
}

/// 解码 chunked transfer encoding。
///
/// `pub(crate)` 以便 parser property/fuzz 测试入口（`parser_properties.rs`）
/// 直接覆盖该纯函数。
pub(crate) fn read_chunked_body<R: BufRead>(reader: &mut R) -> Result<String, String> {
    let mut out: Vec<u8> = Vec::new();
    let mut size_buf = String::new();
    loop {
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
    head.lines().find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("content-length")
            .then(|| value.trim().parse().ok())
            .flatten()
    })
}

fn parse_status(line: &str) -> Result<u16, String> {
    line.split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .ok_or_else(|| format!("无法解析状态行: {line}"))
}

#[cfg(test)]
mod tests {
    use super::*;
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
        assert!(parse_status("broken").is_err());

        let mut chunked = std::io::Cursor::new(b"4;foo=bar\r\ntest\r\n3\r\n123\r\n0\r\n\r\n");
        assert_eq!(read_chunked_body(&mut chunked).unwrap(), "test123");
        let mut invalid = std::io::Cursor::new(b"x\r\n");
        assert!(read_chunked_body(&mut invalid).is_err());
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
