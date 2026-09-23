//! 报告里的速率曲线：把逐样本 CSV 画成内联 SVG。
//!
//! # 为什么存在
//!
//! 逐样本 CSV 一直在落盘，报告里也一直给着下载链接。但要回答「这一条是不是
//! 中途掉过速」，读报告的人必须**下载 CSV、开 Excel、自己画**。
//!
//! 而这恰恰是这套判定最想说清楚的那件事。ADR-17 举的例子就是它：一条全程平均
//! 2200Mbps、中间整整断了一分钟的链路。第一版把整套越界判据消音之后，它报出
//! 一个干干净净的 PASS；现在 `rx_acceptance_diagnostics` 会给一行原因码，
//! 但一行「存在持续掉速」和一条塌下去的曲线，对读报告的人不是同一件事。
//!
//! # 为什么是自绘 SVG
//!
//! 单文件分发 + 运行期离线是硬约束（AGENTS.md §0）：报告不许有任何外部
//! JS/CSS/字体，CSP 里也没有外部源。图表库直接违反它。而这里要画的只是一条
//! 折线、一条门限横线和一块判定窗口阴影——手写比引库便宜得多。
//!
//! 前端的实时曲线（`ui/src/components/RateChart.vue` + `domain/monitor-chart.ts`）
//! 是同一个思路的另一份实现。两边**没有共用代码**：一个是 TypeScript 跑在浏览器
//! 里、吃 `/api/monitor/samples` 的实时点，一个是 Rust 跑在报告生成时、吃落盘的
//! CSV。共用要么把 Rust 的东西塞进产物、要么让报告依赖 Node，两条都违反约束。
//!
//! # 降采样必须保峰值
//!
//! 一次 180 秒的灌包有 180 个样本，画在 720 像素宽里绰绰有余；但 11.5 小时的
//! 监控回放可以有几万个点。等距抽样会把「掉到 0 的那一拍」整个抽掉——而那一拍
//! 正是要看的东西。所以按像素列压 min/max，**每一列都保留该列的最低点**。

use super::format::esc;

/// 一个网卡计数器样本里，画图真正用得上的那几项。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct RateSample {
    pub(super) elapsed_ms: u64,
    pub(super) mbps: f64,
    /// 采样本身是否可信。不可信的点**不画**，在曲线上留一个断口——
    /// 把它当成 0 会凭空造出一次掉速，把它插值连过去则会掩盖一次采样失败。
    pub(super) valid: bool,
}

/// 解析 `build_monitor_samples_csv` 写出来的那种 CSV。
///
/// 只认表头里声明过的两列（`elapsed_ms`、`rx_mbps` / `tx_mbps`），按列名定位
/// 而不是按下标——列的顺序是那个函数的实现细节，哪天中间插一列，按下标取
/// 会静默地画出另一条曲线来。
///
/// `#` 开头的是注释行（文件头和 `# monitor_errors` 段），跳过。
/// 任何一行解析不出来就跳过那一行：报告里少一个点，比整条曲线消失好。
pub(super) fn parse_samples_csv(text: &str, value_column: &str) -> Vec<RateSample> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'));
    let Some(header) = lines.next() else {
        return Vec::new();
    };
    let columns: Vec<&str> = header.split(',').map(str::trim).collect();
    let index_of = |name: &str| columns.iter().position(|column| *column == name);
    let (Some(time_at), Some(value_at)) = (index_of("elapsed_ms"), index_of(value_column)) else {
        return Vec::new();
    };
    // `valid` 缺席时按「都可信」处理：旧版本写出来的 CSV 没有这一列。
    let valid_at = index_of("valid");

    let mut out = Vec::new();
    for line in lines {
        let cells: Vec<&str> = line.split(',').collect();
        let (Some(time), Some(value)) = (cells.get(time_at), cells.get(value_at)) else {
            continue;
        };
        let (Ok(elapsed_ms), Ok(mbps)) = (time.trim().parse::<u64>(), value.trim().parse::<f64>())
        else {
            continue;
        };
        if !mbps.is_finite() {
            continue;
        }
        let valid = valid_at
            .and_then(|at| cells.get(at))
            .map(|cell| !cell.trim().eq_ignore_ascii_case("false"))
            .unwrap_or(true);
        out.push(RateSample {
            elapsed_ms,
            mbps,
            valid,
        });
    }
    out.sort_by_key(|sample| sample.elapsed_ms);
    out
}

