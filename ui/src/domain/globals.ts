/**
 * 执行请求里那些**跨套件生效**的东西：全局默认档位、按网口的门限/带宽策略。纯函数。
 *
 * 这一层和「意图模型」（`plan-build.ts` 的 UiPlan）是两回事：UiPlan 说的是
 * 「跑哪些链路的哪些套件」，这里说的是「没有更具体的设定时，用什么参数」和
 * 「这个网口作为接收端算多少才算达标」。后端把它们分别收在 `RunRequest` 的顶层
 * 字段（`tcp_windows`/`udp_bandwidths`/…）与 `nic_policies` 里。
 *
 * # 空格子 = 沿用主控的 config.json
 *
 * 后端的口径是 `non_empty(&req.tcp_windows, &cfg.iperf.tcp_windows)`：只有请求里
 * 非空才覆盖配置。所以这里的空值有**确定语义**，不是「还没加载出来」——界面把
 * 配置里当前生效的那份写成灰字 placeholder，看得见但不会被误发回去。
 *
 * **不从 `config.json` 回填**：那份 `udp_profiles` 可能是一份有意不成叉积的列表
 * （比如只有 `2500m/14k` 和 `1000m/1k` 两条，而不是 2×2 四条），把它按
 * 「带宽 × 长度 × 窗口」拆进三个输入框再发回去，就会被展成叉积，单元数悄悄变多。
 *
 * 少数几格有**内置默认值**（见 [`defaultGlobals`]），那是按 Windows 调的基线，
 * 与配置文件无关——同一个控制台在哪台机器上开局都是同一组数。
 */

/**
 * 逗号/空白分隔的档位串 → 数组。
 *
 * 三种分隔符都收（`,`、`、`、空白）：这些值是从 README、聊天记录里粘过来的，
 * 强求一种写法只会让人以为「填了没生效」。
 */
export function parseTokenList(raw: string): string[] {
  return String(raw ?? '')
    .split(/[,，、\s]+/)
    .map((token) => token.trim())
    .filter((token) => token !== '');
}

export function formatTokenList(values: readonly string[]): string {
  return values.join(', ');
}

/** 同上，但收正整数；非数字与 0 直接丢掉（0 流 / 0 包长都不是合法档位）。 */
export function parseNumberList(raw: string): number[] {
  return parseTokenList(raw)
    .map((token) => Number(token))
    .filter((value) => Number.isFinite(value) && value > 0)
    .map((value) => Math.trunc(value));
}

export function formatNumberList(values: readonly number[]): string {
  return values.join(', ');
}

/**
 * 全局默认档位。字段名与 `RunRequest`（`webui/model.rs`）一一对应。
 *
 * TCP 与 UDP 的 `-w` 是**两个独立的输入**：UDP 的挂在每个 udp_profile 上、
 * TCP 的挂在 `iperf.tcp_windows` 上，共用一个框会让两边互相污染。
 */
export interface UiGlobals {
  tcp_windows: string[];
  tcp_streams: number[];
  udp_bandwidths: string[];
  udp_lengths: string[];
  udp_windows: string[];
  /** 0 表示不覆盖；后端把 0 当 1 处理，所以这里用 0 表达「不填」。 */
  udp_streams: number;
  /** 0 = 沿用配置里的 `ping.count`。 */
  ping_count: number;
  /** 空 = 沿用配置里的 `ping.payload_sizes`。 */
  ping_payload_sizes: number[];
}

/** 全空。**只给「读回一份已经跑过的请求」用**（`domain/rerun.ts`）：那时每一格都要
 *  按文件里的原样填，内置默认值会污染重放出来的计划。新建计划请用
 *  [`defaultGlobals`]。 */
export function emptyGlobals(): UiGlobals {
  return {
    tcp_windows: [],
    tcp_streams: [],
    udp_bandwidths: [],
    udp_lengths: [],
    udp_windows: [],
    udp_streams: 0,
    ping_count: 0,
    ping_payload_sizes: [],
  };
}

