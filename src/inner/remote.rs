//! 内环专用 agent 适配器；复用公开协议，不调用子网执行器、不清理其他 owner。
use super::config::AgentConfig;
use crate::protocol::*;
use serde::{de::DeserializeOwned, Serialize};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// 只读轮询的重试预算。与子网 `RELIABLE_HTTP_ATTEMPTS` 同一口径。
const POLL_ATTEMPTS: usize = 3;
const POLL_RETRY_DELAY: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub(super) struct Remote {
    config: AgentConfig,
    transport: Arc<dyn crate::http_client::Transport>,
}
impl Remote {
    #[cfg(test)]
    pub(super) fn with_transport(
        config: AgentConfig,
        transport: Arc<dyn crate::http_client::Transport>,
    ) -> Self {
        Self { config, transport }
    }
    pub fn new(config: AgentConfig) -> Self {
        Self {
            config,
            transport: Arc::new(crate::http_client::TcpTransport),
        }
    }
    pub fn post<T: DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
    ) -> Result<T, String> {
        self.post_with_timeout(path, request, Duration::from_secs(15))
    }
    pub fn post_with_timeout<T: DeserializeOwned>(
        &self,
        path: &str,
        request: &impl Serialize,
        timeout: Duration,
    ) -> Result<T, String> {
        let body = serde_json::to_string(request).map_err(|e| e.to_string())?;
        let (status, text) = crate::http_client::post_json_auth_with_transport(
            self.transport.as_ref(),
            &self.config.address,
            self.config.port,
            path,
            &body,
            &self.config.token,
            timeout,
        )?;
        if status != 200 {
            return Err(format!(
                "辅测机 {} HTTP {status}，请检查连接与令牌",
                self.config.id
            ));
        }
        let response: Resp<T> = serde_json::from_str(&text)
            .map_err(|e| format!("辅测机 {} 响应解析失败: {e}", self.config.id))?;
        if !response.ok {
            return Err(response.error.unwrap_or_else(|| "辅测机执行失败".into()));
        }
        response.data.ok_or_else(|| "辅测机响应缺少 data".into())
    }
    /// 幂等查询专用：失败重试若干次再放弃。与子网执行器同一口径
    /// （`RELIABLE_HTTP_ATTEMPTS` = 3 次，250ms 间隔）。
    ///
    /// 状态轮询每秒一次、一条腿要问几百次，而 [`Self::post`] 的任何一次失败都会
    /// 一路冒泡到 `execute()`，把**整轮**中止——后面的单元一个都不跑了（已完成
    /// 的结果仍进报告，但一次 TCP 抖动就把几小时的计划提前收尾）。子网侧同类
    /// RPC 早就是三次重试，内环这边一次都不重试，同一类抖动在两条链上后果
    /// 完全不同。
    ///
    /// **只给只读查询用**：start/stop 这类带副作用的请求不能盲目重发。
    fn poll<T: DeserializeOwned>(&self, path: &str, request: &impl Serialize) -> Result<T, String> {
        let mut errors = Vec::new();
        for attempt in 1..=POLL_ATTEMPTS {
            match self.post(path, request) {
                Ok(out) => return Ok(out),
                Err(error) => {
                    errors.push(format!("第{attempt}次: {error}"));
                    if attempt < POLL_ATTEMPTS {
                        std::thread::sleep(POLL_RETRY_DELAY);
                    }
                }
            }
        }
        Err(errors.join("；"))
    }
    pub fn info(&self) -> Result<HostInfo, String> {
        let health: HealthOut = self.post("/health", &serde_json::json!({}))?;
        if health.iperf3.is_none()
            || !health
                .capabilities
                .iter()
                .any(|v| v == RELIABLE_LIFECYCLE_CAPABILITY)
        {
            return Err(format!(
                "辅测机 {} 缺少 iperf3 或可靠资源回收协议，请使用同版本 agent",
                self.config.id
            ));
        }
        // 内环总是扫全部接口：请求构造与能力规则都走 `InfoReq`，和子网入口同一份。
        // 提示文案是内环自己的——这里没有「改填 IPv4 前缀」这条退路。
        let request = InfoReq::for_scan(&[]);
        if request.missing_capability(&health.capabilities).is_some() {
            return Err(format!(
                "辅测机 {} 不支持完整网卡扫描，请使用同版本 agent",
                self.config.id
            ));
        }
        self.post("/info", &request)
    }
    pub fn cleanup(&self, owner: &str) -> Result<(), String> {
        let result: ResourceCleanupOut = self.post(
            "/resources/cleanup",
            &ResourceCleanupReq {
                owner_id: owner.into(),
                ..Default::default()
            },
        )?;
        if result.errors.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "辅测机 {} 回收未确认: {}",
                self.config.id,
                result.errors.join("；")
            ))
        }
    }
    pub fn client(
        &self,
        req: IperfClientReq,
        owner: &str,
        epoch: Instant,
        cancel: &AtomicBool,
    ) -> Result<(IperfClientOut, Vec<IperfFlowEvent>), String> {
        let id = format!("{owner}-client");
        let started: IperfClientStartOut = self.post(
            "/iperf/client/start",
            &IperfClientStartReq {
                request: req.clone(),
                request_id: id.clone(),
                owner_id: owner.into(),
                lease_secs: req.duration + 150,
            },
        )?;
        if started.id != id {
            return Err("辅测机返回了不匹配的内环 job ID".into());
        }
        let origin = (epoch.elapsed().as_millis() as u64).saturating_sub(started.elapsed_ms);
        let deadline = Instant::now() + Duration::from_secs(req.duration + 125);
        let mut events = Vec::new();
        let mut cursor = 0;
        loop {
            if cancel.load(Ordering::SeqCst) || Instant::now() >= deadline {
                let stopped: IperfClientStopOut = self.post(
                    "/iperf/client/stop",
                    &IperfClientStopReq {
                        id: id.clone(),
                        wait_secs: 5,
                    },
                )?;
                if !stopped.terminated {
                    return Err("辅测机内环 client 停止未确认".into());
                }
                let mut result = stopped.result.unwrap_or_default();
                result.cancelled = cancel.load(Ordering::SeqCst);
                result.timed_out = !result.cancelled;
                return Ok((result, events));
            }
            let status: IperfClientStatusOut = self.poll(
                "/iperf/client/status",
                &IperfClientStatusReq {
                    id: id.clone(),
                    cursor,
                },
            )?;
            if status.id != id || status.next_cursor < cursor {
                return Err("辅测机内环状态的 job ID/游标不一致".into());
            }
            cursor = status.next_cursor;
            for mut event in status.events {
                event.elapsed_ms = event.elapsed_ms.saturating_add(origin);
                events.push(event);
            }
            if status.done {
                return Ok((status.result.ok_or("辅测机完成状态缺少结果")?, events));
            }
            super::pause(Duration::from_secs(1), cancel);
        }
    }
}

