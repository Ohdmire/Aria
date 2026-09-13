// 从 icon.png(源图)派生其余图标:128x128.png / 32x32.png /
// icon.ico(256/128/64/48/32/16 多尺寸,PNG payload)。
// 源图为手绘/导出的方形 PNG;无第三方依赖:手写 PNG 解码(zlib inflate +
// 去滤波)+ 盒式面积采样(预乘 alpha)缩放 + PNG/ICO 编码。
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const zlib = require('node:zlib');

const SRC = path.join(__dirname, 'icon.png');

// —— PNG 解码(8-bit 非隔行;truecolor / truecolor+alpha / 调色板) ——
function decodePng(buf) {
  if (buf.subarray(0, 8).toString('hex') !== '89504e470d0a1a0a') {
    throw new Error('不是 PNG 文件');
  }
  let off = 8;
  let ihdr = null;
  const idat = [];
  let plte = null;
  let trns = null;
  while (off + 8 <= buf.length) {
    const len = buf.readUInt32BE(off);
    const type = buf.subarray(off + 4, off + 8).toString('ascii');
    const data = buf.subarray(off + 8, off + 8 + len);
    if (type === 'IHDR') {
      ihdr = {
        width: data.readUInt32BE(0),
        height: data.readUInt32BE(4),
        depth: data[8],
        colorType: data[9],
        interlace: data[12],
      };
    } else if (type === 'IDAT') {
      idat.push(data);
    } else if (type === 'PLTE') {
      plte = data;
    } else if (type === 'tRNS') {
      trns = data;
    } else if (type === 'IEND') {
      break;
    }
    off += 12 + len;
  }
  if (!ihdr) throw new Error('缺少 IHDR');
  const { width, height, depth, colorType, interlace } = ihdr;
  if (depth !== 8) throw new Error(`不支持的位深 ${depth}(需要 8)`);
  if (interlace !== 0) throw new Error('不支持隔行 PNG');
  const channels = { 2: 3, 6: 4, 3: 1 }[colorType];
  if (!channels) throw new Error(`不支持的颜色的类型 ${colorType}`);
  // 去滤波
  const raw = zlib.inflateSync(Buffer.concat(idat));
  const bpp = channels;
  const stride = width * bpp;
  const px = Buffer.alloc(height * stride);
  for (let y = 0; y < height; y++) {
    const filter = raw[y * (stride + 1)];
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const out = px.subarray(y * stride, (y + 1) * stride);
    const prev = y > 0 ? px.subarray((y - 1) * stride, y * stride) : null;
    for (let x = 0; x < stride; x++) {
      const a = x >= bpp ? out[x - bpp] : 0;
      const b = prev ? prev[x] : 0;
      const c = x >= bpp && prev ? prev[x - bpp] : 0;
      let v = line[x];
      if (filter === 1) v += a;
      else if (filter === 2) v += b;
      else if (filter === 3) v += (a + b) >> 1;
      else if (filter === 4) {
        const p = a + b - c;
        const pa = Math.abs(p - a), pb = Math.abs(p - b), pc = Math.abs(p - c);
        v += pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
      }
      out[x] = v & 0xff;
    }
  }
  // 统一转 RGBA
  const rgba = Buffer.alloc(width * height * 4);
  for (let i = 0; i < width * height; i++) {
    if (colorType === 6) {
      rgba.copy(rgba, i * 4, 0, 0); // no-op 占位,下方直接拷
      rgba[i * 4] = px[i * 4];
      rgba[i * 4 + 1] = px[i * 4 + 1];
      rgba[i * 4 + 2] = px[i * 4 + 2];
      rgba[i * 4 + 3] = px[i * 4 + 3];
    } else if (colorType === 2) {
      rgba[i * 4] = px[i * 3];
      rgba[i * 4 + 1] = px[i * 3 + 1];
      rgba[i * 4 + 2] = px[i * 3 + 2];
      rgba[i * 4 + 3] = 255;
    } else {
      const idx = px[i] * 3;
      rgba[i * 4] = plte[idx];
      rgba[i * 4 + 1] = plte[idx + 1];
      rgba[i * 4 + 2] = plte[idx + 2];
      rgba[i * 4 + 3] = trns && px[i] < trns.length ? trns[px[i]] : 255;
    }
  }
  return { width, height, rgba };
}

