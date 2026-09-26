import { describe, expect, it } from 'vitest';
import { backoffDelay, classifyClose, decodeServerFrame, encodeBinaryInput, encodeInput, encodeResize } from './protocol';

describe('decodeServerFrame', () => {
  it('decodes the snapshot text frame', () => {
    expect(decodeServerFrame('{"type":"snapshot","data":"\\u001b[2J$ "}')).toEqual({ type: 'snapshot', data: '\u001b[2J$ ' });
  });
  it('treats binary frames as output', () => {
    const buf = new Uint8Array([104, 105]).buffer;
    const f = decodeServerFrame(buf);
    expect(f.type).toBe('output');
    expect(f.type === 'output' && Array.from(f.data)).toEqual([104, 105]);
  });
  it('ignores malformed or unknown text frames', () => {
    expect(decodeServerFrame('not json').type).toBe('ignored');
    expect(decodeServerFrame('42').type).toBe('ignored');
    expect(decodeServerFrame('{"type":"snapshot","data":5}').type).toBe('ignored');
    expect(decodeServerFrame('{"type":"exit"}').type).toBe('ignored');
  });
});

describe('encoders', () => {
  it('encodes input as UTF-8', () => {
    expect(Array.from(encodeInput('é\r'))).toEqual([0xc3, 0xa9, 0x0d]);
  });
  it('encodes binary input byte-for-byte', () => {
    expect(Array.from(encodeBinaryInput('ÿ\u0000A'))).toEqual([255, 0, 65]);
  });
  it('encodes resize and rejects bad sizes', () => {
    expect(JSON.parse(encodeResize(120, 40))).toEqual({ type: 'resize', cols: 120, rows: 40 });
    expect(() => encodeResize(0, 10)).toThrow(RangeError);
    expect(() => encodeResize(10.5, 10)).toThrow(RangeError);
  });
});

describe('reconnect policy', () => {
  it('classifies close codes', () => {
    expect(classifyClose(1000)).toBe('ended');
    expect(classifyClose(4403)).toBe('forbidden');
    expect(classifyClose(4404)).toBe('not_found');
    expect(classifyClose(1006)).toBe('retry');
  });
  it('backs off exponentially within bounds', () => {
    expect(backoffDelay(0, 500, 15000, () => 0)).toBe(250);
    expect(backoffDelay(0, 500, 15000, () => 1)).toBe(500);
    expect(backoffDelay(3, 500, 15000, () => 1)).toBe(4000);
    expect(backoffDelay(20, 500, 15000, () => 1)).toBe(15000);
    expect(backoffDelay(20, 500, 15000, () => 0)).toBe(7500);
  });
});
