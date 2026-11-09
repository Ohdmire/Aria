#!/usr/bin/env bash
# 构建本地 Arch Linux/pacman 包。
# 用法: packaging/arch/build-package.sh [--keep-build]
# 从当前工作树生成源码归档，因此未提交的修复也会进入包。
set -Eeuo pipefail

here=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
root=$(cd -- "$here/../.." && pwd)
keep_build=false

usage() { printf '用法: %s [--keep-build]\n' "${BASH_SOURCE[0]}"; }
case "${1:-}" in
    '') ;;
    --keep-build) keep_build=true ;;
    -h|--help) usage; exit 0 ;;
    *) usage >&2; exit 2 ;;
esac

for command in cargo makepkg tar zstd; do
    command -v "$command" >/dev/null || {
        printf '错误: 找不到命令: %s\n' "$command" >&2
        exit 1
    }
done

version=$(awk '
    $1 == "version" && $2 == "=" {
        gsub(/"/, "", $3); print $3; exit
    }
' "$root/Cargo.toml")
[[ "$version" =~ ^[0-9]+\.[0-9]+\.[0-9]+([.-][0-9A-Za-z.-]+)?$ ]] || {
    printf '错误: Cargo.toml 中的版本号无效: %s\n' "$version" >&2
    exit 1
}

work=$(mktemp -d "${TMPDIR:-/tmp}/aria-pkg.XXXXXX")
trap 'rm -rf -- "$work"' EXIT
src_root="$work/aria-$version"
mkdir -p "$src_root"

printf '==> 从当前工作树生成源码归档 (aria-%s)\n' "$version"
# 排除本地构建产物、Git 数据库和旧的 pacman 产物，保留工作区修改。
tar \
    --exclude='./.git' \
    --exclude='./target' \
    --exclude='./packaging/arch/src' \
    --exclude='./packaging/arch/pkg' \
    --exclude='./packaging/arch/*.pkg.tar.*' \
    --exclude='./packaging/arch/*.src.tar.*' \
    -C "$root" -cf - . | tar -C "$src_root" -xf -

archive="$work/aria-$version-src.tar.zst"
tar -C "$work" -cf - "aria-$version" | zstd -19 -T0 -q -f -o "$archive"
cp -- "$archive" "$here/"

build_dir="$work/makepkg"
mkdir -p "$build_dir"
cp -- "$here/PKGBUILD.in" "$build_dir/PKGBUILD"
sed -i "s/@PKGVER@/$version/g" "$build_dir/PKGBUILD"
cp -- "$here/aria-$version-src.tar.zst" "$build_dir/"

printf '==> makepkg 构建 (版本 %s)\n' "$version"
(cd "$build_dir" && makepkg --force --cleanbuild --nocheck)

package=$(find "$build_dir" -maxdepth 1 -type f \
    -name "aria-$version-*.pkg.tar.*" -printf '%T@ %p\n' |
    sort -nr | awk 'NR == 1 { sub(/^[^ ]+ /, ""); print }')
[[ -n "$package" ]] || {
    printf '错误: makepkg 没有生成 aria 包\n' >&2
    exit 1
}

cp -- "$package" "$here/"
package="$here/$(basename -- "$package")"
printf '\n==> 包已生成: %s\n' "$package"
printf '    安装: sudo pacman -U %q\n' "$package"

if [[ "$keep_build" == true ]]; then
    kept="$here/build-$version"
    rm -rf -- "$kept"
    cp -a -- "$build_dir" "$kept"
    printf '    构建目录: %s\n' "$kept"
fi
