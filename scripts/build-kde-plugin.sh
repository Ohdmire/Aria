#!/usr/bin/env bash
# 构建并安装 KDE Plasma 壁纸插件(KPackage + 原生 QML 控件)。
#
# 需要:cmake、Qt6 Quick/Qml 开发包(Arch: qt6-base qt6-declarative;
# Debian/Ubuntu: qt6-base-dev qt6-declarative-dev)。
#
# 用法:
#   scripts/build-kde-plugin.sh              # 只构建,产物落进 kde/wallpaper
#   scripts/build-kde-plugin.sh --install    # 构建 + 安装到 ~/.local/share/plasma/wallpapers
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
build_dir="${root}/target/kde-plugin"

cmake -S "${root}/kde/plugin" -B "${build_dir}" -DCMAKE_BUILD_TYPE=Release
cmake --build "${build_dir}" -j"$(nproc)"

dest="${XDG_DATA_HOME:-${HOME}/.local/share}/plasma/wallpapers/com.ohdmire.aria"
if [[ "${1:-}" == "--install" ]]; then
    mkdir -p "${dest}"
    # 逐文件"临时文件 + rename":plasmashell 可能正映射着旧的
    # libariaplugin.so,原地覆盖会让它 SIGILL。rename 换 inode 才安全。
    (cd "${root}/kde/wallpaper" && find . -type f | while read -r f; do
        mkdir -p "${dest}/$(dirname "$f")"
        cp -f "$f" "${dest}/$f.tmp"
        mv -f "${dest}/$f.tmp" "${dest}/$f"
    done)
    echo "已安装到 ${dest}"
    echo "在桌面右键 → 配置桌面和壁纸 → 壁纸类型选 Aria 即可(Aria 启动时也会自动切换)。"
fi
