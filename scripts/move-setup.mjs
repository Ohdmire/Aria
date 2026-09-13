// ffmpeg 变体构建后,把产物 -setup.exe 改名为 -setup-ffmpeg.exe。
// 这样 -setup.exe 永远只属于"不带 ffmpeg"的变体,两份安装包可以
// 同时存在(build:setup 与 build:setup:ffmpeg 任意顺序、任意重复)。
import { readdirSync, renameSync } from 'node:fs';

const dir = 'target/release/bundle/nsis';
for (const f of readdirSync(dir).filter((x) => x.endsWith('-setup.exe'))) {
  renameSync(`${dir}/${f}`, `${dir}/${f.replace('-setup.exe', '-setup-ffmpeg.exe')}`);
  console.log('ffmpeg 变体:', f.replace('-setup.exe', '-setup-ffmpeg.exe'));
}
