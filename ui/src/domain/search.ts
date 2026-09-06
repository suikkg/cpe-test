/**
 * 页内搜索的**唯一**匹配规则（方案 §11.1）。
 *
 * 只做字面量匹配，大小写不敏感，去掉首尾空格，多段空白分隔的词**各自**都要命中
 * 该对象的至少一个字段（AND）。
 *
 * # 为什么不是正则
 *
 * 这一页上要搜的东西全是 IPv6、点分地址、带括号的驱动描述。把查询当正则，
 * `192.168.` 里的点会匹配任意字符，`fe80::` 里的 `::` 直接是语法错误——
 * 用户敲的是他屏幕上看到的字符串，那就该按字符串比。
 *
 * # 为什么是 AND 而不是 OR
 *
 * 「en0 1000」这种输入的意图是「又要 en0 又要 1000」。OR 会把整张表几乎全留下，
 * 搜了等于没搜。
 */

/** 把查询拆成词。空查询返回空数组，调用方据此判断「没有在搜」。 */
export function queryTerms(query: string): string[] {
  return query.trim().toLowerCase().split(/\s+/).filter(Boolean);
}

/** 这些字段里，是不是每个词都能找到。字段可以是 null / 数字，统一转成字符串。 */
export function matchesTerms(
  terms: string[],
  fields: Array<string | number | null | undefined>,
): boolean {
  if (terms.length === 0) return true;
  const haystack = fields
    .filter((value) => value !== null && value !== undefined && value !== '')
    .map((value) => String(value).toLowerCase())
    .join(' ');
  return terms.every((term) => haystack.includes(term));
}

/**
 * 按查询过滤，**保持原顺序**。
 *
 * 顺序不动是硬要求：网卡表沿用后端 inventory 顺序、单元表沿用服务端 `seq`。
 * 搜索只该控制「看得见哪些行」，不该重排——重排之后同一份数据在搜与不搜时
 * 长得不一样，用户会以为业务数据变了。
 */
export function filterByQuery<T>(
  items: readonly T[],
  query: string,
  fields: (item: T) => Array<string | number | null | undefined>,
): T[] {
  const terms = queryTerms(query);
  if (terms.length === 0) return [...items];
  return items.filter((item) => matchesTerms(terms, fields(item)));
}

/** 「显示 8 / 42 项」。方案 §11.1 指定的写法，几处共用一份，别各写各的。 */
export function visibleCountLabel(shown: number, total: number): string {
  return shown === total ? `${total} 项` : `显示 ${shown} / ${total} 项`;
}
