import { describe, expect, it } from 'vitest';
import { emptyPlan, ensureDefaults } from './plan-build';
import { PROJECT_VERSION, parseProject, serializeProject } from './project';
import { emptyGlobals, type UiNicPolicy } from './globals';

describe('项目文件的形状检查', () => {
  it('吃畸形输入不炸，并说清楚是哪一步坏了', () => {
    const cases: Array<[string, string]> = [
      ['', '解析'],
      ['不是 JSON', '解析'],
      ['[]', '对象'],
      ['null', '对象'],
      ['{}', 'project_version'],
      ['{"project_version":"1"}', 'project_version'],
      ['{"project_version":1}', 'ui_plan'],
      ['{"project_version":1,"ui_plan":null}', 'ui_plan'],
      ['{"project_version":1,"ui_plan":{"link_sets":{}}}', 'link_sets'],
      ['{"project_version":1,"ui_plan":{"link_sets":[],"suites":[]}}', 'bindings'],
      [
        '{"project_version":1,"ui_plan":{"link_sets":[],"suites":[],"bindings":[]}}',
        'recipes',
      ],
      [
        '{"project_version":1,"ui_plan":{"link_sets":[],"suites":[],"bindings":[],"recipes":{"tcp":{}}}}',
        'ui_plan.recipes.tcp',
      ],
    ];
    for (const [text, expected] of cases) {
      const result = parseProject(text);
      expect(result.ok, `${text} 不该被接受`).toBe(false);
      expect(result.error, `${text} 的报错要提到 ${expected}`).toContain(expected);
    }
  });

  it('比程序新的版本明确拒绝，并说该怎么办', () => {
    const result = parseProject(
      JSON.stringify({ project_version: PROJECT_VERSION + 1, ui_plan: {} }),
    );
    expect(result.ok).toBe(false);
    expect(result.error).toContain('升级 cpe_test');
  });

  it('导出的项目使用当前格式版本', () => {
    const text = serializeProject(ensureDefaults(emptyPlan()));
    expect(JSON.parse(text).project_version).toBe(3);
  });

  it('**不做语义校验**：端点不存在照样导入得进来', () => {
    // 端点是否存在没有拓扑根本判不了，而项目允许在未连接时导入（在飞机上改
    // 计划是真实场景）。语义错误只能在首次预览时暴露——那是 Rust 侧
    // validate_ui_plan 的职责，前端再写一份就是把重复固化成制度（ADR-11）。
    const plan = ensureDefaults(emptyPlan());
    plan.link_sets = [
      {
        id: 'set-x',
        name: '指向不存在网口的集合',
        pair_refs: [{ id: 'p', src: 'master:NAME=根本没有', dst: 'agent:NAME=也没有' }],
      },
    ];
    const result = parseProject(serializeProject(plan));
    expect(result.ok, '形状没问题就该放行').toBe(true);
    expect(result.plan!.link_sets[0].pair_refs[0].src).toBe('master:NAME=根本没有');
  });
});

describe('导入导出往返', () => {
  it('一份计划写出去再读回来是同一份', () => {
    const plan = ensureDefaults(emptyPlan());
    plan.link_sets = [{ id: 'a', name: 'A', pair_refs: [] }];
    const back = parseProject(serializeProject(plan));
    expect(back.ok).toBe(true);
    expect(back.plan!.suites).toEqual(plan.suites);
    expect(back.plan!.recipes).toEqual(plan.recipes);
    expect(back.plan!.link_sets).toEqual(plan.link_sets);
  });

  it('项目导出带上执行设置、网口策略和 Wi-Fi 门限', () => {
    const plan = ensureDefaults(emptyPlan());
    const policies: UiNicPolicy[] = [
      { endpoint: 'master:NAME=WLAN 3', rx_target: '1000', udp_bandwidth: '1000m', udp_length: '1400' },
    ];
    const settings = {
      duration: 60,
      limit_udp_by_link_speed: true,
      globals: {
        ...emptyGlobals(),
        udp_lengths: ['14k'],
        wifi_band_thresholds: [
          {
            master_band: 'wifi_5g',
            agent_band: 'wifi_2_4g',
            rx_target_master_to_agent_mbps: 700,
            rx_target_agent_to_master_mbps: 420,
            bidir_total_rx_target_mbps: 540,
          },
        ],
        wifi_pair_thresholds: [
          {
            src_endpoint: 'master:NAME=WLAN 5',
            dst_endpoint: 'agent:NAME=WLAN 2',
            rx_target_ab_mbps: 680,
            rx_target_ba_mbps: 420,
            bidir_rx_target_ab_mbps: 300,
            bidir_rx_target_ba_mbps: 210,
          },
        ],
      },
    };
    const back = parseProject(serializeProject(plan, settings, policies));
    expect(back.ok).toBe(true);
    expect(back.settings).toEqual(settings);
    expect(back.nicPolicies).toEqual(policies);
  });

  it('项目文件不携带 RESUME 和截图等本地运行态', () => {
    const text = serializeProject(ensureDefaults(emptyPlan()), {
      duration: 60,
      limit_udp_by_link_speed: false,
      globals: emptyGlobals(),
    });
    expect(text).not.toContain('resume');
    expect(text).not.toContain('screenshot');
  });

  it('导出的文件不含任何口令', () => {
    // 项目文件是要传阅的：agent token 或控制台口令混进去等于当场泄露。
    const text = serializeProject(ensureDefaults(emptyPlan()));
    for (const forbidden of ['token', 'password', 'secret', '口令']) {
      expect(text.toLowerCase(), `导出内容里不该出现 ${forbidden}`).not.toContain(forbidden);
    }
  });
});

