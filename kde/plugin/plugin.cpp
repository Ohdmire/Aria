// QML 插件入口:把 AriaSurface 注册成 `AriaWallpaper` 模块的类型。
#include <QtQml/QQmlExtensionPlugin>
#include <QtQml/qqml.h>

#include "ariasurface.h"

class AriaPlugin : public QQmlExtensionPlugin
{
    Q_OBJECT
    Q_PLUGIN_METADATA(IID QQmlExtensionInterface_iid)

public:
    void registerTypes(const char *uri) override
    {
        qmlRegisterType<AriaSurface>(uri, 1, 0, "AriaSurface");
    }
};

#include "plugin.moc"
