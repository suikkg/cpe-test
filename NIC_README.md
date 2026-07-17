# Rust 如何读取网卡信息（cpe_test 实现详解）

本文档以 `cpe_test` 项目为例，介绍 Rust 在 Windows / macOS / Linux 上如何扫描网卡、获取 IP、速率、WiFi 频段，以及连续采集 RX/TX 流量。

---

## 一、总体架构

```
nic/
├── mod.rs           # scan_host() 入口，调用平台实现
├── classify.rs      # 角色分类（纯逻辑，不分平台）
├── scan_windows.rs  # Windows: ipconfig + GetIfTable2 + netsh
├── scan_macos.rs    # macOS:   ifconfig + system_profiler + networksetup
└── monitor.rs       # RX/TX 连续采样：GetIfTable2 / netstat -ibn / Linux sysfs
```

编译时自动选择：

```rust
// nic/mod.rs
#[cfg(windows)]
pub mod scan_windows;
#[cfg(target_os = "macos")]
pub mod scan_macos;
```

---

## 二、Windows 平台

### 2.1 IP 地址：解析 `ipconfig /all`

不用 PowerShell，不用 wmic，直接解析系统自带命令输出。

**cmd 命令**：
```cmd
ipconfig /all
```

**输出示例**（中文 Windows）：
```
以太网适配器 以太网:

   连接特定的 DNS 后缀 . . . . . . . :
   描述. . . . . . . . . . . . . . . : Realtek PCIe 2.5GbE Family Controller
   本地链接 IPv6 地址. . . . . . . . : fe80::c4b:1a2b:3c4d:5e6f%12(首选)
   IPv4 地址 . . . . . . . . . . . . : 192.168.1.2(首选)
```

**Rust 实现**：

```rust
// src/cmd/ipconfig.rs
pub fn parse(text: &str) -> Vec<IpcfgAdapter> {
    let field_re = Regex::new(r"^\s{2,}(.+?)[\s.]*:\s*(.*)$").unwrap();
    //                                                  ↑
    //                                  ipconfig 的字段格式：
    //                                  "   IPv4 地址 . . . . . . . . : 192.168.1.2(首选)"
    //                                  缩进 + 字段名 + 点号 + 冒号 + 值

    for line in text.lines() {
        if !line.starts_with(' ') && line.ends_with(':') {
            // 适配器头部：非缩进行以冒号结尾
            // "以太网适配器 以太网:" → name = "以太网"
        }
        if let Some(cap) = field_re.captures(line) {
            // 解析字段：key=IPv4地址, val=192.168.1.2(首选)
            // strip_paren() 去括号，looks_ipv4() 验证格式
        }
    }
}
```

**关键细节**：
- 输出是 **GBK 编码**（中文 Windows），Rust 执行 cmd 后用 `encoding_rs::GBK` 解码
- IPv6 的 zone 从 `fe80::xxxx%12` 中提取 `%` 后面的数字（接口索引）
- 可以用 `媒体状态: 已断开` 判断接口是否活跃

### 2.2 速率和类型：`GetIfTable2` Win32 API

这是整个扫描最核心的一步。不用 wmic（Win11 24H2 已移除），直接用 Windows 内置 API。

**需要 crates**：
```toml
[target.'cfg(windows)'.dependencies]
windows = { version = "0.58", features = [
    "Win32_NetworkManagement_IpHelper",
    "Win32_NetworkManagement_Ndis",
] }
```

**Rust 实现**：

```rust
// src/nic/scan_windows.rs
use windows::Win32::NetworkManagement::IpHelper::{
    GetIfTable2, FreeMibTable, MIB_IF_TABLE2
};

pub fn if_rows() -> Vec<IfRow> {
    unsafe {
        let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
        GetIfTable2(&mut table);          // ← 填充指针
        let t = &*table;
        let rows = std::slice::from_raw_parts(
            t.Table.as_ptr(),
            t.NumEntries as usize
        );

        for r in rows {
            // r.Alias          — 接口别名（如"以太网"）
            // r.Description    — 驱动描述
            // r.ReceiveLinkSpeed — 接收速率（bps）
            // r.TransmitLinkSpeed — 发送速率（bps）
            // r.InterfaceIndex — 接口索引
            // r.Type           — 71 = WiFi (IF_TYPE_IEEE80211)
            // r.OperStatus     — 0=down, 1=up
            // r.InOctets       — 累计 RX 字节（监控用）
            // r.OutOctets      — 累计 TX 字节（监控用）
        }
        FreeMibTable(table as *const _);  // ← 释放内存
    }
}
```