/// 一列像素上的取值范围。`min` 是这一列的最低点——掉速就藏在它里面。
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Column {
    pub(super) index: usize,
    pub(super) min: f64,
    pub(super) max: f64,
    /// 这一列和上一列之间隔着**不可信样本**，曲线必须在这里断开。
    ///
    /// 不能靠「列号跳了一格以上」推断：一条 60 秒、每秒一拍的腿摊进 560 个
    /// 像素列，每两列之间本来就隔着九格，那样判会把整条曲线拆成散点。
    /// 断口只有一个来源——采样本身不可信（`RateSample::valid`）。
    pub(super) gap_before: bool,
}

/// 按像素列把样本压成 min/max 对。
///
/// **不是等距抽样**：等距抽样会把「掉到 0 的那一拍」整个抽掉，而那一拍正是
/// 这张图存在的理由。每一列都保留该列的最低点和最高点，所以无论横向压缩多少
/// 倍，一次掉速都不会消失。
///
/// 不可信的样本不参与——它们在曲线上是断口，见 [`RateSample::valid`]。
pub(super) fn columns(samples: &[RateSample], width: usize) -> Vec<Column> {
    if samples.is_empty() || width == 0 {
        return Vec::new();
    }
    let first = samples.first().expect("非空").elapsed_ms;
    let last = samples.last().expect("非空").elapsed_ms;
    let span = last.saturating_sub(first);
    let mut out: Vec<Column> = Vec::new();
    // 上一个有效样本之后是不是出现过不可信样本。它就是曲线上那个断口。
    let mut pending_gap = false;
    for sample in samples {
        if !sample.valid {
            pending_gap = true;
            continue;
        }
        // 全部样本落在同一毫秒时（span == 0）压进第 0 列，而不是除零。
        let index = if span == 0 {
            0
        } else {
            let offset = sample.elapsed_ms.saturating_sub(first) as f64 / span as f64;
            ((offset * (width - 1) as f64).round() as usize).min(width - 1)
        };
        // 首列前面没有「上一列」，断口无从谈起。
        let gap_before = pending_gap && !out.is_empty();
        match out.last_mut() {
            Some(column) if column.index == index => {
                column.min = column.min.min(sample.mbps);
                column.max = column.max.max(sample.mbps);
            }
            _ => out.push(Column {
                index,
                min: sample.mbps,
                max: sample.mbps,
                gap_before,
            }),
        }
        pending_gap = false;
    }
    out
}

/// 纵轴上界。
///
/// 取「实测峰值」和「门限」里更大的那个再留 8% 余量——**门限必须画得进来**。
/// 只按实测取的话，一条远远不达标的链路会把门限线顶到画布外面，而那张图恰恰
/// 是要说明「差多少」的。
pub(super) fn axis_max(peak: f64, target: Option<f64>) -> f64 {
    let mut top = peak.max(target.unwrap_or(0.0));
    if !top.is_finite() || top <= 0.0 {
        top = 1.0;
    }
    top * 1.08
}

/// 画图用的全部输入。
pub(super) struct ChartInput<'a> {
    pub(super) samples: &'a [RateSample],
    /// 判定门限；`None` = 这一轮没有门限，不画那条横线。
    pub(super) target_mbps: Option<f64>,
    /// 判定窗口（相对单元 epoch 的毫秒）。两端都在才画阴影。
    ///
    /// 它回答的是报告里一直答不上来的那个问题：**判定用的是 CSV 里的哪一段**。
    /// 采样覆盖率、有效秒和逐样本 CSV 三样都在报告里，却对不上号。
    pub(super) window_start_ms: Option<u64>,
    pub(super) window_end_ms: Option<u64>,
    /// 无障碍描述；渲染成 `<title>`，屏幕阅读器和悬停提示都用它。
    pub(super) caption: &'a str,
    /// 紧凑模式（概览里的一格）：不画刻度文字，只留曲线和门限线。
    pub(super) compact: bool,
}

const PLOT_W: f64 = 720.0;

