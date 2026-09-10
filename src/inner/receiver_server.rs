//! 接收端 server：上行在板侧，下行在网口所在 PC，始终按自有 request/owner 回收。
use super::*;
use crate::protocol::{
    IperfServerStartOut, IperfServerStartReq, IperfServerStopOut, IperfServerStopReq,
};

enum Backend {
    Board(Server),
    Local(Box<iperf::IperfServerMgr>),
    Remote(remote::RemoteLease),
}

pub(super) struct ReceiverServer {
    backend: Backend,
    request: IperfServerStartReq,
    log: PathBuf,
    closed: bool,
}

impl ReceiverServer {
    pub fn start(
        context: &UnitContext<'_>,
        leg: &LegPlan,
        owner: &str,
        log: &Path,
    ) -> Result<Self, String> {
        let request = IperfServerStartReq {
            bind_ip: context.link.local_ip.to_string(),
            port: leg.port,
            request_id: format!("{owner}-server"),
            owner_id: format!("{owner}-server"),
            lease_secs: context.cfg.duration_secs + 150,
            ..Default::default()
        };
        let backend = if leg.flow.receiver_is_board() {
            Backend::Board(Server::start(
                context.adb,
                context.cfg,
                context.link.gateway,
                leg.port,
                owner,
                log,
                context.cancel,
            )?)
        } else if let Some(agent) = &context.agent {
            Backend::Remote(remote::RemoteLease::new(
                remote::Remote::new(agent.clone()),
                &request.owner_id,
            ))
        } else {
            Backend::Local(Box::default())
        };
        let mut server = Self {
            backend,
            request,
            log: log.into(),
            closed: false,
        };
        let started = match &server.backend {
            Backend::Board(_) => Ok(()),
            Backend::Local(mgr) => mgr.start(context.bin, &server.request).map(|_| ()),
            Backend::Remote(lease) => lease
                .remote
                .post::<IperfServerStartOut>("/iperf/server/start", &server.request)
                .map(|_| ()),
        };
        if let Err(error) = started {
            return Err(match server.stop() {
                Ok(()) => error,
                Err(cleanup) => format!("{error}；server 回收未确认：{cleanup}"),
            });
        }
        Ok(server)
    }

    pub fn stop(&mut self) -> Result<(), String> {
        if self.closed {
            return Ok(());
        }
        // 一开始回收就记为已关：**失败也不再来第二遍**。
        //
        // 以前每一条 `?` 都从 `closed = true` 之前返回，于是任何一次回收失败都
        // 会让 `Drop` 再跑一遍整个 `stop()`——对着可能已经不在的资源重发带副
        // 作用的 RPC（/iperf/server/stop 加第二次 /resources/cleanup），并把同
        // 一个故障打印两遍。回收是一次性的收尾，重试要由上层决定，不该藏在
        // 析构里。
        self.closed = true;
        let result = match &mut self.backend {
            Backend::Board(server) => {
                server.stop()?;
                None
            }
            Backend::Local(mgr) => Some(mgr.stop_checked(
                self.request.port,
                &self.request.request_id,
                Duration::ZERO,
            )?),
            Backend::Remote(lease) => {
                let stopped = lease.remote.post::<IperfServerStopOut>(
                    "/iperf/server/stop",
                    &IperfServerStopReq {
                        port: self.request.port,
                        request_id: self.request.request_id.clone(),
                        wait_secs: 0,
                    },
                );
                let cleanup = lease.close();
                // 两条回收路径都执行，停止响应丢失也不留下远端资源。
                //
                // 两个错误都要报：以前是 `stopped?` 先返回，同一台辅测机上的
                // cleanup 失败被静默丢掉——恰恰是「远端还留着资源」这件最需要
                // 让人知道的事，在诊断里一个字都看不到。
                match (stopped, cleanup) {
                    (Ok(out), Ok(())) => Some(out),
                    (stopped, cleanup) => {
                        let mut errors = Vec::new();
                        if let Err(error) = stopped {
                            errors.push(format!("停止 server: {error}"));
                        }
                        if let Err(error) = cleanup {
                            errors.push(format!("回收远端资源: {error}"));
                        }
                        return Err(errors.join("；"));
                    }
                }
            }
        };
        if let Some(out) = result {
            if !out.terminated {
                return Err("PC iperf3 server 回收未确认".into());
            }
            std::fs::write(&self.log, out.output)
                .map_err(|e| format!("保存 PC server 输出失败: {e}"))?;
        }
        Ok(())
    }
}

impl Drop for ReceiverServer {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("内环接收端 server 回收: {error}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::http_client::{HttpRequest, HttpResponse, Transport};
    use std::sync::Mutex;

    struct Agent {
        calls: Mutex<Vec<String>>,
        lose_stop: bool,
    }
    impl Transport for Agent {
        fn send(&self, req: &HttpRequest, _: Duration) -> Result<HttpResponse, String> {
            self.calls.lock().unwrap().push(req.path.clone());
            let body: serde_json::Value = serde_json::from_str(&req.body).unwrap();
            let data = match req.path.as_str() {
                "/iperf/server/stop" => {
                    assert_eq!(body["request_id"], "down-server");
                    assert_eq!(body["port"], 56191);
                    if self.lose_stop {
                        return Err("停止响应丢失".into());
                    }
                    serde_json::json!({"terminated":true,"existed":true,"output":"PC receiver log"})
                }
                "/resources/cleanup" => {
                    assert_eq!(body["owner_id"], "down-server");
                    serde_json::to_value(crate::protocol::ResourceCleanupOut::default()).unwrap()
                }
                path => panic!("unexpected {path}"),
            };
            Ok(HttpResponse::new(200, crate::protocol::ok_json(data)))
        }
    }

    #[test]
    fn pc_server_stop_collects_receiver_log_and_cleans_only_its_owner_even_if_response_is_lost() {
        for lose_stop in [false, true] {
            let agent = Arc::new(Agent {
                calls: Mutex::new(Vec::new()),
                lose_stop,
            });
            let remote = remote::Remote::with_transport(
                config::AgentConfig {
                    id: "pc".into(),
                    address: "test".into(),
                    port: 28801,
                    token: String::new(),
                },
                agent.clone(),
            );
            let log = std::env::temp_dir().join(format!(
                "inner-pc-server-{}-{lose_stop}.log",
                std::process::id()
            ));
            let mut server = ReceiverServer {
                backend: Backend::Remote(remote::RemoteLease::new(remote, "down-server")),
                request: IperfServerStartReq {
                    port: 56191,
                    request_id: "down-server".into(),
                    ..Default::default()
                },
                log: log.clone(),
                closed: false,
            };
            assert_eq!(server.stop().is_err(), lose_stop);
            assert_eq!(
                *agent.calls.lock().unwrap(),
                vec!["/iperf/server/stop", "/resources/cleanup"]
            );
            if !lose_stop {
                assert_eq!(std::fs::read_to_string(&log).unwrap(), "PC receiver log");
            }
            drop(server);
            let _ = std::fs::remove_file(log);
        }
    }
}