**关键细节**：
- `MIB_IF_ROW2.Alias` 是 `[u16]`（UTF-16），需转 Rust String
  ```rust
  fn u16z(buf: &[u16]) -> String {
      let end = buf.iter().position(|c| *c == 0).unwrap_or(buf.len());
      String::from_utf16_lossy(&buf[..end])
  }
  ```
- 速率单位是 bps，除以 1,000,000 得到 Mbps
- 有的驱动会报 int64 max（`9.22e18`），需过滤：
  ```rust
  if speed_bps > 0 && speed_bps < 1_000_000_000_000 {
      speed_bps / 1_000_000
  } else {
      0
  }
  ```

### 2.3 WiFi 频段：`netsh wlan show interfaces`

**cmd 命令**：
```cmd
netsh wlan show interfaces
```

**输出示例**：
```
系统上有 1 个接口:

    名称                   : WLAN
    状态                   : 已连接
    SSID                   : CPE_TEST_5G
    频带                   : 5 GHz
    接收速率(Mbps)         : 866.7
    传输速率(Mbps)         : 866.7
```

**Rust 解析**：

```rust
// src/cmd/netsh.rs
let kv = Regex::new(r"^\s*(.+?)\s*:\s*(.*)$").unwrap();
for line in text.lines() {
    let (key, val) = /* 正则提取 */;
    match key {
        "名称" | "Name"   => { /* 新接口开始 */ }
        "频带" | "Band"   => { band = normalize_band(val); }
        "状态" | "State"  => { connected = val.contains("已连接"); }
        "SSID"            => { ssid = val; }
        _ => {}
    }
}
```

**频段规范化**：
```rust
fn normalize_band(raw: &str) -> String {
    let s = raw.to_lowercase().replace(' ', "");
    if s.contains("2.4") { "2.4GHz" }
    else if s.contains('5') { "5GHz" }
    else if s.contains('6') { "6GHz" }
    else { "" }
}
```

### 2.4 三方数据关联

三份数据通过 **接口名**（ipconfig 适配器名 ≈ netsh 名称 ≈ GetIfTable2.Alias）关联：

```
ipconfig          GetIfTable2         netsh wlan
─────────         ────────────        ──────────
以太网 ─────────── Alias="以太网" ──── (无，有线接口)
                    Speed=2500Mbps
                    InOctets=...

WLAN ──────────── Alias="WLAN" ────── Name="WLAN"
                    Type=71(WiFi)       Band=5GHz
                    Speed=866Mbps       Connected=true
```

**关联代码**：
```rust
for a in ip_adapters {
    let row = getiftable2_rows.get(&a.name);  // 速率/类型/索引
    let wlan = netsh_wlans.get(&a.name);       // 频段/SSID
    let role = classify_role(&row.desc, row.speed, row.is_wifi, wlan.band);
}
```

---

## 三、macOS 平台

macOS 是开发测试环境，网卡扫描走纯命令行。

### 3.1 IP 地址：`ifconfig -a`

```rust
// 解析 ifconfig 输出
// en0: flags=8863<UP,BROADCAST,...> mtu 1500
//     inet 192.168.8.100 netmask 0xffffff00    ← IPv4
//     inet6 fe80::1c2e:...%en0                 ← IPv6 link-local
//     inet6 2408:8207:...                      ← IPv6 global
//     status: active                             ← 连接状态
```

### 3.2 WiFi 判定：`networksetup -listallhardwareports`

```
Hardware Port: Wi-Fi
Device: en0
```
通过 Device 名映射回 `ifconfig` 识别的接口，判定是否为 WiFi。

### 3.3 WiFi 频段：`system_profiler SPAirPortDataType`

```
Current Network Information:
    Channel: 149 (5 GHz, 80MHz)
```
解析 `Channel` 行中的 `5 GHz` / `2.4 GHz` 关键词。

### 3.4 速率：`ifconfig -m <iface>`

```
media: autoselect (1000baseT <full-duplex>)
```
正则提取 `1000baseT` 或 `10GbaseT`，单位换算 Mbps。

---

## 四、角色分类

