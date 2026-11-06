#!/usr/bin/env bash
# 打一个本地 pacman 包(.pkg.tar.zst):
#   1. 用 git archive 把 Aria HEAD 打进源码包(osu-replay-render 走 git 依赖,
#      makepkg 构建时由 cargo 拉取);
#   2. 展开 PKGBUILD 模板,makepkg 构建;
#   3. 输出包路径和安装命令。
set -euo pipefail

here="$(cd "$(dirname "$0")" && pwd)"
root="$(cd "$here/../.." && pwd)"
ver="$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)"
[[ -n "$ver" ]] || { echo "读不到 Cargo.toml 版本号" >&2; exit 1; }

stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
git -C "$root" archive --prefix="aria-$ver/" HEAD | tar -x -C "$stage"
tar -C "$stage" -cf - "aria-$ver" | zstd -19 -T0 -q -f -o "$here/aria-$ver-src.tar.zst"

sed "s/@PKGVER@/$ver/g" "$here/PKGBUILD.in" > "$here/PKGBUILD"
echo "==> 源码包:$here/aria-$ver-src.tar.zst"
echo "==> makepkg(首次会全量编译 release,约 5-15 分钟)"

cd "$here"
makepkg -f --nocheck

pkg="$(ls -t "$here"/aria-"$ver"-*.pkg.tar.zst 2>/dev/null | head -1)"
[[ -n "$pkg" ]] && {
    echo
    echo "==> 包已生成:$pkg"
    echo "    安装:sudo pacman -U \"$pkg\""
}
