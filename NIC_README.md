# Rust 如何读取网卡信息（cpe_test 实现详解）

本文档以 `cpe_test` 项目为例，介绍 Rust 在 Windows / macOS 上如何扫描网卡、获取 IP、速率、WiFi 频段和 RX 流量。

---

## 一、总体架构

```
nic/
├── mod.rs           # scan_host() 入口，调用平台实现
├── classify.rs      # 角色分类（纯逻辑，不分平台）
├── scan_windows.rs  # Windows: ipconfig + GetIfTable2 + netsh
├── scan_macos.rs    # macOS:   ifconfig + system_profiler + networksetup
└── monitor.rs       # RX 监控：GetIfTable2 / netstat -ibn
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

## 五、RX 流量监控

### Windows

直接用已获取的 `MIB_IF_ROW2.InOctets`（累计字节数）：

```rust
// monitor.rs — start/stop 模式
pub fn read_rx_bytes(iface: &str) -> Result<u64, String> {
    let rows = if_rows();  // 再调一次 GetIfTable2
    rows.iter()
        .find(|r| r.alias == iface)
        .map(|r| r.in_octets)
        .ok_or("接口不存在")
}
```

两次数值相减 ÷ 时间 = 平均 Mbps：
```rust
let delta = end_bytes - start_bytes;
let avg_mbps = delta as f64 * 8.0 / secs / 1_000_000.0;
```

### macOS

`netstat -ibn` 的 `<Link#N>` 行取 `Ibytes` 列：

```
en0   1500  <Link#14>   aa:bb:cc:dd:ee:ff   9219567    0  9083840014  ...
                                                ↑Ibytes  ↑Opkts    ↑Obytes
```

---

## 六、与 Python 方案的对比

| 操作 | Python 方案 | Rust 方案 | 可靠性 |
|------|-----------|----------|:---:|
| 网卡列表 | `psutil.net_if_addrs()` | `GetIfTable2` API | ✅ API 不会变 |
| 速率 | `wmic nic get Speed` | `GetIfTable2.ReceiveLinkSpeed` | ✅ wmic 已从 Win11 移除 |
| IPv6 fe80 | `psutil` 偶尔丢 | `ipconfig /all` 解析 | ✅ 永远可用 |
| WiFi 频段 | `netsh wlan show` (仅 Windows) | 同左 + `system_profiler` (macOS) | ✅ 同 |
| RX 监控 | `psutil.net_io_counters()`（丢包） | `GetIfTable2.InOctets` | ✅ API 准 |
| 编码 | 手动 GBK decode | `encoding_rs::GBK` | ✅ 自动 |
| 线程安全 | COM 不能在子线程 | 无 COM，无问题 | ✅ |

核心优势：**每一步都走系统最底层、最稳定的接口，不依赖可能被移除的工具（wmic）或行为不可靠的第三方库（psutil）。**
