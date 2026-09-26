import { describe, expect, it } from 'vitest';
import { backoffDelay, decodeServerFrame, encodeBinaryInput, encodeInput, encodeResize } from './protocol';

describe('decodeServerFrame', () => {
  it('decodes the snapshot text frame with its size', () => {
    expect(decodeServerFrame('{"type":"snapshot","cols":120,"rows":40,"data":"\\u001bc$ "}')).toEqual({
      type: 'snapshot',
      cols: 120,
      rows: 40,
      data: '\u001bc$ ',
    });
  });
  it('decodes resize and exit', () => {
    expect(decodeServerFrame('{"type":"resize","cols":80,"rows":24}')).toEqual({ type: 'resize', cols: 80, rows: 24 });
    expect(decodeServerFrame('{"type":"exit","status":"completed","exit_code":0}')).toEqual({
      type: 'exit',
      status: 'completed',
      exit_code: 0,
    });
    expect(decodeServerFrame('{"type":"exit","status":"failed","exit_code":null}')).toMatchObject({ exit_code: null });
  });
  it('treats binary frames as output', () => {
    const buf = new Uint8Array([104, 105]).buffer;
    const f = decodeServerFrame(buf);
    expect(f.type).toBe('output');
    expect(f.type === 'output' && Array.from(f.data)).toEqual([104, 105]);
  });
  it('ignores malformed or unknown text frames', () => {
    for (const bad of [
      'not json',
      '42',
      '{"type":"snapshot","data":"x"}',
      '{"type":"snapshot","cols":0,"rows":1,"data":"x"}',
      '{"type":"resize","cols":"80","rows":24}',
      '{"type":"exit","status":"gone","exit_code":0}',
      '{"type":"exit","status":"failed","exit_code":1.5}',
      '{"type":"output"}',
    ]) {
      expect(decodeServerFrame(bad).type, bad).toBe('ignored');
    }
  });
});

describe('encoders', () => {
  it('encodes input as UTF-8', () => {
    expect(Array.from(encodeInput('é\r'))).toEqual([0xc3, 0xa9, 0x0d]);
  });
  it('encodes binary input byte-for-byte', () => {
    expect(Array.from(encodeBinaryInput('ÿ\u0000A'))).toEqual([255, 0, 65]);
  });
  it('encodes resize within the daemon limits', () => {
    expect(JSON.parse(encodeResize(120, 40))).toEqual({ type: 'resize', cols: 120, rows: 40 });
    expect(() => encodeResize(0, 10)).toThrow(RangeError);
    expect(() => encodeResize(10.5, 10)).toThrow(RangeError);
    expect(() => encodeResize(1001, 10)).toThrow(RangeError);
  });
});

describe('reconnect backoff', () => {
  it('backs off exponentially within bounds', () => {
    expect(backoffDelay(0, 500, 15000, () => 0)).toBe(250);
    expect(backoffDelay(0, 500, 15000, () => 1)).toBe(500);
    expect(backoffDelay(3, 500, 15000, () => 1)).toBe(4000);
    expect(backoffDelay(20, 500, 15000, () => 1)).toBe(15000);
    expect(backoffDelay(20, 500, 15000, () => 0)).toBe(7500);
  });
});
