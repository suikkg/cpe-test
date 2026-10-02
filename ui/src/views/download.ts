/**
 * 把一段文本存成本地文件：Blob + 一次性 `<a download>`。
 *
 * 内环页（报告、配置导出）和历史页（内环历史报告）共用这一份。
 */
export function saveFile(name: string, body: string, type: string): void {
  const url = URL.createObjectURL(new Blob([body], { type }));
  const a = document.createElement('a');
  a.href = url;
  a.download = name;
  a.click();
  setTimeout(() => URL.revokeObjectURL(url), 1000);
}
