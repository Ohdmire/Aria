<p align="center">
  <img src="icons/icon.png" alt="Aria" width="240">
</p>

# Aria

**Aria** -- **A**TRI **R**hythm **I**nteractive **A**nimation ~ 咏叹调
osu! storyboard 动态桌面壁纸

## 快速开始

Release 下载安装 exe 即用
- 如电脑内未安装 `ffmpeg` 则第一次下载需要下载带 `-ffmpeg` 的安装包 
- 后续更新可以直接下载不带 `-ffmpeg` 的

## 核心特色

- **storyboard支持** —— 享受storyboard!
- **双端支持** —— osu!stable与 osu!lazer都可使用
- **随心选歌** —— 曲库可按收藏夹 / 星级 / 关键词 / 仅 storyboard·视频过滤
- **画面增强** —— FSR / Anime4K 超分让视频与背景更清晰

## 注意事项

- 设置与播放列表保存在 `%APPDATA%\com.ohdmire.aria\`

## 使用的开源库

- [osu-replay-render](https://github.com/Ohdmire/osu-replay-render) —— storyboard / 回放渲染核心,osu!stable 与 lazer 曲库解析
- [kira](https://github.com/tesselode/kira) / [Symphonia](https://github.com/pdeljanov/Symphonia) —— 音频引擎与解码
- [SoundTouch](https://codeberg.org/soundtouch/soundtouch)(LGPL-2.1) —— BGM 变速
- [Tauri](https://github.com/tauri-apps/tauri) —— 应用框架

osu! 是 Ppy Pty Ltd 的游戏;本项目与 ppy 无关 · MIT License

## 从源码构建

```bash
cargo build --release
```

## 赞助
感谢支持~
<p align="center">
  <a href="https://ifdian.net/a/ATRI1024">
    <img src="https://img.shields.io/badge/Aifadian-Support%20My%20Work-946CE6?style=for-the-badge" />
  </a>
</p>

## 特别感谢

- [xlfish233](https://osu.ppy.sh/users/34424018) —— Token Provider
- [Citrusis](https://osu.ppy.sh/users/30298378) —— Logo 设计
- pxyxy —— 5元赞助~
- [telecomadm1145](https://osu.ppy.sh/users/30656658) —— 5元赞助~
