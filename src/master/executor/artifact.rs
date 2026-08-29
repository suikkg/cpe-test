//! 落盘产物：iperf 原始日志、网卡逐样本 CSV 及其文件名。
//!
//! 这些文件是事后复核判定的唯一依据（报告里的每个结论都要能回到某一行样本），
//! 所以格式改动必须是自觉的——单独成模块就是为了让改动看得见。

use super::*;

pub(super) struct IperfRawArtifact<'a> {
    pub(super) owner_id: &'a str,
    pub(super) lidx: usize,
    pub(super) stream_pos: usize,
    pub(super) tag: &'a str,
    pub(super) task: &'a IperfTask,
    pub(super) client: &'a IperfClientOut,
    pub(super) server_output: &'a str,
    pub(super) events: &'a [IperfFlowEvent],
    pub(super) error: &'a str,
}

pub(super) fn raw_iperf_filename(
    owner_id: &str,
    lidx: usize,
    stream_pos: usize,
    tag: &str,
    task: &IperfTask,
) -> String {
    format!(
        "iperf_raw_{}_l{:02}_s{:02}_{}_{}_p{}.log",
        sanitize(owner_id),
        lidx,
        stream_pos,
        if task.udp { "udp" } else { "tcp" },
        sanitize(if tag.is_empty() { "oneway" } else { tag }),
        task.port
    )
}

pub(super) fn build_iperf_raw_record(
    task: &IperfTask,
    client: &IperfClientOut,
    server_output: &str,
    events: &[IperfFlowEvent],
    error: &str,
) -> String {
    format!(
        "# CPE iperf3 raw record\n\
# saved_at,{}\n\
# transport,{}\n\
# profile,{}\n\
# source,{} / {} / {}\n\
# destination,{} / {} / {}\n\
# port,{}\n\
# duration_secs,{}\n\
# client_ok,{}\n\
# client_timed_out,{}\n\
# client_cancelled,{}\n\
# error,{}\n\
\n=== CLIENT COMMAND ===\n$ {}\n\
\n=== CLIENT STDOUT+STDERR / ALL ATTEMPTS ===\n{}\n\
\n=== SERVER STDOUT+STDERR / ALL ATTEMPTS ===\n{}\n\
\n=== FLOW EVENTS ===\n{}",
        now_full(),
        if task.udp { "UDP" } else { "TCP" },
        task.profile_label,
        task.src.side.cn(),
        task.src.nic.name,
        task.src.nic.ipv4,
        task.dst.side.cn(),
        task.dst.nic.name,
        task.dst.nic.ipv4,
        task.port,
        task.duration,
        client.ok,
        client.timed_out,
        client.cancelled,
        error.replace(['\r', '\n'], " "),
        client.cmd,
        client.output,
        server_output,
        format_flow_events(events, error)
    )
}

/// `origin_offset_ms` 是把远端（或本地）采样零点对齐到本测试单元时间轴的偏移量。
///
/// 两台机器的系统时钟不要求同步，零点用 RPC 往返做有界估计：真实启动落在
/// `[0, latest_start]` 区间内，取中点，因此**不确定度的半宽正好等于该偏移本身**。
/// 共同有效窗口卡在 180.0/180.0 边界时，没有这个数就无法判断是真够还是对齐
/// 误差凑够的——所以把估计值和它的半宽一起写进表头。
pub(super) fn build_monitor_samples_csv(
    endpoint: &str,
    iface: &str,
    origin_offset_ms: u64,
    out: &MonitorStopOut,
) -> String {
    let mut csv = format!(
        "# CPE OS NIC counter samples\n\
# endpoint,{}\n\
# interface,{}\n\
# origin_offset_ms,{}\n\
# origin_uncertainty_half_width_ms,{}\n\
# full_lifecycle_seconds,{:.6}\n\
# full_lifecycle_average_rx_mbps,{:.6}\n\
# full_lifecycle_average_tx_mbps,{:.6}\n\
elapsed_ms,interval_ms,rx_bytes,tx_bytes,rx_delta_bytes,tx_delta_bytes,rx_mbps,tx_mbps,valid,error\n",
        csv_field(endpoint),
        csv_field(iface),
        origin_offset_ms,
        origin_offset_ms,
        out.seconds,
        out.avg_mbps,
        out.tx_avg_mbps
    );
    for sample in &out.samples {
        csv.push_str(&format!(
            "{},{},{},{},{},{},{:.6},{:.6},{},{}\n",
            sample.elapsed_ms,
            sample.interval_ms,
            sample.rx_bytes,
            sample.tx_bytes,
            sample.rx_delta_bytes,
            sample.tx_delta_bytes,
            sample.rx_mbps,
            sample.tx_mbps,
            sample.valid,
            csv_field(&sample.error)
        ));
    }
    if !out.errors.is_empty() {
        csv.push_str("# monitor_errors\n");
        for error in &out.errors {
            csv.push_str(&format!("# {}\n", csv_field(error)));
        }
    }
    csv
}

