from pathlib import Path

p = Path('src/master/webui/tests.rs')
s = p.read_text(encoding='utf-8')
fields = '''        ping_small_max_bytes: 0,
        ping_medium_max_bytes: 0,
        ping_wired_small_avg_rtt_ms: 0.0,
        ping_wired_small_max_rtt_ms: 0.0,
        ping_wired_medium_avg_rtt_ms: 0.0,
        ping_wired_medium_max_rtt_ms: 0.0,
        ping_wired_large_avg_rtt_ms: 0.0,
        ping_wired_large_max_rtt_ms: 0.0,
        ping_wifi_small_avg_rtt_ms: 0.0,
        ping_wifi_small_max_rtt_ms: 0.0,
        ping_wifi_medium_avg_rtt_ms: 0.0,
        ping_wifi_medium_max_rtt_ms: 0.0,
        ping_wifi_large_avg_rtt_ms: 0.0,
        ping_wifi_large_max_rtt_ms: 0.0,
'''
needle = '        ping_max_rtt_ms: 0.0,\n'
if s.count(needle) != 1:
    raise SystemExit(f'expected one base request ping_max_rtt_ms, got {s.count(needle)}')
s = s.replace(needle, needle + fields, 1)
needle2 = '        ping_max_rtt_ms: settings["ping_max_rtt_ms"].as_f64().unwrap_or(0.0),\n'
if s.count(needle2) != 1:
    raise SystemExit(f'expected one restored request ping_max_rtt_ms, got {s.count(needle2)}')
s = s.replace(needle2, needle2 + fields, 1)
p.write_text(s, encoding='utf-8')