/// 渲染成内联 SVG；样本不足两个时返回空串（一个点连不成线）。
///
/// 返回空串是**正常路径**：ping 单元没有网卡采样，重放旧目录时 CSV 可能已经
///被删掉，跨平台自测时采样也可能整段失败。调用方据此决定这一格画不画。
pub(super) fn render_svg(input: ChartInput<'_>) -> String {
    let valid: Vec<&RateSample> = input.samples.iter().filter(|s| s.valid).collect();
    if valid.len() < 2 {
        return String::new();
    }
    let plot_h = if input.compact { 40.0 } else { 150.0 };
    let pad_left = if input.compact { 0.0 } else { 58.0 };
    let pad_right = if input.compact { 0.0 } else { 10.0 };
    let pad_top = 6.0;
    let pad_bottom = if input.compact { 2.0 } else { 20.0 };
    let total_w = PLOT_W + pad_left + pad_right;
    let total_h = plot_h + pad_top + pad_bottom;

    let peak = valid.iter().fold(0.0_f64, |acc, s| acc.max(s.mbps));
    let top = axis_max(peak, input.target_mbps);
    let cols = columns(input.samples, PLOT_W as usize);
    if cols.is_empty() {
        return String::new();
    }

    let first_ms = input.samples.first().expect("非空").elapsed_ms;
    let last_ms = input.samples.last().expect("非空").elapsed_ms;
    let span_ms = last_ms.saturating_sub(first_ms).max(1);
    let x_of_ms = |ms: u64| {
        let clamped = ms.clamp(first_ms, last_ms);
        pad_left + (clamped.saturating_sub(first_ms) as f64 / span_ms as f64) * PLOT_W
    };
    let y_of = |mbps: f64| pad_top + plot_h - (mbps / top).clamp(0.0, 1.0) * plot_h;

    let mut svg = String::with_capacity(4 * 1024);
    svg.push_str(&format!(
        "<svg class=\"rate-chart{}\" viewBox=\"0 0 {total_w:.0} {total_h:.0}\" \
         role=\"img\" preserveAspectRatio=\"xMidYMid meet\"><title>{}</title>",
        if input.compact { " compact" } else { "" },
        esc(input.caption)
    ));

    // 判定窗口阴影**画在最底下**：它是背景，不能盖住曲线。
    if let (Some(start), Some(end)) = (input.window_start_ms, input.window_end_ms) {
        if end > start {
            let x0 = x_of_ms(start);
            let x1 = x_of_ms(end);
            if x1 > x0 {
                svg.push_str(&format!(
                    "<rect class=\"win\" x=\"{x0:.1}\" y=\"{:.1}\" width=\"{:.1}\" height=\"{plot_h:.1}\"/>",
                    pad_top,
                    x1 - x0
                ));
            }
        }
    }

    // 绘图区边框与零线。
    svg.push_str(&format!(
        "<rect class=\"frame\" x=\"{pad_left:.1}\" y=\"{pad_top:.1}\" width=\"{PLOT_W:.1}\" height=\"{plot_h:.1}\"/>"
    ));

    // 门限线。虚线 + 与曲线不同的颜色：它是「要求」，不是「测到的」。
    if let Some(target) = input.target_mbps.filter(|t| t.is_finite() && *t > 0.0) {
        let y = y_of(target);
        svg.push_str(&format!(
            "<line class=\"target\" x1=\"{pad_left:.1}\" y1=\"{y:.1}\" x2=\"{:.1}\" y2=\"{y:.1}\"/>",
            pad_left + PLOT_W
        ));
        if !input.compact {
            svg.push_str(&format!(
                "<text class=\"lbl target-lbl\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">门限 {target:.0}</text>",
                pad_left - 5.0,
                y + 3.5
            ));
        }
    }

    // 每一列的 min–max 竖条：掉速就藏在 min 里，等距抽样会把它抽掉。
    //
    // 曲线按断口切成**多条** polyline，而不是一条贯通的。一条贯通的会把
    // 采样中断的两端用直线连起来：整整一分钟没有数据的链路，画出来是一段
    // 平直的健康曲线——恰好和 `RateSample::valid` 说的「它们在曲线上是断口」相反。
    let mut band = String::new();
    let mut segments: Vec<String> = Vec::new();
    let mut line = String::new();
    for column in &cols {
        let x = pad_left + column.index as f64;
        let y_hi = y_of(column.max);
        let y_lo = y_of(column.min);
        if (y_lo - y_hi).abs() > 0.5 {
            band.push_str(&format!(
                "M{x:.1} {y_hi:.1}V{y_lo:.1}",
                x = x,
                y_hi = y_hi,
                y_lo = y_lo
            ));
        }
        if column.gap_before && !line.is_empty() {
            segments.push(std::mem::take(&mut line));
        }
        line.push_str(&format!(
            "{}{x:.1},{:.1}",
            if line.is_empty() { "" } else { " " },
            (y_hi + y_lo) / 2.0
        ));
    }
    if !line.is_empty() {
        segments.push(line);
    }
    if !band.is_empty() {
        svg.push_str(&format!("<path class=\"band\" d=\"{band}\"/>"));
    }
    for segment in &segments {
        // 两个断口之间只剩一个点时补成零长线段：`stroke-linecap: round` 会把它
        // 画成一个圆点。不补的话这一拍在图上完全消失——而「中断之间只测到一拍」
        // 本身就是要看见的信息。
        let points = if segment.contains(' ') {
            segment.clone()
        } else {
            format!("{segment} {segment}")
        };
        svg.push_str(&format!("<polyline class=\"rx\" points=\"{points}\"/>"));
    }

    if !input.compact {
        // 纵轴只标两个数：上界和 0。中间的刻度对「有没有掉过速」没有帮助，
        // 只会把一张小图塞满字。
        svg.push_str(&format!(
            "<text class=\"lbl\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{top:.0}</text>\
             <text class=\"lbl\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">0</text>",
            pad_left - 5.0,
            pad_top + 9.0,
            pad_left - 5.0,
            pad_top + plot_h
        ));
        // 横轴标总时长：曲线的横向尺度必须能对回「这一轮跑了多久」。
        svg.push_str(&format!(
            "<text class=\"lbl\" x=\"{:.1}\" y=\"{:.1}\">0s</text>\
             <text class=\"lbl\" x=\"{:.1}\" y=\"{:.1}\" text-anchor=\"end\">{:.0}s</text>",
            pad_left,
            total_h - 6.0,
            pad_left + PLOT_W,
            total_h - 6.0,
            span_ms as f64 / 1000.0
        ));
    }
    svg.push_str("</svg>");
    svg
}