/**
 * 控制台的**内置默认档位**。
 *
 * 只钉三格，其余留空（= 沿用主控 `config.json`，界面上以灰字显示当前值）：
 *
 * - **UDP `-b` = `2500m`**、**TCP `-w` = `4m`** —— 这是按 Windows 调的基线值。
 *   不从 `config.json` 回填：那份可能是多档的（`1m/100m/500m/1000m/2500m`），
 *   照抄进来就是一开局五倍单元数，而人看着框里那串数字以为「本来就该这样」。
 * - **UDP `-l` 留空 = 不下发 `-l`**，用 iperf3 默认值。「没指定」和「指定成某个
 *   具体值」在报告里是两件事，所以这一格的空是**有意的**，不是忘了填。
 *
 * TCP `-P`、UDP `-w`、UDP 并发流、ping 那几项没有内置值：它们要么本来就该跟着
 * 配置走，要么留空的语义已经够清楚。
 */
export function defaultGlobals(): UiGlobals {
  return {
    ...emptyGlobals(),
    udp_bandwidths: ['2500m'],
    tcp_windows: ['4m'],
  };
}

/** 有没有任何一项真的被填了——用来在界面上说清「这一轮到底覆盖了什么」。 */
export function globalsAreEmpty(globals: UiGlobals): boolean {
  return (
    globals.tcp_windows.length === 0 &&
    globals.tcp_streams.length === 0 &&
    globals.udp_bandwidths.length === 0 &&
    globals.udp_lengths.length === 0 &&
    globals.udp_windows.length === 0 &&
    globals.udp_streams === 0 &&
    globals.ping_count === 0 &&
    globals.ping_payload_sizes.length === 0
  );
}

/**
 * 一个网口在所有配对里共用的判定/负载策略。
 *
 * 与 `webui/model.rs::NicPolicySelection` 同形。`rx_target` 两种写法共用一个框：
 * `1800` = 绝对 1800Mbps，`90%` = 协商速率的 90%。分成两个框会逼着人先想清楚
 * 用哪种，而这两种本来就是二选一。
 *
 * **不在前端校验写法**：语义校验一律交给 `/api/plan` 的报错（ADR-11）。
 * 前端再写一份「看得懂 90% 吗」就是第二份口径，两份迟早对不上。
 */
export interface UiNicPolicy {
  endpoint: string;
  rx_target: string;
  udp_bandwidth: string;
  udp_length: string;
}

export function emptyNicPolicy(endpoint: string): UiNicPolicy {
  return { endpoint, rx_target: '', udp_bandwidth: '', udp_length: '' };
}

export function nicPolicyIsEmpty(policy: UiNicPolicy): boolean {
  return (
    policy.rx_target.trim() === '' &&
    policy.udp_bandwidth.trim() === '' &&
    policy.udp_length.trim() === ''
  );
}

/** 取某个端点的策略；没有就给一份空的（**不写回列表**，读取不该有副作用）。 */
export function policyFor(policies: readonly UiNicPolicy[], endpoint: string): UiNicPolicy {
  return policies.find((policy) => policy.endpoint === endpoint) ?? emptyNicPolicy(endpoint);
}

/**
 * 就地改一项，并把改空了的整条丢掉。
 *
 * 留着空条目不是无害的：后端 `nic_profile()` 对三项全空的返回 `None`，所以行为
 * 上没差别，但导出的项目文件里会攒下一堆空壳，下一个人打开会以为这些网卡「设过
 * 什么又被清掉了」。
 */
export function setNicPolicy(
  policies: readonly UiNicPolicy[],
  endpoint: string,
  patch: Partial<Omit<UiNicPolicy, 'endpoint'>>,
): UiNicPolicy[] {
  const next = { ...policyFor(policies, endpoint), ...patch, endpoint };
  const rest = policies.filter((policy) => policy.endpoint !== endpoint);
  return nicPolicyIsEmpty(next) ? rest : [...rest, next];
}

/** 发给服务端的那一份：只有真的填了东西的条目。 */
export function activeNicPolicies(policies: readonly UiNicPolicy[]): UiNicPolicy[] {
  return policies.filter((policy) => !nicPolicyIsEmpty(policy));
}