impl Ctx {
    pub(super) fn write_output_artifact(
        &self,
        filename: &str,
        contents: &str,
        label: &str,
    ) -> String {
        if let Err(error) = std::fs::create_dir_all(&self.outdir) {
            logln(&format!(
                "    [{label}] 无法创建输出目录 {}: {error}",
                self.outdir.display()
            ));
            return String::new();
        }
        let full = self.outdir.join(filename);
        let tmp = self.outdir.join(format!(".{filename}.tmp"));
        if let Err(error) =
            std::fs::write(&tmp, contents).and_then(|_| std::fs::rename(&tmp, &full))
        {
            let _ = std::fs::remove_file(&tmp);
            logln(&format!(
                "    [{label}] 写入失败 {}: {error}",
                full.display()
            ));
            return String::new();
        }
        logln(&format!("    [{label}] 已保存: {}", full.display()));
        self.outdir
            .file_name()
            .map(|dir| format!("./{}/{}", dir.to_string_lossy(), filename))
            .unwrap_or_else(|| full.to_string_lossy().into_owned())
    }

    pub(super) fn save_iperf_raw_record(&self, artifact: IperfRawArtifact<'_>) -> String {
        let filename = raw_iperf_filename(
            artifact.owner_id,
            artifact.lidx,
            artifact.stream_pos,
            artifact.tag,
            artifact.task,
        );
        let contents = build_iperf_raw_record(
            artifact.task,
            artifact.client,
            artifact.server_output,
            artifact.events,
            artifact.error,
        );
        self.write_output_artifact(&filename, &contents, "原始记录")
    }

    pub(super) fn save_monitor_samples(
        &self,
        owner_id: &str,
        side: Side,
        iface: &str,
        endpoint_identity: &str,
        origin_offset_ms: u64,
        out: &MonitorStopOut,
    ) -> String {
        let side_slug = match side {
            Side::Master => "master",
            Side::Agent => "agent",
        };
        let filename = format!(
            "nic_samples_{}_{}_{}_{}.csv",
            sanitize(owner_id),
            side_slug,
            sanitize(iface),
            &md5_hex(endpoint_identity)[..8]
        );
        let contents = build_monitor_samples_csv(side.cn(), iface, origin_offset_ms, out);
        self.write_output_artifact(&filename, &contents, "网卡原始样本")
    }

    /// 两端都尝试截图，任一成功就保存。返回报告用相对路径（多个用分号隔开）
    pub(super) fn take_screenshots(&self, sides: &[Side], label: &str) -> (String, String) {
        let mut master = String::new();
        let mut agent = String::new();
        for side in sides.iter() {
            let png: Vec<u8> = match side {
                Side::Master => match crate::screenshot::capture_png() {
                    Ok(p) => p,
                    Err(e) => {
                        logln(&format!("    [截图] 主控端截图失败，任务 [{}]: {e}", label));
                        continue;
                    }
                },
                Side::Agent => {
                    let body = match serde_json::to_string(&ScreenshotReq {
                        label: label.to_string(),
                    }) {
                        Ok(body) => body,
                        Err(e) => {
                            logln(&format!("    [截图] 辅测请求序列化失败: {e}"));
                            continue;
                        }
                    };
                    let timeout = Duration::from_secs(180);
                    let (status, text) = match crate::http_client::post_json_auth(
                        &self.agent_host,
                        self.agent_port,
                        "/screenshot",
                        &body,
                        &self.cfg.agent_token,
                        timeout,
                    ) {
                        Ok((s, t)) => {
                            logln(&format!("    [截图] 辅测响应: status={s}, len={}", t.len()));
                            (s, t)
                        }
                        Err(e) => {
                            logln(&format!("    [截图] 辅测请求失败: {e}"));
                            continue;
                        }
                    };
                    if status != 200 {
                        logln(&format!(
                            "    [截图] 辅测 HTTP {status}: {}",
                            text_preview(&text, 200)
                        ));
                        continue;
                    }
                    let resp: Resp<ScreenshotOut> = match serde_json::from_str(&text) {
                        Ok(r) => r,
                        Err(e) => {
                            logln(&format!(
                                "    [截图] JSON解析失败: {e}, raw前100字符: {}",
                                text_preview(&text, 100)
                            ));
                            continue;
                        }
                    };
                    if !resp.ok {
                        logln(&format!(
                            "    [截图] 辅测截图错误: {}",
                            resp.error.unwrap_or_default()
                        ));
                        continue;
                    }
                    let Some(data) = resp.data else {
                        logln("    [截图] 辅测响应缺data");
                        continue;
                    };
                    let b64_len = data.image_b64.len();
                    match base64::engine::general_purpose::STANDARD.decode(data.image_b64) {
                        Ok(p) => p,
                        Err(e) => {
                            logln(&format!(
                                "    [截图] 辅测 base64 解码失败: {e}, len={b64_len}"
                            ));
                            continue;
                        }
                    }
                }
            };
            let (tag, ref mut out_path) = match side {
                Side::Master => ("_master", &mut master),
                Side::Agent => ("_agent", &mut agent),
            };
            let fname = format!(
                "screenshot_{}{}_{}.png",
                sanitize(label),
                tag,
                now_compact()
            );
            let full = self.outdir.join(&fname);
            if let Err(e) = std::fs::write(&full, &png) {
                logln(&format!(
                    "    [截图] {}端截图写入失败 {}: {e}",
                    side.cn(),
                    full.display()
                ));
                continue;
            }
            if let Some(dir_name) = self.outdir.file_name() {
                out_path.clear();
                out_path.push_str(&format!("./{}/{}", dir_name.to_string_lossy(), fname));
                logln(&format!(
                    "    [截图] {}端截图已保存: {}",
                    side.cn(),
                    full.display()
                ));
            } else {
                logln(&format!(
                    "    [截图] {}端截图文件已写入，但输出目录缺少可用目录名: {}",
                    side.cn(),
                    full.display()
                ));
            }
        }
        (master, agent)
    }
}
