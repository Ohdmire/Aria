// Aria —— Plasma 壁纸插件入口。
//
// 壁纸是桌面 containment 里的一个 QML item,桌面图标由 plasmashell 画在
// 它上面 —— 所以图标永远可见。画面来自 Aria 渲染子进程写进的共享内存
// (见 src/kde.rs),由随包的原生控件 `aria/libariaplugin.so` 取帧显示。
import QtQuick
import org.kde.plasma.plasmoid
import "aria"

WallpaperItem {
    id: root

    AriaSurface {
        anchors.fill: parent
    }
}
