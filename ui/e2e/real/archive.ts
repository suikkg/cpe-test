// 真机验收用的 ZIP / xlsx 读取：只靠 node:zlib，不引入依赖。
//
// 下载包由 `bundle::build_bundle` 写成 Stored + `large_file(true)`：中央目录里的长度是
// 0xFFFFFFFF，真实值在 ZIP64 扩展字段（0x0001）里。xlsx 是普通的 Deflate ZIP。
// 每个条目都按中央目录记录的长度和 CRC 校验——「能下载」不等于「包是完整的」。
import { crc32, inflateRawSync } from 'node:zlib';

export interface ZipEntry { name: string; data: Buffer }

const MAX32 = 0xffffffff;

export function readZip(buf: Buffer): ZipEntry[] {
  let eocd = -1;
  for (let i = buf.length - 22; i >= Math.max(0, buf.length - 22 - 0xffff); i--) {
    if (buf.readUInt32LE(i) === 0x06054b50) { eocd = i; break; }
  }
  if (eocd < 0) throw new Error('不是 ZIP：找不到中央目录结尾');
  let count = buf.readUInt16LE(eocd + 10);
  let ptr = buf.readUInt32LE(eocd + 16);
  if ((count === 0xffff || ptr === MAX32) && eocd >= 20 && buf.readUInt32LE(eocd - 20) === 0x07064b50) {
    const record = Number(buf.readBigUInt64LE(eocd - 12));
    if (buf.readUInt32LE(record) !== 0x06064b50) throw new Error('ZIP64 中央目录结尾损坏');
    count = Number(buf.readBigUInt64LE(record + 32));
    ptr = Number(buf.readBigUInt64LE(record + 48));
  }
  const entries: ZipEntry[] = [];
  for (let n = 0; n < count; n++) {
    if (buf.readUInt32LE(ptr) !== 0x02014b50) throw new Error(`中央目录第 ${n + 1} 项损坏`);
    const method = buf.readUInt16LE(ptr + 10);
    const crc = buf.readUInt32LE(ptr + 16);
    let compressed = buf.readUInt32LE(ptr + 20);
    let size = buf.readUInt32LE(ptr + 24);
    const nameLen = buf.readUInt16LE(ptr + 28);
    const extraLen = buf.readUInt16LE(ptr + 30);
    const commentLen = buf.readUInt16LE(ptr + 32);
    let local = buf.readUInt32LE(ptr + 42);
    const name = buf.subarray(ptr + 46, ptr + 46 + nameLen).toString('utf8');
    // ZIP64 扩展字段只依次放「头里写成 0xFFFFFFFF 的那几项」。
    for (let x = ptr + 46 + nameLen; x < ptr + 46 + nameLen + extraLen;) {
      const id = buf.readUInt16LE(x);
      const len = buf.readUInt16LE(x + 2);
      if (id === 0x0001) {
        let field = x + 4;
        const next = () => { const value = Number(buf.readBigUInt64LE(field)); field += 8; return value; };
        if (size === MAX32) size = next();
        if (compressed === MAX32) compressed = next();
        if (local === MAX32) local = next();
      }
      x += 4 + len;
    }
    if (buf.readUInt32LE(local) !== 0x04034b50) throw new Error(`${name}: 本地文件头损坏`);
    const start = local + 30 + buf.readUInt16LE(local + 26) + buf.readUInt16LE(local + 28);
    const raw = buf.subarray(start, start + compressed);
    let data: Buffer;
    if (method === 0) data = Buffer.from(raw);
    else if (method === 8) data = inflateRawSync(raw);
    else throw new Error(`${name}: 不支持的压缩方式 ${method}`);
    if (data.length !== size || (crc32(data) >>> 0) !== crc) throw new Error(`${name}: 长度或 CRC 与中央目录不符`);
    entries.push({ name, data });
    ptr += 46 + nameLen + extraLen + commentLen;
  }
  return entries;
}

const unescapeXml = (text: string) => text
  .replace(/_x000D_/g, '\r')
  .replace(/&lt;/g, '<').replace(/&gt;/g, '>').replace(/&quot;/g, '"').replace(/&apos;/g, "'").replace(/&amp;/g, '&');

const columnIndex = (ref: string) => [...ref.replace(/\d+$/, '')].reduce((n, ch) => n * 26 + ch.charCodeAt(0) - 64, 0) - 1;

/** 工作表名 → 行（第一行是表头，按列号放回空单元格的位置）。 */
export function readXlsx(buf: Buffer): Map<string, (string | number | null)[][]> {
  const files = new Map(readZip(buf).map(entry => [entry.name, entry.data.toString('utf8')]));
  const shared = [...(files.get('xl/sharedStrings.xml') ?? '').matchAll(/<si>([\s\S]*?)<\/si>/g)]
    .map(match => unescapeXml([...match[1].matchAll(/<t[^>]*>([\s\S]*?)<\/t>/g)].map(t => t[1]).join('')));
  const rels = new Map([...(files.get('xl/_rels/workbook.xml.rels') ?? '').matchAll(/<Relationship\b[^>]*>/g)]
    .map(match => [/Id="([^"]+)"/.exec(match[0])![1], /Target="([^"]+)"/.exec(match[0])![1]]));
  const sheets = new Map<string, (string | number | null)[][]>();
  for (const match of (files.get('xl/workbook.xml') ?? '').matchAll(/<sheet\b[^>]*>/g)) {
    const name = unescapeXml(/name="([^"]+)"/.exec(match[0])![1]);
    const target = rels.get(/r:id="([^"]+)"/.exec(match[0])![1])!.replace(/^\/?(xl\/)?/, 'xl/');
    const rows = [...(files.get(target) ?? '').matchAll(/<row\b[^>]*>([\s\S]*?)<\/row>/g)].map(row => {
      const cells: (string | number | null)[] = [];
      for (const cell of row[1].matchAll(/<c\b([^>]*?)(?:\/>|>([\s\S]*?)<\/c>)/g)) {
        const attrs = cell[1];
        const ref = /r="([A-Z]+\d+)"/.exec(attrs)![1];
        const type = /t="([^"]+)"/.exec(attrs)?.[1];
        const value = /<v>([\s\S]*?)<\/v>/.exec(cell[2] ?? '')?.[1];
        const inline = /<is>([\s\S]*?)<\/is>/.exec(cell[2] ?? '')?.[1];
        cells[columnIndex(ref)] = type === 's' ? shared[Number(value)]
          : type === 'inlineStr' ? unescapeXml([...(inline ?? '').matchAll(/<t[^>]*>([\s\S]*?)<\/t>/g)].map(t => t[1]).join(''))
          : type === 'str' ? unescapeXml(value ?? '')
          : value === undefined ? null : Number(value);
      }
      return Array.from(cells, cell => cell ?? null);
    });
    sheets.set(name, rows);
  }
  return sheets;
}

/** 把表头那一行当键，返回对象数组。 */
export function sheetRecords(rows: (string | number | null)[][]): Record<string, string | number | null>[] {
  const [header = [], ...body] = rows;
  return body.map(row => Object.fromEntries(header.map((title, i) => [String(title), row[i] ?? null])));
}
