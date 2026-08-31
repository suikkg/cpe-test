import { beforeEach, describe, expect, it } from 'vitest';
import { buildRunRequest, plan, previewIsCurrent, reset } from './plan';

/**
 * 执行请求里那几项**跨套件生效**的设置必须真的发出去。
 *
 * 这条守的是一个已经犯过的错：`buildRunRequest` 只发
 * `duration/resume/screenshot + ui_plan`，`nic_policies` 恒为空数组，
 * 全局档位和 `limit_udp_by_link_speed` 一个都不发。后端这几条通路一直是通的
 * （`webui/plan.rs::ui_request_base_config` 全都消费），所以没有任何服务端测试
 * 会红——表现只是「界面上没有这些开关」，而用户以为是设了不生效。
 *
 * 请求体形状同时是 `plan_hash` 那道闸的前提：`/api/plan` 和 `/api/run` 必须发
 * 同一份东西，复核过的计划才等于实际跑的计划。
 */
describe('buildRunRequest', () => {
  beforeEach(reset);

  it('带上按链路上限裁剪速率的开关', () => {
    expect(buildRunRequest().limit_udp_by_link_speed).toBe(false);
    plan.limitUdpByLinkSpeed = true;
    expect(buildRunRequest().limit_udp_by_link_speed).toBe(true);
  });

  /**
   * `udp_streams` 是这一排里唯一**没有「不填」取值**的字段。
   *
   * 服务端是 `#[serde(default = "default_streams")]`（默认 1），校验器又要求
   * 1..=32：发一个显式的 0 会把**整份请求**顶回来（「UDP 流数必须在 1..=32
   * 之间」），于是界面上一个留空的输入框让预览彻底点不动。留空必须是
   * 「不发这个键」，不是「发 0」。
   */
  it('留空的 UDP 并发流不发出去，而不是发 0——发 0 会让整份请求被拒', () => {
    expect('udp_streams' in buildRunRequest()).toBe(false);
    plan.globals.udp_streams = 4;
    expect(buildRunRequest().udp_streams).toBe(4);
  });

  /**
   * 内置默认只钉三格：UDP `-b 2500m`、TCP `-w 4m`、UDP `-l` 留空（不下发）。
   *
   * 它们是按 Windows 调的基线，**与主控 config.json 无关**——那份可能是多档的
   * （`1m/100m/500m/1000m/2500m`），回填进来就是一开局五倍单元数。
   */
  it('开局带内置默认档位，-l 是有意留空的', () => {
    const fresh = buildRunRequest();
    expect(fresh.udp_bandwidths).toEqual(['2500m']);
    expect(fresh.tcp_windows).toEqual(['4m']);
    expect(fresh.udp_lengths).toEqual([]);
  });

  it('全局档位原样进请求体；留空发空，空在后端就是「沿用配置」', () => {
    const empty = buildRunRequest();
    expect(empty.tcp_streams).toEqual([]);
    expect(empty.udp_windows).toEqual([]);

    plan.globals.tcp_windows = ['4m'];
    plan.globals.tcp_streams = [1, 10];
    plan.globals.udp_bandwidths = ['2500m'];
    plan.globals.udp_lengths = ['14k'];
    plan.globals.udp_windows = ['256m'];
    plan.globals.udp_streams = 2;
    plan.globals.ping_count = 5;
    plan.globals.ping_payload_sizes = [32, 1400];

    const filled = buildRunRequest();
    expect(filled.tcp_windows).toEqual(['4m']);
    expect(filled.tcp_streams).toEqual([1, 10]);
    expect(filled.udp_bandwidths).toEqual(['2500m']);
    expect(filled.udp_lengths).toEqual(['14k']);
    expect(filled.udp_windows).toEqual(['256m']);
    expect(filled.udp_streams).toBe(2);
    expect(filled.ping_count).toBe(5);
    expect(filled.ping_payload_sizes).toEqual([32, 1400]);
  });

  it('只发真的填了东西的网卡策略', () => {
    plan.nicPolicies = [
      { endpoint: 'master:NAME=eth0', rx_target: '90%', udp_bandwidth: '', udp_length: '' },
      { endpoint: 'agent:NAME=eth1', rx_target: '', udp_bandwidth: '', udp_length: '' },
    ];
    expect(buildRunRequest().nic_policies).toEqual([
      { endpoint: 'master:NAME=eth0', rx_target: '90%', udp_bandwidth: '', udp_length: '' },
    ]);
  });

  it('矩阵路径已退役：pairs 恒为空', () => {
    expect(buildRunRequest().pairs).toEqual([]);
  });
});

describe('previewIsCurrent', () => {
  beforeEach(reset);

  it('预览后参数没有变化时才允许复用 plan_hash', () => {
    plan.preview = { plan_hash: 'hash' } as typeof plan.preview;
    plan.previewRequestFingerprint = JSON.stringify(buildRunRequest());
    expect(previewIsCurrent()).toBe(true);

    plan.duration += 1;
    expect(previewIsCurrent()).toBe(false);
  });
});
