/**
 * 「屏幕上这份数据是什么时候的」——纯函数，没有计时器。
 *
 * 单独成模块是因为这句话在四个地方要说得一模一样（本机、辅测、进度、历史），
 * 而它最容易出的错是**没有时刻时随手填一个**：填页面打开时间、填 `new Date()`、
 * 填「刚刚」。那三种都会把「从来没同步过」显示成「刚同步过」，正好把用户最需要
 * 察觉的那一种情况藏起来。所以入口只接受 `number | null`，`null` 有唯一的说法。
 */

/** 本地时钟的时分秒。只用来回答「这份是刚才的吗」，不做跨时区换算。 */
export function clockStamp(at: number | Date): string {
  const date = typeof at === 'number' ? new Date(at) : at;
  return [date.getHours(), date.getMinutes(), date.getSeconds()]
    .map((part) => String(part).padStart(2, '0'))
    .join(':');
}

/**
 * 「最近一次成功是什么时候」。
 *
 * `null` = 这个标签页从来没成功读到过 → 「尚未同步」。**不许**回落成当前时间：
 * 那等于宣称刚刚同步过一次从未发生的成功。
 */
export function freshnessLabel(at: number | null): string {
  return at === null ? '尚未同步' : `${clockStamp(at)} 更新`;
}
