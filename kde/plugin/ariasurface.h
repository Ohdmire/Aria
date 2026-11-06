// Aria 的 Plasma 壁纸控件:从共享内存取帧,或直接采样渲染进程导出的
// dmabuf(零拷贝)。
#pragma once

#include <QtCore/QTimer>
#include <QtCore/QVector>
#include <QtGui/QImage>
#include <QtQuick/QQuickItem>

class QSGTexture;

class AriaSurface : public QQuickItem
{
    Q_OBJECT

public:
    explicit AriaSurface(QQuickItem *parent = nullptr);
    ~AriaSurface() override;

protected:
    QSGNode *updatePaintNode(QSGNode *old, UpdatePaintNodeData *data) override;
    void componentComplete() override;

private:
    void tick();
    bool attach();
    void detach();
    void debugLog(const QString &line);

    /// dmabuf 通道:连 socket 收 fd/几何,失败下次再试。
    bool connectDmabuf();
    void dropDmabuf();
    /// 在 render thread 上把槽位 fd 导入成 GL 纹理 + QSGTexture。
    QSGTexture *ensureSlotTexture(int idx);

    struct DmabufSlot {
        int fd = -1;
        quint32 stride = 0;
        quint32 offset = 0;
        quint64 modifier = 0;
        unsigned int tex = 0;   // GLuint(EGLImage 导入的纹理)
        bool external = false;  // true: GL_TEXTURE_EXTERNAL_OES(NVIDIA 只能绑这个)
        QSGTexture *qsg = nullptr;
    };

    // NVIDIA 的 EGL dmabuf 导入只能绑到 GL_TEXTURE_EXTERNAL_OES,Qt 的
    // 场景图只认 sampler2D。所以每帧做一次 GPU blit 到普通 2D 纹理
    // (纯 GPU,~0.5ms@1440p),再交给 QSGSimpleTextureNode。
    bool ensureOutputTarget();
    bool blitToOutput(int idx);

    QTimer m_timer;
    int m_fd = -1;
    uchar *m_map = nullptr;
    qsizetype m_mapLen = 0;
    QImage m_frame;
    bool m_dirty = false;
    quint64 m_lastSeq = 0;
    qint64 m_lastAttachTry = 0;

    // dmabuf 零拷贝
    bool m_dmabuf = false;
    int m_dmabufSock = -1;
    qint64 m_lastDmabufTry = 0;
    quint32 m_dmabufW = 0;
    quint32 m_dmabufH = 0;
    quint32 m_fourcc = 0;
    int m_slot = 0;
    QVector<DmabufSlot> m_slots;
    unsigned int m_outTex = 0;      // GLuint:2D 输出纹理
    unsigned int m_outFbo = 0;      // GLuint
    unsigned int m_outProg = 0;     // GLuint
    unsigned int m_outVbo = 0;      // GLuint
    int m_attrPos = -1;
    int m_attrUv = -1;
    int m_uniformTex = -1;
    QSGTexture *m_outQsg = nullptr;
    QSize m_outSize;
};