/// 报告里这张图用到的全部样式。跟着 `report.rs` 的调色板走。
pub(super) const CHART_CSS: &str = "\
.rate-chart { display: block; width: 100%; max-width: 760px; height: auto; }\n\
.rate-chart.compact { max-width: 190px; }\n\
.rate-chart .frame { fill: none; stroke: var(--line); stroke-width: 1; }\n\
.rate-chart .win { fill: #1769aa; fill-opacity: .08; }\n\
.rate-chart .band { stroke: #1769aa; stroke-opacity: .35; stroke-width: 1; fill: none; }\n\
.rate-chart .rx { fill: none; stroke: #1769aa; stroke-width: 1.4; stroke-linejoin: round; stroke-linecap: round; }\n\
.rate-chart .target { stroke: #b3261e; stroke-width: 1.2; stroke-dasharray: 5 4; }\n\
.rate-chart .lbl { fill: var(--muted); font-size: 11px; font-family: inherit; }\n\
.rate-chart .target-lbl { fill: #b3261e; }\n\
.chart-cell { min-width: 150px; }\n\
.chart-note { margin: 4px 0 0; color: var(--muted); font-size: 12px; }\n";

#[cfg(test)]
mod tests {
    use super::*;

    const CSV: &str = "\
# CPE OS NIC counter samples
# endpoint,主控
# interface,eth0
# origin_offset_ms,0
elapsed_ms,interval_ms,rx_bytes,tx_bytes,rx_delta_bytes,tx_delta_bytes,rx_mbps,tx_mbps,valid,error
0,1000,0,0,0,0,1800.000000,10.000000,true,
1000,1000,0,0,0,0,1850.000000,11.000000,true,
2000,1000,0,0,0,0,0.000000,0.000000,true,
3000,1000,0,0,0,0,9999.000000,12.000000,false,采样失败
4000,1000,0,0,0,0,1860.000000,13.000000,true,
";

