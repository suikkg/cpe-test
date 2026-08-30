import { describe, expect, it } from 'vitest';
import { emptyPlan, ensureDefaults } from './plan-build';
import { PROJECT_VERSION, parseProject, serializeProject } from './project';

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

  it('导出的文件不含任何口令', () => {
    // 项目文件是要传阅的：agent token 或控制台口令混进去等于当场泄露。
    const text = serializeProject(ensureDefaults(emptyPlan()));
    for (const forbidden of ['token', 'password', 'secret', '口令']) {
      expect(text.toLowerCase(), `导出内容里不该出现 ${forbidden}`).not.toContain(forbidden);
    }
  });
});

describe('废弃字段的迁移', () => {
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