```rust
// src/nic/classify.rs — 纯逻辑，不分平台
pub fn classify_role(desc: &str, speed_mbps: u64, is_wifi: bool, band: &str) -> String {
    if is_wifi {
        return match band {
            "5GHz"   => "WIFI5G",
            "2.4GHz" => "WIFI2.4G",
            "6GHz"   => "WIFI6G",
            _        => "WIFI",
        };
    }
    let desc_l = desc.to_lowercase();
    if desc_l.contains("usb") && speed_mbps >= 4001 {
        return "10GUSB";  // ← EVB 10G USB 网卡（兼容 4.2G 协商 bug）
    }
    match speed_mbps {
        9000..=12000 => "10GETH",
        2400..=2600  => "SGMII2.5G",
        900..=1100   => "SGMII1G",
        3400..=4000  => "RNDIS",
        _            => "UNKNOWN",
    }
}
```

---

## 五、RX/TX 连续流量监控

UDP 验收不再只在开始和结束各读一次计数器。`MonitorMgr` 从第一条流启动前开始按 200～5000ms 周期连续采样，并为每个样本记录：

```text
elapsed_ms, interval_ms,
rx_bytes, tx_bytes,
rx_delta_bytes, tx_delta_bytes,
rx_mbps, tx_mbps,
valid, error
```

同一个测试单元里，每个唯一端点只启动一个监控器。双向测试共享同一份端点样本，避免两个方向各开一个线程、时间点又不一致。监控起点使用本地 `Instant`，主控把样本加上 monitor start offset 后，与流的 `Traffic/Retry/Ended` 事件放到同一条相对时间线上，不依赖两台电脑系统时钟同步。

### Windows

`GetIfTable2` 同时读取 `MIB_IF_ROW2.InOctets` 和 `OutOctets`：

```rust
pub fn read_counters(iface: &str) -> Result<(u64, u64), String> {
    let row = if_rows()
        .into_iter()
        .find(|row| row.alias == iface)
        .ok_or("接口不存在")?;
    Ok((row.in_octets, row.out_octets))
}
```

相邻两次累计值相减，再除以真实采样间隔：

```rust
let rx_mbps = rx_delta as f64 * 8.0 / secs / 1_000_000.0;
let tx_mbps = tx_delta as f64 * 8.0 / secs / 1_000_000.0;
```

若任一计数器回退（驱动 reset、接口重连或溢出异常），该样本会标记 `valid=false` 并记录旧值、新值；不会用 `saturating_sub` 把异常样本伪装成 0 Mbps 后继续参与判定。

### macOS

`netstat -ibn` 的 `<Link#N>` 行同时取 `Ibytes` 和 `Obytes`。因为 Address 列可能为空、整行会移位，解析器从行尾固定位置取值：`Ibytes = cols[len-5]`，`Obytes = cols[len-2]`。

```text
en0  1500  <Link#14>  aa:bb:cc:dd:ee:ff  ...  9083840014  ...  749169011  0
                                                   Ibytes          Obytes
```

### Linux

直接读取：

```text
/sys/class/net/<iface>/statistics/rx_bytes
/sys/class/net/<iface>/statistics/tx_bytes
```

### ctsTraffic 如何复用同一套 NIC 采样

Windows 10+ 的 ctsTraffic 后端不会另造一套吞吐口径。主控仍在配置所表示的
`src → dst` 接收端启动 `MonitorMgr`，只统计 CTS client 实际运行窗口，排除 server
预热和停止清理时间；正式 RX 仍来自 `GetIfTable2.InOctets`。

ctsTraffic 的原生角色需要特别区分：TCP Push 是 `src` client 发送、`dst` server 接收；
UDP MediaStream 是 `src` server 发送、`dst` client 接收。执行器会按协议反转 UDP 的
client/server 角色，但 NIC monitor 始终跟随用户配置的数据接收端 `dst`，因此报告方向不变。
`streams` 映射成一个 CTS 进程里的 `Connections:N`，而不是 N 个独立监控器。

CTS status/summary 提供连接、字节、速率、frame 和错误诊断。这里必须区分两层证据：

- **是否灌通**只能由 CTS 自身的 rate、bytes、successful frames 等输出确认；NIC RX 有
  背景流量不能证明 CTS 连接已经建立。
- **已建立流是否达到目标吞吐**仍以 client 实际窗口内的 NIC RX、覆盖率和错误/丢帧条件
  判定，CTS 自报速率不会替代 NIC 作为正式吞吐口径。