describe('废弃字段的迁移', () => {
  it('导入旧项目时把单一 Ping RTT 迁移到有线 small Max RTT', () => {
    const legacy = JSON.stringify({
      project_version: 2,
      ui_plan: ensureDefaults(emptyPlan()),
      settings: { globals: { ...emptyGlobals(), ping_max_rtt_ms: 12.5 } },
    });
    const result = parseProject(legacy);
    expect(result.ok).toBe(true);
    expect(result.settings?.globals).not.toHaveProperty('ping_max_rtt_ms');
    expect(result.settings?.globals?.ping_wired_small_max_rtt_ms).toBe(12.5);
  });

  it('兼容随包旧项目的扁平执行设置，并忽略本地运行态', () => {
    const legacy = {
      project_version: PROJECT_VERSION,
      ui_plan: ensureDefaults(emptyPlan()),
      settings: {
        agent_host: '192.168.0.101',
        duration: 60,
        tcp_windows: ['4m'],
        udp_bandwidths: ['1000m'],
        udp_lengths: ['14k'],
        udp_windows: ['256m'],
        udp_streams: 1,
        ping_count: 180,
        ping_payload_sizes: [32, 1600],
        screenshot: true,
      },
      limit_udp_by_link_speed: true,
      resume: true,
    };
    const result = parseProject(JSON.stringify(legacy));
    expect(result.ok).toBe(true);
    expect(result.settings?.duration).toBe(60);
    expect(result.settings?.limit_udp_by_link_speed).toBe(true);
    expect(result.settings?.globals).toMatchObject({
      tcp_windows: ['4m'],
      udp_bandwidths: ['1000m'],
      udp_lengths: ['14k'],
      udp_windows: ['256m'],
      ping_payload_sizes: [32, 1600],
    });
    expect(result.settings?.globals).not.toHaveProperty('screenshot');
    expect(result.settings).not.toHaveProperty('agent_host');
    expect(result.settings).not.toHaveProperty('resume');
    expect(result.notices).toContain('已兼容旧版项目里的扁平执行设置，并迁移到当前项目格式。');
  });

  it('项目设置里的错误数组不会污染后续 UI 编辑态', () => {
    const raw = {
      project_version: PROJECT_VERSION,
      ui_plan: ensureDefaults(emptyPlan()),
      settings: {
        globals: {
          udp_lengths: '14k',
          ping_payload_sizes: { value: [32] },
          wifi_pair_thresholds: [{ src_endpoint: 'master:NAME=x' }],
        },
      },
    };
    const result = parseProject(JSON.stringify(raw));
    expect(result.ok).toBe(true);
    expect(result.settings?.globals?.udp_lengths).toEqual([]);
    expect(result.settings?.globals?.ping_payload_sizes).toEqual([]);
    expect(result.settings?.globals?.wifi_pair_thresholds).toEqual([]);
  });

  it('导入时抹掉 recipe 的 mode，并告诉用户', () => {
    // mode 是死字段：计划编译器从不读它，fixed 与 scan 产出同一份计划。
    // 服务端现在会明确拒绝非空 mode（ADR-16），而那个字段是**旧版界面自动
    // 写进去的**——让用户为工具自己填的东西去手改 JSON 不合理。
    const legacy = {
      project_version: 1,
      ui_plan: {
        ui_plan_version: 1,
        link_sets: [],
        bindings: [],
        suites: [],
        recipes: {
          tcp: [{ id: 't', name: 'T', mode: 'fixed', profiles: [{ window: '4m' }] }],
          udp: [{ id: 'u', name: 'U', mode: 'scan', profiles: [], bandwidths: ['1000m'] }],
          ping: [],
        },
      },
    };
    const result = parseProject(JSON.stringify(legacy));
    expect(result.ok).toBe(true);
    expect(result.plan!.recipes.tcp[0].mode).toBeUndefined();
    expect(result.plan!.recipes.udp[0].mode).toBeUndefined();
    // 其余字段一个不动。
    expect(result.plan!.recipes.udp[0].bandwidths).toEqual(['1000m']);
    expect(result.notices.join(' ')).toContain('mode');
    expect(result.notices.join(' ')).toContain('2 处');
  });

  it('没有 mode 的项目不产生噪声提示', () => {
    const result = parseProject(serializeProject(ensureDefaults(emptyPlan())));
    expect(result.notices).toHaveLength(0);
  });

  it('空字符串的 mode 也不算，不报噪声', () => {
    const legacy = {
      project_version: 1,
      ui_plan: {
        ui_plan_version: 1,
        link_sets: [],
        bindings: [],
        suites: [],
        recipes: { tcp: [{ id: 't', name: 'T', mode: '', profiles: [] }], udp: [], ping: [] },
      },
    };
    expect(parseProject(JSON.stringify(legacy)).notices).toHaveLength(0);
  });
});