    #[test]
    fn samples_are_read_by_column_name_not_by_position() {
        let rx = parse_samples_csv(CSV, "rx_mbps");
        assert_eq!(rx.len(), 5);
        assert_eq!(rx[0].elapsed_ms, 0);
        assert!((rx[1].mbps - 1850.0).abs() < 1e-9);
        // 同一份 CSV 换一列就是发送端的曲线——按下标取的话，哪天中间插一列
        // 会静默地画出另一条线来。
        let tx = parse_samples_csv(CSV, "tx_mbps");
        assert!((tx[1].mbps - 11.0).abs() < 1e-9);
    }

    #[test]
    fn an_invalid_sample_is_a_gap_not_a_zero() {
        // 采样失败的那一拍既不能当成 0（凭空造出一次掉速），也不能插值连过去
        // （掩盖一次采样失败）。它只是不画。
        let rx = parse_samples_csv(CSV, "rx_mbps");
        assert!(!rx[3].valid);
        let cols = columns(&rx, 100);
        assert!(
            cols.iter().all(|c| c.max < 9999.0),
            "采样失败那一拍的读数不该进入任何一列——它既不是 0 也不是一个峰值"
        );
        // 但它也没有被当成 0：曲线上剩下的最低点仍然是那次真实的掉速。
        assert!(cols.iter().any(|c| c.min <= 1e-9), "真实的掉速必须还在");
    }

    /// **断口必须真的把曲线断开。**
    ///
    /// 这条守的是模块文档里那句「它们在曲线上是断口」。把无效样本从列表里剔掉
    /// 但仍画成一条贯通的 `<polyline>`，渲染器会把缺口两端用直线连起来：
    /// 一段整整一分钟没有数据的链路，画出来是一条平直的健康曲线。
    #[test]
    fn a_sampling_outage_actually_breaks_the_line_instead_of_being_bridged() {
        let samples: Vec<RateSample> = (0..300)
            .map(|i| RateSample {
                elapsed_ms: i * 1_000,
                // 中段 60 拍采样失败（monitor RPC 挂了），其余全程 2200Mbps。
                valid: !(100..160).contains(&i),
                mbps: 2200.0,
            })
            .collect();
        let cols = columns(&samples, 200);
        let gaps = cols.iter().filter(|c| c.gap_before).count();
        assert_eq!(gaps, 1, "中间断一次，就该只有一个断口");

        let svg = render_svg(ChartInput {
            samples: &samples,
            target_mbps: Some(1800.0),
            window_start_ms: None,
            window_end_ms: None,
            caption: "断口",
            compact: false,
        });
        assert_eq!(
            svg.matches("<polyline class=\"rx\"").count(),
            2,
            "断口两侧必须是两条独立的折线，不能连成一条：{svg}"
        );
    }

    /// 采样稀疏**不是**断口。
    ///
    /// 一条 60 秒、每秒一拍的腿摊进几百个像素列，每两列之间本来就隔着好几格。
    /// 拿「列号跳了」当断口判据的话，整条曲线会被拆成一地散点。
    #[test]
    fn sparse_sampling_is_not_mistaken_for_an_outage() {
        let samples: Vec<RateSample> = (0..60)
            .map(|i| RateSample {
                elapsed_ms: i * 1_000,
                valid: true,
                mbps: 900.0,
            })
            .collect();
        let svg = render_svg(ChartInput {
            samples: &samples,
            target_mbps: None,
            window_start_ms: None,
            window_end_ms: None,
            caption: "稀疏",
            compact: false,
        });
        assert_eq!(
            svg.matches("<polyline class=\"rx\"").count(),
            1,
            "全程有效就该是一条连续折线：{svg}"
        );
    }

    #[test]
    fn downsampling_keeps_the_dropout_no_matter_how_narrow_the_chart_is() {
        // 这条守的就是这张图存在的理由：一条全程 2200Mbps、中间断了一分钟的
        // 链路，横向压到 8 像素时那次掉速**仍然必须看得见**。等距抽样会把它
        // 整个抽掉，而那一拍正是要看的东西。
        let mut samples: Vec<RateSample> = (0..600)
            .map(|i| RateSample {
                elapsed_ms: i * 1000,
                mbps: 2200.0,
                valid: true,
            })
            .collect();
        for sample in samples.iter_mut().skip(300).take(60) {
            sample.mbps = 0.0;
        }
        for width in [8, 40, 720] {
            let cols = columns(&samples, width);
            assert!(
                cols.iter().any(|column| column.min <= 0.001),
                "宽度 {width} 时掉速被抽掉了——保峰值降采样失效"
            );
        }
    }