CTS UDP `Connections:1` 是每方向独立的硬连通门槛，总尝试数为
`max(flow_retries + 1, 3)`。双向 AB、BA 各自拥有完整预算并行执行；每轮必须完整启动
server/client，并确认停止、进程和输出线程均已回收后才能复用端口。全部安全尝试仍没有
CTS 自身测量时记录 `RATE_FAIL / CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`；平台、工具、
非法参数、生命周期或清理异常仍是 `SETUP_ERROR`。一旦某轮已有工具测量，就按该轮真实
运行错误、UDP 丢帧和目标速率判定，不再通过重试掩盖结果。

原始输出保存为 `iperf_outputs/ctstraffic_raw_*.log`，包含各 attempt 的 client/server
输出和生命周期信息；目录名为兼容旧版本保留。

### UDP 单流为何不能只看 NIC

iperf3 UDP 单流和 CTS UDP `Connections:1` 使用相同的硬连通原则：单向 1 流或双向每方向
各 1 流时，每个方向总尝试数均为 `max(flow_retries + 1, 3)`，AB/BA 独立并行。每轮完整
server/client 生命周期和清理都确认后，仍没有所选工具自身的 rate、bytes、frame/datagram
证据，iperf3 返回 `RATE_FAIL / SINGLE_UDP_STREAM_FAILED`，CTS 返回
`RATE_FAIL / CTSTRAFFIC_SINGLE_UDP_STREAM_FAILED`。

NIC 计数器会包含同接口的其他业务和背景流量，所以只能回答“接口实际收发了多少”，不能
单独回答“本轮工具流是否建立”。反过来，工具已证明流建立后，正式产品目标仍看 NIC，而
不是用工具自报速率代替操作系统口径。工具/平台/参数错误、显式取消、server/client 启停
或清理未确认都属于 `SETUP_ERROR`，不能伪装成单流性能失败。

### iperf3 UDP 稳态统计如何使用样本

1. 起流前默认采 3 秒背景流量，以 RX/TX 中位数作为 baseline。
2. 根据最后一次 Retry 后的事件重建活跃区间。优先解析 iperf interval 行内的 `a-b sec`，按 `b-a` 从 Ended 时刻反推真实起点，避免 stdout 块缓冲把 180 秒误缩成几毫秒；解析不到可信区间时才回退到 `Traffic` / `Connected` / `Started`。
3. 双向测试要求两个方向同时达到各自最低活跃流数，取最长连续交集。
4. 交集开头再丢弃默认 5 秒 settle，只截取用户要求的 180 秒作为计分窗口。
5. 正式速率始终取操作系统网卡 RX/TX 计数器；iperf interval 只确定裁剪窗口，iperf 自报 Mbps 不参与网卡平均/P10。
6. 样本先扣 baseline；平均值按样本与窗口的重叠时长加权，覆盖率按时间并集计算，再统计 5 秒滚动 P10、中位、P95、最低和最高。
7. 计数器恢复后的跨周期样本可补回平均值覆盖，但不会参与 5 秒稳定窗口；不足 5 秒时 P10 为空。有目标时 RX/TX 总覆盖率和滚动窗口覆盖率都至少为 95%，否则返回 `NOT_EVALUATED`，不会把采样问题混成 CPE 速率失败。

---

## 六、与 Python 方案的对比

| 操作 | Python 方案 | Rust 方案 | 可靠性 |
|------|-----------|----------|:---:|
| 网卡列表 | `psutil.net_if_addrs()` | `GetIfTable2` API | ✅ API 不会变 |
| 速率 | `wmic nic get Speed` | `GetIfTable2.ReceiveLinkSpeed` | ✅ wmic 已从 Win11 移除 |
| IPv6 fe80 | `psutil` 偶尔丢 | `ipconfig /all` 解析 | ✅ 永远可用 |
| WiFi 频段 | `netsh wlan show` (仅 Windows) | 同左 + `system_profiler` (macOS) | ✅ 同 |
| RX/TX 监控 | `psutil.net_io_counters()`（接口映射易错） | `GetIfTable2.InOctets/OutOctets` + 连续有效性检查 | ✅ API 准 |
| 编码 | 手动 GBK decode | `encoding_rs::GBK` | ✅ 自动 |
| 线程安全 | COM 不能在子线程 | 无 COM，无问题 | ✅ |

核心优势：**每一步都走系统最底层、最稳定的接口，不依赖可能被移除的工具（wmic）或行为不可靠的第三方库（psutil）。**
