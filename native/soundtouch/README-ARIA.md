# SoundTouch 2.3.2(内嵌副本)

上游:https://codeberg.org/soundtouch/soundtouch(LGPL-2.1,见 COPYING.TXT)。

本项目仅使用 `include/` + `source/SoundTouch/` 的核心源码:由
`build.rs` 经 cc 静态编译,`native/soundtouch_shim.cpp` 导出 C 接口
供 `src/soundtouch.rs` 调用 —— BGM 变速不变调(SoundTouch tempo
离线预处理)。上游其余内容(构建系统 / Android / DLL 示例)已剔除。