    #[test]
    fn the_target_line_always_fits_inside_the_chart() {
        // 一条远远不达标的链路（实测 200、门限 1800），如果纵轴只按实测取，
        // 门限线会被顶到画布外面——而这张图恰恰是要说明「差多少」的。
        let top = axis_max(200.0, Some(1800.0));
        assert!(top > 1800.0, "门限必须画得进来，实得上界 {top}");
        // 没有门限时按实测取，留一点余量。
        assert!(axis_max(2200.0, None) > 2200.0);
        // 全 0 或非法值不许让后面除零。
        assert!(axis_max(0.0, None) > 0.0);
        assert!(axis_max(f64::NAN, None) > 0.0);
    }

    #[test]
    fn a_single_point_draws_nothing_instead_of_a_broken_chart() {
        // 一个点连不成线。返回空串是正常路径：ping 单元没有网卡采样，
        // 重放旧目录时 CSV 也可能已经被清掉。
        let one = [RateSample {
            elapsed_ms: 0,
            mbps: 100.0,
            valid: true,
        }];
        assert!(render_svg(ChartInput {
            samples: &one,
            target_mbps: None,
            window_start_ms: None,
            window_end_ms: None,
            caption: "x",
            compact: false,
        })
        .is_empty());
        assert!(render_svg(ChartInput {
            samples: &[],
            target_mbps: None,
            window_start_ms: None,
            window_end_ms: None,
            caption: "x",
            compact: false,
        })
        .is_empty());
    }

    #[test]
    fn the_judgement_window_is_drawn_as_a_band_and_the_caption_is_escaped() {
        let samples = parse_samples_csv(CSV, "rx_mbps");
        let svg = render_svg(ChartInput {
            samples: &samples,
            target_mbps: Some(1800.0),
            window_start_ms: Some(1000),
            window_end_ms: Some(4000),
            caption: "a<b&c",
            compact: false,
        });
        assert!(svg.starts_with("<svg"), "应当直出内联 SVG");
        assert!(svg.contains("class=\"win\""), "判定窗口必须画成阴影带");
        assert!(svg.contains("class=\"target\""), "门限必须画成横线");
        assert!(svg.contains("门限 1800"));
        // caption 是唯一进入 SVG 的外来文本，必须转义。
        assert!(svg.contains("a&lt;b&amp;c") && !svg.contains("a<b&c"));
        // 单文件离线约束：报告里不许出现任何外部引用。
        assert!(!svg.contains("http://") && !svg.contains("https://"));
        assert!(!svg.contains("<script"));
    }

    #[test]
    fn the_compact_variant_drops_the_axis_labels_but_keeps_the_target() {
        let samples = parse_samples_csv(CSV, "rx_mbps");
        let svg = render_svg(ChartInput {
            samples: &samples,
            target_mbps: Some(1800.0),
            window_start_ms: None,
            window_end_ms: None,
            caption: "概览缩略",
            compact: true,
        });
        assert!(svg.contains("class=\"target\""), "缩略图也要有门限线");
        assert!(!svg.contains("门限 1800"), "缩略图不放刻度文字");
        assert!(!svg.contains("0s"), "缩略图不放横轴刻度");
    }

    #[test]
    fn a_csv_without_the_expected_columns_yields_nothing_instead_of_garbage() {
        assert!(parse_samples_csv("", "rx_mbps").is_empty());
        assert!(parse_samples_csv("# 只有注释\n", "rx_mbps").is_empty());
        assert!(parse_samples_csv("a,b\n1,2\n", "rx_mbps").is_empty());
        // 表头对了但数据行残缺：跳过那一行，别把整条曲线扔掉。
        let partial = "elapsed_ms,rx_mbps\n0,100\nbroken\n2000,\n3000,300\n";
        let out = parse_samples_csv(partial, "rx_mbps");
        assert_eq!(out.len(), 2);
        assert!((out[1].mbps - 300.0).abs() < 1e-9);
    }
}