pub(super) struct RemoteLease {
    pub remote: Remote,
    pub owner: String,
    closed: bool,
}
impl RemoteLease {
    pub fn new(remote: Remote, owner: &str) -> Self {
        Self {
            remote,
            owner: owner.into(),
            closed: false,
        }
    }
    pub fn close(&mut self) -> Result<(), String> {
        let out = self.remote.cleanup(&self.owner);
        self.closed = true;
        out
    }
}
impl Drop for RemoteLease {
    fn drop(&mut self) {
        if !self.closed {
            let _ = self.close();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{HttpRequest, HttpResponse, Transport};
    use serde_json::json;
    use std::sync::Mutex;

    struct ScreenshotAgent;
    impl Transport for ScreenshotAgent {
        fn send(&self, req: &HttpRequest, timeout: Duration) -> Result<HttpResponse, String> {
            assert_eq!(req.path, "/screenshot");
            assert_eq!(req.token.as_deref(), Some("test-secret"));
            assert_eq!(timeout, Duration::from_secs(180));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&req.body).unwrap()["label"],
                "inner_unit_1"
            );
            Ok(HttpResponse::new(
                200,
                json!({"ok":true,"data":{"image_b64":"iVBORw0KGgo=","format":"png"}}).to_string(),
            ))
        }
    }
    #[test]
    fn screenshot_uses_authenticated_agent_and_subnet_timeout() {
        let remote = Remote::with_transport(
            AgentConfig {
                id: "agent1".into(),
                address: "agent.example".into(),
                port: 28801,
                token: "test-secret".into(),
            },
            Arc::new(ScreenshotAgent),
        );
        let out: crate::protocol::ScreenshotOut = remote
            .post_with_timeout(
                "/screenshot",
                &crate::protocol::ScreenshotReq {
                    label: "inner_unit_1".into(),
                },
                Duration::from_secs(180),
            )
            .unwrap();
        assert_eq!(out.format, "png");
    }

    struct ScanAgent {
        supports_all: bool,
    }
    impl Transport for ScanAgent {
        fn send(&self, req: &HttpRequest, _: Duration) -> Result<HttpResponse, String> {
            let data = match req.path.as_str() {
                "/health" => {
                    let mut capabilities = vec![RELIABLE_LIFECYCLE_CAPABILITY];
                    if self.supports_all {
                        capabilities.push(UNFILTERED_INFO_CAPABILITY);
                    }
                    json!({"hostname":"remote", "os":"windows", "version":"test", "iperf3":"iperf3", "capabilities":capabilities})
                }
                "/info" => {
                    assert!(self.supports_all, "旧 agent 必须在扫描前被明确拒绝");
                    let request: InfoReq = serde_json::from_str(&req.body).unwrap();
                    let defaults = vec!["192.168.".to_string()];
                    // 真实线协议和 agent 使用的前缀解析不得漏掉私网和 v6-only。
                    let prefixes = request.effective_prefixes(&defaults);
                    assert!(prefixes.is_empty(), "内环全扫不能回落到 agent 默认前缀");
                    for ip in ["192.168.8.2", "10.0.0.2", "172.16.0.2"] {
                        assert!(crate::nic::ipv4_match(ip, prefixes));
                    }
                    serde_json::to_value(HostInfo {
                        interfaces: vec![NicInfo {
                            name: "ETH6".into(),
                            ipv6_ll: "fe80::2".into(),
                            ..Default::default()
                        }],
                        ..Default::default()
                    })
                    .unwrap()
                }
                path => panic!("unexpected request {path}"),
            };
            Ok(HttpResponse::new(
                200,
                json!({"ok":true,"data":data}).to_string(),
            ))
        }
    }