// —— 盒式面积采样缩放(预乘 alpha;源坐标映射精确到像素边界) ——
function resize(src, dw, dh) {
  const { width: sw, height: sh, rgba } = src;
  const out = Buffer.alloc(dw * dh * 4);
  for (let dy = 0; dy < dh; dy++) {
    const y0 = Math.floor((dy * sh) / dh);
    const y1 = Math.max(y0 + 1, Math.floor(((dy + 1) * sh) / dh));
    for (let dx = 0; dx < dw; dx++) {
      const x0 = Math.floor((dx * sw) / dw);
      const x1 = Math.max(x0 + 1, Math.floor(((dx + 1) * sw) / dw));
      let r = 0, g = 0, b = 0, a = 0, n = 0;
      for (let y = y0; y < y1; y++) {
        for (let x = x0; x < x1; x++) {
          const o = (y * sw + x) * 4;
          const al = rgba[o + 3] / 255;
          r += rgba[o] * al; g += rgba[o + 1] * al; b += rgba[o + 2] * al;
          a += rgba[o + 3]; n++;
        }
      }
      const o = (dy * dw + dx) * 4;
      const avgA = a / n;
      const al = avgA / 255;
      out[o] = al > 0 ? Math.round(r / n / al) : 0;
      out[o + 1] = al > 0 ? Math.round(g / n / al) : 0;
      out[o + 2] = al > 0 ? Math.round(b / n / al) : 0;
      out[o + 3] = Math.round(avgA);
    }
  }
  return out;
}

// —— PNG 编码(RGBA,filter: None) ——
const CRC_TABLE = (() => {
  const t = new Uint32Array(256);
  for (let n = 0; n < 256; n++) {
    let c = n;
    for (let k = 0; k < 8; k++) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    t[n] = c >>> 0;
  }
  return t;
})();
const crc32 = (buf) => {
  let c = 0xffffffff;
  for (const b of buf) c = CRC_TABLE[(c ^ b) & 0xff] ^ (c >>> 8);
  return (c ^ 0xffffffff) >>> 0;
};
const chunk = (type, data) => {
  const out = Buffer.alloc(12 + data.length);
  out.writeUInt32BE(data.length, 0);
  out.write(type, 4, 'ascii');
  data.copy(out, 8);
  out.writeUInt32BE(crc32(out.subarray(4, 8 + data.length)), 8 + data.length);
  return out;
};
const encodePng = (px, size) => {
  const raw = Buffer.alloc(size * (size * 4 + 1));
  for (let y = 0; y < size; y++) {
    raw[y * (size * 4 + 1)] = 0; // filter: None
    px.copy(raw, y * (size * 4 + 1) + 1, y * size * 4, (y + 1) * size * 4);
  }
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0); ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; ihdr[9] = 6; // 8-bit RGBA
  return Buffer.concat([
    Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]),
    chunk('IHDR', ihdr),
    chunk('IDAT', zlib.deflateSync(raw, { level: 9 })),
    chunk('IEND', Buffer.alloc(0)),
  ]);
};

// —— 主流程 ——
const src = decodePng(fs.readFileSync(SRC));
if (src.width !== src.height) throw new Error(`源图需要正方形,当前 ${src.width}x${src.height}`);
const bySize = new Map();
for (const s of [256, 128, 64, 48, 32, 16]) {
  bySize.set(s, resize(src, s, s));
  console.log(`缩放 ${s}x${s} 完成`);
}

// 派生 PNG(tauri bundle 引用)
fs.writeFileSync(path.join(__dirname, '128x128.png'), encodePng(bySize.get(128), 128));
fs.writeFileSync(path.join(__dirname, '32x32.png'), encodePng(bySize.get(32), 32));

// ICO:ICONDIR + ICONDIRENTRY × N + PNG payload(256 宽高字节写 0)
const sizes = [...bySize.keys()].sort((a, b) => b - a);
const payloads = sizes.map((s) => encodePng(bySize.get(s), s));
const headerSize = 6 + sizes.length * 16;
const icoParts = [Buffer.alloc(headerSize)];
icoParts[0].writeUInt16LE(0, 0); icoParts[0].writeUInt16LE(1, 2);
icoParts[0].writeUInt16LE(sizes.length, 4);
let offset = headerSize;
sizes.forEach((s, i) => {
  const e = i * 16 + 6;
  icoParts[0][e] = s >= 256 ? 0 : s;
  icoParts[0][e + 1] = s >= 256 ? 0 : s;
  icoParts[0].writeUInt16LE(1, e + 4); icoParts[0].writeUInt16LE(32, e + 6);
  icoParts[0].writeUInt32LE(payloads[i].length, e + 8);
  icoParts[0].writeUInt32LE(offset, e + 12);
  icoParts.push(payloads[i]);
  offset += payloads[i].length;
});
fs.writeFileSync(path.join(__dirname, 'icon.ico'), Buffer.concat(icoParts));
console.log('icons written:', fs.readdirSync(__dirname).filter((f) => f !== 'generate.js').join(', '));