describe('畸形项目文件不抛异常、不产出会让页面崩掉的编辑态', () => {
  /**
   * 这一组是 gptreview 找到的 P2：v2 只检查顶层容器是不是数组，元素一律强转。
   * `suites: [null]` 会在 PlanView 的 computed 读 `suite.tasks` 时抛异常，
   * `recipes.tcp: [null]` 会在剥 `recipe.mode` 时抛异常——**导入用户挑的一个
   * JSON 文件不该能让页面崩掉**。
   */
  const malformed: Array<[string, unknown]> = [
    ['suites 里是 null', { suites: [null] }],
    ['suites 里是字符串', { suites: ['suite-a'] }],
    ['suites 里缺 id', { suites: [{ name: '没有 id' }] }],
    ['tasks 里是 null', { suites: [{ id: 's', tasks: [null] }] }],
    ['tasks 不是数组', { suites: [{ id: 's', tasks: 'task-a' }] }],
    ['recipes.tcp 里是 null', { recipes: { tcp: [null], udp: [], ping: [] } }],
    ['recipes.tcp 里是数字', { recipes: { tcp: [42], udp: [], ping: [] } }],
    [
      'recipe.profiles 里是 null',
      { recipes: { tcp: [{ id: 't', profiles: [null] }], udp: [], ping: [] } },
    ],
    ['bindings 里是 null', { bindings: [null] }],
    ['bindings 缺 suite_id', { bindings: [{ id: 'b', link_set_id: 'l' }] }],
    ['link_sets 里是 null', { link_sets: [null] }],
    ['pair_refs 里是 null', { link_sets: [{ id: 'l', pair_refs: [null] }] }],
    ['重复的 suite id', { suites: [{ id: 's' }, { id: 's' }] }],
    ['order 指向不存在的任务', { suites: [{ id: 's', tasks: [], order: ['nope'] }] }],
    [
      'ping_payload_sizes 里是 NaN / 负数 / 无穷',
      { suites: [{ id: 's', tasks: [{ id: 't', ping_payload_sizes: [NaN, -1, 1e999, 32] }] }] },
    ],
    ['duration 是负数', { suites: [{ id: 's', tasks: [{ id: 't', duration: -5 }] }] }],
  ];

  for (const [label, patch] of malformed) {
    it(`${label}：解析不抛异常，产出的编辑态可以安全遍历`, () => {
      const base = {
        ui_plan_version: 1,
        link_sets: [],
        recipes: { tcp: [], udp: [], ping: [] },
        suites: [],
        bindings: [],
      };
      const file = {
        project_version: PROJECT_VERSION,
        plan: { ...base, ...(patch as Record<string, unknown>) },
      };
      let result!: ReturnType<typeof parseProject>;
      expect(() => {
        result = parseProject(JSON.stringify(file));
      }).not.toThrow();
      expect(result.ok).toBe(true);
      // 页面真正会做的两件事：遍历套件读 tasks、遍历配置读字段。
      expect(() => {
        for (const suite of result.plan!.suites) {
          expect(Array.isArray(suite.tasks)).toBe(true);
          for (const task of suite.tasks) expect(typeof task.id).toBe('string');
        }
        for (const list of Object.values(result.plan!.recipes)) {
          for (const recipe of list) expect(typeof recipe.id).toBe('string');
        }
        for (const set of result.plan!.link_sets) {
          for (const pair of set.pair_refs) expect(typeof pair.src).toBe('string');
        }
      }).not.toThrow();
    });
  }

  it('门限是对象 / NaN / 负数 / 无穷时收敛成 0，不进请求体', () => {
    const file = {
      project_version: PROJECT_VERSION,
      plan: {
        ui_plan_version: 1,
        link_sets: [],
        recipes: { tcp: [], udp: [], ping: [] },
        suites: [],
        bindings: [],
      },
      acceptance: {
        ping_thresholds: {
          ping_wired_small_avg_rtt_ms: { value: 10 },
          ping_wifi_large_max_rtt_ms: NaN,
          ping_small_max_bytes: -128,
        },
        global_rate_targets: { forward: NaN, ab: -1, ba: 900 },
        wifi_band_thresholds: [null, 'x', { master_band: 'wifi_5g' }],
      },
    };
    const result = parseProject(JSON.stringify(file));
    expect(result.ok).toBe(true);
    expect(result.settings?.globals?.ping_wired_small_avg_rtt_ms).toBe(0);
    expect(result.settings?.globals?.ping_wifi_large_max_rtt_ms).toBe(0);
    expect(result.settings?.globals?.ping_small_max_bytes).toBe(0);
    expect(result.settings?.globals?.global_rate_targets).toEqual({
      forward: null,
      ab: null,
      ba: 900,
    });
    expect(result.settings?.globals?.wifi_band_thresholds).toEqual([]);
  });
});

