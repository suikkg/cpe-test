import { afterEach, describe, expect, it, vi } from 'vitest';
import { api, NetworkError } from './client';

afterEach(() => { vi.useRealTimers(); vi.unstubAllGlobals(); });

describe('请求正文也是命令响应的一部分', () => {
  it('响应头到了但正文挂住时仍会超时', async () => {
    vi.useFakeTimers();
    vi.stubGlobal('fetch', vi.fn(async (_path, init) => ({
      status: 200, ok: true,
      json: () => new Promise((_resolve, reject) => {
        init.signal.addEventListener('abort', () => reject(init.signal.reason));
      }),
    })));
    const result = expect(api.post('/api/run', {}, { timeoutMs: 50 })).rejects.toBeInstanceOf(NetworkError);
    await vi.advanceTimersByTimeAsync(50);
    await result;
    expect(vi.getTimerCount()).toBe(0);
  });

  it('正文被截断属于结果未知，明确 ok:false 才属于业务失败', async () => {
    vi.stubGlobal('fetch', vi.fn(async () => ({
      status: 200, ok: true, json: async () => { throw new SyntaxError('truncated'); },
    })));
    await expect(api.post('/api/run')).rejects.toBeInstanceOf(NetworkError);
    vi.stubGlobal('fetch', vi.fn(async () => ({
      status: 200, ok: true, json: async () => ({ ok: false, error: '计划过期' }),
    })));
    await expect(api.post('/api/run')).rejects.toThrow('计划过期');
  });
});