    #[test]
    fn inner_remote_scan_is_unfiltered_and_rejects_agents_that_would_ignore_the_request() {
        for supports_all in [true, false] {
            let remote = Remote::with_transport(
                AgentConfig {
                    id: "agent1".into(),
                    address: "agent.example".into(),
                    port: 28801,
                    token: String::new(),
                },
                Arc::new(ScanAgent { supports_all }),
            );
            if supports_all {
                let info = remote.info().unwrap();
                assert_eq!(info.interfaces[0].ipv6_ll, "fe80::2");
                assert!(info.interfaces[0].ipv4.is_empty());
            } else {
                assert!(remote.info().unwrap_err().contains("不支持完整网卡扫描"));
            }
        }
    }

    struct FakeAgent {
        paths: Mutex<Vec<String>>,
        cancel: bool,
        lose_start_response: bool,
    }
    impl Transport for FakeAgent {
        fn send(&self, req: &HttpRequest, _: Duration) -> Result<HttpResponse, String> {
            self.paths.lock().unwrap().push(req.path.clone());
            assert_eq!(req.token.as_deref(), Some("test-secret"));
            assert_eq!(req.host, "agent.example");
            let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            let data = match req.path.as_str() {
                "/iperf/client/start" => {
                    assert_eq!(body["owner_id"], "inner-test");
                    assert_eq!(body["request_id"], "inner-test-client");
                    assert_eq!(body["request"]["bind_ip"], "192.168.8.101");
                    assert_eq!(body["request"]["dst"], "192.168.8.1");
                    assert_eq!(body["request"]["extra"], json!([]));
                    if self.lose_start_response {
                        return Err("响应丢失".into());
                    }
                    json!({"id":"inner-test-client","elapsed_ms":0})
                }
                "/iperf/client/status" => {
                    assert!(!self.cancel);
                    assert_eq!(body["id"], "inner-test-client");
                    json!({"id":"inner-test-client","done":true,"next_cursor":0,"events":[],"result":IperfClientOut{ok:true,..Default::default()}})
                }
                "/iperf/client/stop" => {
                    assert!(self.cancel);
                    assert_eq!(body["id"], "inner-test-client");
                    serde_json::to_value(IperfClientStopOut {
                        terminated: true,
                        ..Default::default()
                    })
                    .unwrap()
                }
                "/resources/cleanup" => {
                    assert_eq!(body["owner_id"], "inner-test");
                    serde_json::to_value(ResourceCleanupOut::default()).unwrap()
                }
                path => panic!("unexpected request {path}"),
            };
            Ok(HttpResponse::new(
                200,
                json!({"ok":true,"data":data}).to_string(),
            ))
        }
    }

    #[test]
    fn remote_upload_bind_and_owner_cleanup_survive_completion_cancel_and_lost_response() {
        for (cancel, lose_start_response) in [(false, false), (true, false), (false, true)] {
            let transport = Arc::new(FakeAgent {
                paths: Mutex::new(Vec::new()),
                cancel,
                lose_start_response,
            });
            let remote = Remote {
                config: AgentConfig {
                    id: "agent1".into(),
                    address: "agent.example".into(),
                    port: 28801,
                    token: "test-secret".into(),
                },
                transport: transport.clone(),
            };
            let mut lease = RemoteLease::new(remote, "inner-test");
            let result = lease.remote.client(
                IperfClientReq {
                    dst: "192.168.8.1".into(),
                    bind_ip: "192.168.8.101".into(),
                    duration: 6,
                    extra: Vec::new(),
                    ..Default::default()
                },
                "inner-test",
                Instant::now(),
                &AtomicBool::new(cancel),
            );
            if lose_start_response {
                assert!(result.is_err());
            } else {
                let (result, _) = result.unwrap();
                assert_eq!(result.cancelled, cancel);
                assert_eq!(result.ok, !cancel);
            }
            lease.close().unwrap();
            drop(lease);
            let paths = transport.paths.lock().unwrap();
            let expected = if lose_start_response {
                vec!["/iperf/client/start", "/resources/cleanup"]
            } else if cancel {
                vec![
                    "/iperf/client/start",
                    "/iperf/client/stop",
                    "/resources/cleanup",
                ]
            } else {
                vec![
                    "/iperf/client/start",
                    "/iperf/client/status",
                    "/resources/cleanup",
                ]
            };
            assert_eq!(*paths, expected);
        }
    }
}