describe('v3 是完整有效快照', () => {
  it('导出的是有效值，不是输入框状态', () => {
    const project = JSON.parse(
      serializeProject(ensureDefaults(emptyPlan()), {
        duration: 180,
        limit_udp_by_link_speed: false,
        globals: {
          ...emptyGlobals(),
          ping_count: 30,
          ping_payload_sizes: [32, 1600],
          tcp_windows: ['4m'],
          tcp_streams: [1],
          udp_streams: 1,
          udp_profiles: [{ bandwidth: '2500m' }],
          global_rate_targets: { forward: null, ab: 900, ba: null },
        },
      }),
    );
    expect(project.execution_defaults.ping.count).toBe(30);
    expect(project.execution_defaults.tcp.windows).toEqual(['4m']);
    expect(project.execution_defaults.udp.profiles).toEqual([{ bandwidth: '2500m' }]);
    // `-l` / `-w` 留空是「明确不下发」，导出时保持空数组，不许回落。
    expect(project.execution_defaults.udp.lengths).toEqual([]);
    expect(project.execution_defaults.udp.windows).toEqual([]);
    expect(project.acceptance.global_rate_targets).toEqual({
      forward: null,
      ab: 900,
      ba: null,
    });
  });

  it('在主控 A 导出、主控 B 导入后，门限与档位逐字段一致', () => {
    const globals = {
      ...emptyGlobals(),
      ping_count: 30,
      ping_payload_sizes: [32, 1600, 65500],
      ping_wired_small_max_rtt_ms: 30,
      ping_wifi_large_max_rtt_ms: 200,
      tcp_windows: ['4m'],
      tcp_streams: [10],
      udp_streams: 1,
      udp_profiles: [{ bandwidth: '1m' }, { bandwidth: '1000m', length: '64' }],
      global_rate_targets: { forward: 1200, ab: null, ba: null },
      wifi_band_thresholds: [
        {
          master_band: 'wifi_5g',
          agent_band: 'wifi_5g',
          rx_target_master_to_agent_mbps: 910,
          rx_target_agent_to_master_mbps: 730,
          bidir_total_rx_target_mbps: 900,
        },
      ],
    };
    const back = parseProject(
      serializeProject(ensureDefaults(emptyPlan()), {
        duration: 180,
        limit_udp_by_link_speed: false,
        globals,
      }),
    );
    expect(back.ok).toBe(true);
    expect(back.settings?.globals).toEqual(globals);
  });

  it('全局门限的「明确没有」与「没意见」是两件事', () => {
    // 三个字段全 null 的对象要活着回来：它是「本项目明确没有全局门限」，
    // 导入方不许回落到自己的 config.json。
    const pinnedEmpty = parseProject(
      serializeProject(ensureDefaults(emptyPlan()), {
        globals: { ...emptyGlobals(), global_rate_targets: { forward: null, ab: null, ba: null } },
      }),
    );
    expect(pinnedEmpty.settings?.globals?.global_rate_targets).toEqual({
      forward: null,
      ab: null,
      ba: null,
    });

    // 旧项目根本没有这个概念：保持 null = 沿用本机配置。
    const legacy = parseProject(
      JSON.stringify({
        project_version: 2,
        ui_plan: ensureDefaults(emptyPlan()),
        settings: { globals: emptyGlobals() },
      }),
    );
    expect(legacy.settings?.globals?.global_rate_targets).toBeNull();
  });
});
