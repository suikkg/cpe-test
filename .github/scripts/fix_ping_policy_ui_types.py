from pathlib import Path

p = Path('ui/src/views/run/GlobalDefaults.vue')
s = p.read_text(encoding='utf-8')
old = '''function positiveDecimal(key: keyof UiGlobals) {
  return computed({
    get: () => (plan.globals[key] > 0 ? String(plan.globals[key]) : ''),
    set: (raw: string) => {
      const value = Number(raw.trim());
      (plan.globals[key] as number) = Number.isFinite(value) && value > 0 ? value : 0;
    },
  });
}
'''
new = '''type PingPolicyNumberKey =
  | 'ping_small_max_bytes'
  | 'ping_medium_max_bytes'
  | 'ping_wired_small_avg_rtt_ms'
  | 'ping_wired_small_max_rtt_ms'
  | 'ping_wired_medium_avg_rtt_ms'
  | 'ping_wired_medium_max_rtt_ms'
  | 'ping_wired_large_avg_rtt_ms'
  | 'ping_wired_large_max_rtt_ms'
  | 'ping_wifi_small_avg_rtt_ms'
  | 'ping_wifi_small_max_rtt_ms'
  | 'ping_wifi_medium_avg_rtt_ms'
  | 'ping_wifi_medium_max_rtt_ms'
  | 'ping_wifi_large_avg_rtt_ms'
  | 'ping_wifi_large_max_rtt_ms';

function positiveDecimal(key: PingPolicyNumberKey) {
  return computed({
    get: () => (plan.globals[key] > 0 ? String(plan.globals[key]) : ''),
    set: (raw: string) => {
      const value = Number(raw.trim());
      plan.globals[key] = Number.isFinite(value) && value > 0 ? value : 0;
    },
  });
}
'''
if old not in s:
    raise SystemExit('positiveDecimal block not found')
s = s.replace(old, new, 1)

anchor = '''const policy = Object.fromEntries(policyKeys.map((key) => [key, positiveDecimal(key)])) as Record<(typeof policyKeys)[number], ReturnType<typeof positiveDecimal>>;
'''
insert = anchor + '''
const policyRows: Array<{ label: string; avgKey: PingPolicyNumberKey; maxKey: PingPolicyNumberKey }> = [
  { label: '有线 small', avgKey: 'ping_wired_small_avg_rtt_ms', maxKey: 'ping_wired_small_max_rtt_ms' },
  { label: '有线 medium', avgKey: 'ping_wired_medium_avg_rtt_ms', maxKey: 'ping_wired_medium_max_rtt_ms' },
  { label: '有线 large', avgKey: 'ping_wired_large_avg_rtt_ms', maxKey: 'ping_wired_large_max_rtt_ms' },
  { label: 'Wi-Fi small', avgKey: 'ping_wifi_small_avg_rtt_ms', maxKey: 'ping_wifi_small_max_rtt_ms' },
  { label: 'Wi-Fi medium', avgKey: 'ping_wifi_medium_avg_rtt_ms', maxKey: 'ping_wifi_medium_max_rtt_ms' },
  { label: 'Wi-Fi large', avgKey: 'ping_wifi_large_avg_rtt_ms', maxKey: 'ping_wifi_large_max_rtt_ms' },
];

function policyPlaceholder(key: PingPolicyNumberKey): string {
  const value = configured.value?.[key];
  return typeof value === 'number' && value > 0 ? String(value) : '';
}
'''
if anchor not in s:
    raise SystemExit('policy anchor not found')
s = s.replace(anchor, insert, 1)

old_template = '''        <template v-for="row in [
          ['有线 small','ping_wired_small_avg_rtt_ms','ping_wired_small_max_rtt_ms'],
          ['有线 medium','ping_wired_medium_avg_rtt_ms','ping_wired_medium_max_rtt_ms'],
          ['有线 large','ping_wired_large_avg_rtt_ms','ping_wired_large_max_rtt_ms'],
          ['Wi-Fi small','ping_wifi_small_avg_rtt_ms','ping_wifi_small_max_rtt_ms'],
          ['Wi-Fi medium','ping_wifi_medium_avg_rtt_ms','ping_wifi_medium_max_rtt_ms'],
          ['Wi-Fi large','ping_wifi_large_avg_rtt_ms','ping_wifi_large_max_rtt_ms'],
        ]" :key="row[0]">
          <label><span>{{ row[0] }} Avg RTT（ms）</span><input v-model="policy[row[1] as keyof typeof policy]" :placeholder="String((configured as any)?.[row[1]] ?? '')" /></label>
          <label><span>{{ row[0] }} Max RTT（ms）</span><input v-model="policy[row[2] as keyof typeof policy]" :placeholder="String((configured as any)?.[row[2]] ?? '')" /></label>
        </template>
'''
new_template = '''        <template v-for="row in policyRows" :key="row.label">
          <label><span>{{ row.label }} Avg RTT（ms）</span><input v-model="policy[row.avgKey]" :placeholder="policyPlaceholder(row.avgKey)" /></label>
          <label><span>{{ row.label }} Max RTT（ms）</span><input v-model="policy[row.maxKey]" :placeholder="policyPlaceholder(row.maxKey)" /></label>
        </template>
'''
if old_template not in s:
    raise SystemExit('policy template block not found')
s = s.replace(old_template, new_template, 1)

# UiGlobals is no longer needed directly after narrowing the key union.
s = s.replace('  type UiGlobals,\n', '')
p.write_text(s, encoding='utf-8')
