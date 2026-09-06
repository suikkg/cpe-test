import type { NicInfo } from '../api/dto';

/**
 * 网卡的展示与检索规则。纯函数，表格和详情面板共用同一份——两边各写一份的
 * 代价是同一块网卡在列表里和详情里说法不一样，而那种不一致没人会去核对。
 */

/**
 * 选中标识。用**接口名 + IPv4**，不用数组下标。
 *
 * 下标会在重扫之后指向另一块网卡（拔掉一根网线，后面所有行都往前挪一位），
 * 而屏幕上看起来只是「详情自己换了一块卡」。`ifindex` 在 macOS 上恒为 0，
 * 不能单独当键。
 */
export function nicKey(nic: NicInfo): string {
  return `${nic.name}|${nic.ipv4}`;
}

/** 可搜索字段（方案 §11.1 给网卡定的那一行）：接口名、描述、IPv4、IPv6、角色。 */
export function nicSearchFields(nic: NicInfo): Array<string | number | null | undefined> {
  return [
    nic.name,
    nic.description,
    nic.ipv4,
    nic.ipv6_ll,
    nic.ipv6_global,
    nic.role,
    nic.wifi_band,
  ];
}

/**
 * 协商速率的说法。
 *
 * `0` 是「拿不到」，不是「0 Mbps」——写成 0 会让人以为链路挂了，而实际上多半
 * 只是驱动没报。方案 §5.2 明确要求这里写「未获取」。
 */
export function nicSpeedLabel(nic: NicInfo): string {
  return nic.speed_mbps > 0 ? `${nic.speed_mbps} Mbps` : '未获取';
}

/** 带 zone 的 link-local 写法（`fe80::1%en0`）；没有就返回空串。 */
export function nicLinkLocal(nic: NicInfo): string {
  if (!nic.ipv6_ll) return '';
  return nic.zone ? `${nic.ipv6_ll}%${nic.zone}` : nic.ipv6_ll;
}
