#include "ariasurface.h"

#include <QtCore/QDateTime>
#include <QtCore/QFile>
#include <QtCore/QTextStream>
#include <QtGui/QGuiApplication>
#include <QtQuick/QSGRendererInterface>
#include <QtQuick/QSGSimpleRectNode>
#include <QtQuick/QSGSimpleTextureNode>
#include <QtQuick/QSGTexture>
#include <QtQuick/QQuickWindow>
#include <QtQuick/qsgtexture_platform.h>

#include <cerrno>
#include <cstring>

#include <EGL/egl.h>
#include <EGL/eglext.h>
#include <GLES2/gl2.h>
#include <GLES2/gl2ext.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/un.h>
#include <unistd.h>

namespace {

// 与 src/kde.rs 保持一致
constexpr quint32 kMagic = 0x41495241; // "ARIA" little-endian
constexpr quint32 kVersion = 3;
constexpr quint32 kFormatBgra8 = 0;
constexpr quint32 kFormatDmabuf = 1;
constexpr qsizetype kHeaderReserve = 4096;
constexpr qsizetype kMaxW = 2560;
constexpr qsizetype kMaxH = 1440;
constexpr qsizetype kMaxStride = kMaxW * 4;
constexpr qsizetype kMaxFrame = kMaxStride * kMaxH;
// 三缓冲(与 src/kde.rs 的 SLOTS 一致)
constexpr qsizetype kSlots = 3;
constexpr qsizetype kShmLen = kHeaderReserve + kMaxFrame * kSlots;

struct AriaHeader {
    quint32 magic;
    quint32 version;
    quint32 width;
    quint32 height;
    quint32 stride;
    quint32 format;
    quint64 seq;
    quint64 bytes;
};
static_assert(sizeof(AriaHeader) == 40, "header layout must match Rust");

QString shmPath()
{
    return QStringLiteral("/dev/shm/aria-wallpaper-%1.shm").arg(::getuid());
}

QString dmabufSocketPath()
{
    const QByteArray dir = qgetenv("XDG_RUNTIME_DIR");
    if (!dir.isEmpty()) {
        return QString::fromLocal8Bit(dir) + QStringLiteral("/aria-wallpaper-dmabuf.sock");
    }
    return QStringLiteral("/tmp/aria-wallpaper-dmabuf.sock");
}

} // namespace

AriaSurface::AriaSurface(QQuickItem *parent)
    : QQuickItem(parent)
{
    setFlag(ItemHasContents, true);
    // 4ms 轮询:只是读一个序号原子变量,没有新帧就立即返回;高刷
    // (120/165)下也能取到每一帧,而 Qt 的渲染循环会按 vsync 合并 update。
    m_timer.setInterval(4);
    m_timer.setTimerType(Qt::PreciseTimer);
    connect(&m_timer, &QTimer::timeout, this, &AriaSurface::tick);
}

AriaSurface::~AriaSurface()
{
    dropDmabuf();
    detach();
}

void AriaSurface::componentComplete()
{
    QQuickItem::componentComplete();
    const bool ok = attach();
    debugLog(QStringLiteral("complete attach=%1 mapLen=%2").arg(ok).arg(m_mapLen));
    m_lastAttachTry = QDateTime::currentMSecsSinceEpoch();
    m_timer.start();
}

bool AriaSurface::attach()
{
    detach();
    const QByteArray path = shmPath().toLocal8Bit();
    const int fd = ::open(path.constData(), O_RDONLY | O_CLOEXEC);
    if (fd < 0) {
        return false;
    }
    struct stat st {};
    if (::fstat(fd, &st) != 0 || st.st_size < kHeaderReserve) {
        ::close(fd);
        return false;
    }
    const qsizetype len = qMin<qsizetype>(st.st_size, kShmLen);
    void *map = ::mmap(nullptr, len, PROT_READ, MAP_SHARED, fd, 0);
    if (map == MAP_FAILED) {
        ::close(fd);
        return false;
    }
    m_fd = fd;
    m_map = static_cast<uchar *>(map);
    m_mapLen = len;
    m_lastSeq = 0;
    return true;
}

void AriaSurface::detach()
{
    if (m_map) {
        ::munmap(m_map, m_mapLen);
        m_map = nullptr;
    }
    if (m_fd >= 0) {
        ::close(m_fd);
        m_fd = -1;
    }
    m_mapLen = 0;
}

bool AriaSurface::connectDmabuf()
{
    dropDmabuf();
    const QByteArray path = dmabufSocketPath().toLocal8Bit();
    const int sock = ::socket(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0);
    if (sock < 0) {
        return false;
    }
    struct timeval tv { 1, 0 };
    ::setsockopt(sock, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof tv);
    struct sockaddr_un addr {};
    addr.sun_family = AF_UNIX;
    std::strncpy(addr.sun_path, path.constData(), sizeof(addr.sun_path) - 1);
    if (::connect(sock, reinterpret_cast<struct sockaddr *>(&addr), sizeof addr) != 0) {
        debugLog(QStringLiteral("dmabuf: connect %1 失败 errno=%2").arg(QString::fromLocal8Bit(path)).arg(errno));
        ::close(sock);
        return false;
    }

    char buf[128] = {};
    char cmsgbuf[CMSG_SPACE(sizeof(int) * 8)] = {};
    struct iovec iov { buf, sizeof buf };
    struct msghdr msg {};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = cmsgbuf;
    msg.msg_controllen = sizeof cmsgbuf;
    const ssize_t n = ::recvmsg(sock, &msg, 0);
    ::close(sock);
    if (n < 44) {
        debugLog(QStringLiteral("dmabuf: recvmsg n=%1").arg(qint64(n)));
        return false;
    }
    quint32 v[8] = {};
    quint64 modifier = 0;
    std::memcpy(v, buf, 32);
    std::memcpy(&modifier, buf + 32, 8);
    if (v[0] != kMagic || v[1] != 1) {
        debugLog(QStringLiteral("dmabuf: handshake magic=%1 ver=%2").arg(v[0]).arg(v[1]));
        return false;
    }
    const quint32 width = v[2];
    const quint32 height = v[3];
    const quint32 nslots = v[4]; // 注意:slots 是 Qt 关键字宏
    const quint32 fourcc = v[5];
    const quint32 stride = v[6];
    const quint32 offset = v[7];
    if (width == 0 || height == 0 || nslots == 0 || nslots > 8) {
        return false;
    }

    QVector<int> fds;
    for (struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg); cmsg; cmsg = CMSG_NXTHDR(&msg, cmsg)) {
        if (cmsg->cmsg_level == SOL_SOCKET && cmsg->cmsg_type == SCM_RIGHTS) {
            const int count = int((cmsg->cmsg_len - CMSG_LEN(0)) / sizeof(int));
            const int *data = reinterpret_cast<const int *>(CMSG_DATA(cmsg));
            for (int i = 0; i < count; ++i) {
                fds.append(data[i]);
            }
        }
    }
    if (fds.size() < int(nslots)) {
        debugLog(QStringLiteral("dmabuf: fd 数量 %1 < %2").arg(fds.size()).arg(nslots));
        for (int fd : fds) {
            ::close(fd);
        }
        return false;
    }
    m_slots.clear();
    m_slots.resize(int(nslots));
    for (int i = 0; i < int(nslots); ++i) {
        m_slots[i].fd = fds[i];
        m_slots[i].stride = stride;
        m_slots[i].offset = offset;
        m_slots[i].modifier = modifier;
    }
    m_dmabufW = width;
    m_dmabufH = height;
    m_fourcc = fourcc;
    m_slot = 0;
    m_lastSeq = 0;
    m_dmabuf = true;
    debugLog(QStringLiteral("dmabuf handshake %1x%2 slots=%3").arg(width).arg(height).arg(nslots));
    return true;
}

void AriaSurface::dropDmabuf()
{
    for (auto &s : m_slots) {
        if (s.fd >= 0) {
            ::close(s.fd);
            s.fd = -1;
        }
        // QSGTexture/GL 纹理留给 Qt 回收(必须在本 item 的渲染线程上删,
        // 这里不动它们;重连很少发生,最坏情况是少量泄漏)。
        s.qsg = nullptr;
        s.tex = 0;
    }
    m_slots.clear();
    m_dmabuf = false;
}

QSGTexture *AriaSurface::ensureSlotTexture(int idx)
{
    if (idx < 0 || idx >= m_slots.size() || !window()) {
        return nullptr;
    }
    DmabufSlot &s = m_slots[idx];
    if (!s.tex) {
        if (window()->rendererInterface()->graphicsApi() != QSGRendererInterface::OpenGL) {
            debugLog(QStringLiteral("dmabuf: 当前不是 OpenGL RHI,无法导入"));
            return nullptr;
        }
        static PFNEGLCREATEIMAGEKHRPROC createImage = nullptr;
        static PFNGLEGLIMAGETARGETTEXTURE2DOESPROC targetTexture = nullptr;
        if (!createImage) {
            createImage = reinterpret_cast<PFNEGLCREATEIMAGEKHRPROC>(eglGetProcAddress("eglCreateImageKHR"));
            targetTexture = reinterpret_cast<PFNGLEGLIMAGETARGETTEXTURE2DOESPROC>(
                eglGetProcAddress("glEGLImageTargetTexture2DOES"));
        }
        EGLDisplay dpy = eglGetCurrentDisplay();
        if (!createImage || !targetTexture || dpy == EGL_NO_DISPLAY) {
            debugLog(QStringLiteral("dmabuf: EGL 入口缺失"));
            return nullptr;
        }
        const EGLint attrs[] = {
            EGL_WIDTH, EGLint(m_dmabufW),
            EGL_HEIGHT, EGLint(m_dmabufH),
            EGL_LINUX_DRM_FOURCC_EXT, EGLint(m_fourcc),
            EGL_DMA_BUF_PLANE0_FD_EXT, s.fd,
            EGL_DMA_BUF_PLANE0_OFFSET_EXT, EGLint(s.offset),
            EGL_DMA_BUF_PLANE0_PITCH_EXT, EGLint(s.stride),
            EGL_NONE,
        };
        EGLImageKHR image = createImage(dpy, EGL_NO_CONTEXT, EGL_LINUX_DMA_BUF_EXT, nullptr, attrs);
        if (image == EGL_NO_IMAGE_KHR) {
            debugLog(QStringLiteral("dmabuf: eglCreateImage 失败 0x%1").arg(int(eglGetError()), 0, 16));
            return nullptr;
        }
        GLuint tex = 0;
        glGenTextures(1, &tex);
        // NVIDIA:dmabuf 只能绑 GL_TEXTURE_EXTERNAL_OES;Mesa 等 2D 也行。
        glGetError();
        glBindTexture(GL_TEXTURE_EXTERNAL_OES, tex);
        targetTexture(GL_TEXTURE_EXTERNAL_OES, image);
        if (glGetError() == GL_NO_ERROR) {
            s.external = true;
            glBindTexture(GL_TEXTURE_EXTERNAL_OES, 0);
        } else {
            glDeleteTextures(1, &tex);
            glGenTextures(1, &tex);
            glBindTexture(GL_TEXTURE_2D, tex);
            targetTexture(GL_TEXTURE_2D, image);
            const GLenum err = glGetError();
            glBindTexture(GL_TEXTURE_2D, 0);
            if (err != GL_NO_ERROR) {
                debugLog(QStringLiteral("dmabuf: 纹理导入失败 0x%1").arg(int(err), 0, 16));
                glDeleteTextures(1, &tex);
                return nullptr;
            }
        }
        s.tex = tex;
        debugLog(QStringLiteral("dmabuf: slot=%1 tex=%2 external=%3").arg(idx).arg(tex).arg(s.external));
    }

    if (s.external) {
        return blitToOutput(idx) ? m_outQsg : nullptr;
    }
    if (!s.qsg) {
        s.qsg = QNativeInterface::QSGOpenGLTexture::fromNative(
            s.tex, window(), QSize(int(m_dmabufW), int(m_dmabufH)), QQuickWindow::TextureIsOpaque);
    }
    return s.qsg;
}

bool AriaSurface::ensureOutputTarget()
{
    const QSize size{int(m_dmabufW), int(m_dmabufH)};
    if (m_outTex && m_outSize == size) {
        return true;
    }
    if (!window()) {
        return false;
    }
    static const char *vs =
        "attribute vec2 aPos; attribute vec2 aUv; varying vec2 vUv;\n"
        "void main(){ gl_Position = vec4(aPos, 0.0, 1.0); vUv = aUv; }\n";
    static const char *fs =
        "#extension GL_OES_EGL_image_external : require\n"
        "precision mediump float; varying vec2 vUv; uniform samplerExternalOES uTex;\n"
        "void main(){ gl_FragColor = texture2D(uTex, vUv); }\n";
    auto compile = [](GLenum type, const char *src) -> GLuint {
        GLuint sh = glCreateShader(type);
        glShaderSource(sh, 1, &src, nullptr);
        glCompileShader(sh);
        GLint ok = 0;
        glGetShaderiv(sh, GL_COMPILE_STATUS, &ok);
        return ok ? sh : 0;
    };
    const GLuint v = compile(GL_VERTEX_SHADER, vs);
    const GLuint f = compile(GL_FRAGMENT_SHADER, fs);
    if (!v || !f) {
        debugLog(QStringLiteral("dmabuf: 着色器编译失败"));
        return false;
    }
    m_outProg = glCreateProgram();
    glAttachShader(m_outProg, v);
    glAttachShader(m_outProg, f);
    glLinkProgram(m_outProg);
    GLint linked = 0;
    glGetProgramiv(m_outProg, GL_LINK_STATUS, &linked);
    if (!linked) {
        debugLog(QStringLiteral("dmabuf: 着色器链接失败"));
        return false;
    }
    m_attrPos = glGetAttribLocation(m_outProg, "aPos");
    m_attrUv = glGetAttribLocation(m_outProg, "aUv");
    m_uniformTex = glGetUniformLocation(m_outProg, "uTex");

    glGenTextures(1, &m_outTex);
    glBindTexture(GL_TEXTURE_2D, m_outTex);
    glTexImage2D(GL_TEXTURE_2D, 0, GL_RGBA, size.width(), size.height(), 0, GL_RGBA,
                 GL_UNSIGNED_BYTE, nullptr);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MIN_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_MAG_FILTER, GL_LINEAR);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_S, GL_CLAMP_TO_EDGE);
    glTexParameteri(GL_TEXTURE_2D, GL_TEXTURE_WRAP_T, GL_CLAMP_TO_EDGE);
    glBindTexture(GL_TEXTURE_2D, 0);

    glGenFramebuffers(1, &m_outFbo);
    glBindFramebuffer(GL_FRAMEBUFFER, m_outFbo);
    glFramebufferTexture2D(GL_FRAMEBUFFER, GL_COLOR_ATTACHMENT0, GL_TEXTURE_2D, m_outTex, 0);
    const GLenum status = glCheckFramebufferStatus(GL_FRAMEBUFFER);
    glBindFramebuffer(GL_FRAMEBUFFER, 0);
    if (status != GL_FRAMEBUFFER_COMPLETE) {
        debugLog(QStringLiteral("dmabuf: FBO 不完整 0x%1").arg(int(status), 0, 16));
        return false;
    }

    // dmabuf 第 0 行是画面顶部,GL 纹理 t=0 对应第 0 行;Qt 场景图把 item
    // 顶部映射到 t=0,所以 FBO 底边(y=-1)要采 t=0,顶边采 t=1,画面才正。
    static const float verts[] = {
        -1.f, -1.f, 0.f, 0.f,  1.f, -1.f, 1.f, 0.f,  1.f, 1.f, 1.f, 1.f,
        -1.f, -1.f, 0.f, 0.f,  1.f, 1.f, 1.f, 1.f,  -1.f, 1.f, 0.f, 1.f,
    };
    glGenBuffers(1, &m_outVbo);
    glBindBuffer(GL_ARRAY_BUFFER, m_outVbo);
    glBufferData(GL_ARRAY_BUFFER, sizeof verts, verts, GL_STATIC_DRAW);
    glBindBuffer(GL_ARRAY_BUFFER, 0);

    m_outSize = size;
    m_outQsg = QNativeInterface::QSGOpenGLTexture::fromNative(
        m_outTex, window(), size, QQuickWindow::TextureIsOpaque);
    debugLog(QStringLiteral("dmabuf: 输出纹理 %1x%2 prog=%3").arg(size.width()).arg(size.height()).arg(m_outProg));
    return m_outQsg != nullptr;
}

bool AriaSurface::blitToOutput(int idx)
{
    if (!ensureOutputTarget()) {
        return false;
    }
    GLint prevFbo = 0, prevProg = 0, prevTex = 0, prevActive = 0, prevBuf = 0, prevVp[4] = {};
    GLboolean prevBlend = glIsEnabled(GL_BLEND);
    GLboolean prevDepth = glIsEnabled(GL_DEPTH_TEST);
    GLboolean prevScissor = glIsEnabled(GL_SCISSOR_TEST);
    GLboolean prevCull = glIsEnabled(GL_CULL_FACE);
    GLint prevExtTex = 0;
    glGetIntegerv(GL_FRAMEBUFFER_BINDING, &prevFbo);
    glGetIntegerv(GL_CURRENT_PROGRAM, &prevProg);
    glGetIntegerv(GL_TEXTURE_BINDING_2D, &prevTex);
    glGetIntegerv(GL_ACTIVE_TEXTURE, &prevActive);
    glGetIntegerv(GL_ARRAY_BUFFER_BINDING, &prevBuf);
    glGetIntegerv(GL_VIEWPORT, prevVp);
    glGetIntegerv(GL_TEXTURE_BINDING_EXTERNAL_OES, &prevExtTex);

    glBindFramebuffer(GL_FRAMEBUFFER, m_outFbo);
    glViewport(0, 0, m_outSize.width(), m_outSize.height());
    glUseProgram(m_outProg);
    glActiveTexture(GL_TEXTURE0);
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, m_slots[idx].tex);
    glUniform1i(m_uniformTex, 0);
    glBindBuffer(GL_ARRAY_BUFFER, m_outVbo);
    glEnableVertexAttribArray(GLuint(m_attrPos));
    glVertexAttribPointer(GLuint(m_attrPos), 2, GL_FLOAT, GL_FALSE, 16, static_cast<void *>(nullptr));
    glEnableVertexAttribArray(GLuint(m_attrUv));
    glVertexAttribPointer(GLuint(m_attrUv), 2, GL_FLOAT, GL_FALSE, 16,
                          reinterpret_cast<void *>(8));
    glDisable(GL_BLEND);
    glDisable(GL_DEPTH_TEST);
    glDisable(GL_SCISSOR_TEST);
    glDisable(GL_CULL_FACE);
    glDrawArrays(GL_TRIANGLES, 0, 6);
    glDisableVertexAttribArray(GLuint(m_attrPos));
    glDisableVertexAttribArray(GLuint(m_attrUv));

    glBindFramebuffer(GL_FRAMEBUFFER, GLuint(prevFbo));
    glViewport(prevVp[0], prevVp[1], prevVp[2], prevVp[3]);
    glUseProgram(GLuint(prevProg));
    glBindBuffer(GL_ARRAY_BUFFER, GLuint(prevBuf));
    glActiveTexture(GLuint(prevActive));
    glBindTexture(GL_TEXTURE_EXTERNAL_OES, GLuint(prevExtTex));
    glBindTexture(GL_TEXTURE_2D, GLuint(prevTex));
    if (prevBlend) glEnable(GL_BLEND); else glDisable(GL_BLEND);
    if (prevDepth) glEnable(GL_DEPTH_TEST); else glDisable(GL_DEPTH_TEST);
    if (prevScissor) glEnable(GL_SCISSOR_TEST); else glDisable(GL_SCISSOR_TEST);
    if (prevCull) glEnable(GL_CULL_FACE); else glDisable(GL_CULL_FACE);
    return true;
}

void AriaSurface::tick()
{
    if (!m_map) {
        // 渲染进程可能后启动:每秒重试一次
        const qint64 now = QDateTime::currentMSecsSinceEpoch();
        if (now - m_lastAttachTry < 1000) {
            return;
        }
        m_lastAttachTry = now;
        const bool ok = attach();
        debugLog(QStringLiteral("retry attach=%1 mapLen=%2").arg(ok).arg(m_mapLen));
        if (!ok) {
            return;
        }
    }
    auto *h = reinterpret_cast<AriaHeader *>(m_map);
    if (h->magic != kMagic || h->version != kVersion) {
        debugLog(QStringLiteral("bad header magic=%1 version=%2").arg(h->magic).arg(h->version));
        return;
    }

    if (h->format == kFormatDmabuf) {
        if (!m_dmabuf) {
            const qint64 now = QDateTime::currentMSecsSinceEpoch();
            if (now - m_lastDmabufTry < 1000) {
                return;
            }
            m_lastDmabufTry = now;
            connectDmabuf();
            return;
        }
        if (h->width != m_dmabufW || h->height != m_dmabufH) {
            dropDmabuf();
            return;
        }
        const quint64 seq = __atomic_load_n(&h->seq, __ATOMIC_ACQUIRE);
        if (seq == 0 || seq == m_lastSeq) {
            return;
        }
        if (seq < m_lastSeq) {
            // 渲染进程重启(序号归零):重新握手拿新 fd
            dropDmabuf();
            return;
        }
        m_lastSeq = seq;
        m_slot = int((seq - 1) % quint64(m_slots.size()));
        m_dirty = true;
        update();
        debugLog(QStringLiteral("dmabuf seq=%1 slot=%2").arg(seq).arg(m_slot));
        return;
    }

    if (m_dmabuf) {
        dropDmabuf();
    }
    const quint32 w = h->width;
    const quint32 hgt = h->height;
    const quint32 stride = h->stride;
    const quint64 bytes = h->bytes;
    if (w == 0 || hgt == 0 || w > kMaxW || hgt > kMaxH || stride < w * 4
        || stride > kMaxStride || bytes < quint64(stride) * hgt) {
        debugLog(QStringLiteral("bad frame %1x%2 stride=%3 bytes=%4").arg(w).arg(hgt).arg(stride).arg(bytes));
        return;
    }
    if (qsizetype(bytes) > m_mapLen - kHeaderReserve) {
        debugLog(QStringLiteral("frame too big bytes=%1 mapLen=%2").arg(bytes).arg(m_mapLen));
        return;
    }
    const quint64 seq = __atomic_load_n(&h->seq, __ATOMIC_ACQUIRE);
    if (seq == 0 || seq == m_lastSeq) {
        return;
    }
    const qsizetype idx = qsizetype((seq - 1) % kSlots);
    const uchar *src = m_map + kHeaderReserve + idx * kMaxFrame;

    // BGRA 字节序 = 小端 ARGB32 的内存布局;取 RGB32(忽略恒为 255 的
    // alpha)让 Qt 走不透明上传路径,少一次 alpha 处理与格式转换。
    if (m_frame.width() != int(w) || m_frame.height() != int(hgt)
        || m_frame.format() != QImage::Format_RGB32) {
        m_frame = QImage(int(w), int(hgt), QImage::Format_RGB32);
    }
    const qsizetype dstStride = m_frame.bytesPerLine();
    if (dstStride == qsizetype(stride)) {
        std::memcpy(m_frame.bits(), src, qsizetype(stride) * hgt);
    } else {
        for (quint32 y = 0; y < hgt; ++y) {
            std::memcpy(m_frame.scanLine(int(y)), src + qsizetype(y) * stride, qsizetype(w) * 4);
        }
    }
    // 拷贝期间写者若又推进两帧,这一帧可能被覆盖:丢弃,下一 tick 再取。
    if (__atomic_load_n(&h->seq, __ATOMIC_ACQUIRE) != seq) {
        return;
    }
    m_lastSeq = seq;
    m_dirty = true;
    update();
    debugLog(QStringLiteral("frame seq=%1 %2x%3").arg(seq).arg(w).arg(hgt));
}

void AriaSurface::debugLog(const QString &line)
{
    // 现场排障开关:存在 /tmp/aria-kde-debug 时才写(正常使用零 IO)。
    static const bool enabled = QFile::exists(QStringLiteral("/tmp/aria-kde-debug"));
    if (!enabled) {
        return;
    }
    QFile f(QStringLiteral("/tmp/aria-kde-frames.log"));
    if (f.open(QIODevice::Append | QIODevice::WriteOnly)) {
        QTextStream(&f) << QDateTime::currentDateTime().toString(Qt::ISODateWithMs) << ' '
                        << line << '\n';
    }
}

QSGNode *AriaSurface::updatePaintNode(QSGNode *old, UpdatePaintNodeData *)
{
    if (!m_dmabuf && m_frame.isNull()) {
        delete old;
        return nullptr;
    }

    QSGNode *root = old;
    QSGSimpleRectNode *bg = nullptr;
    QSGSimpleTextureNode *tex = nullptr;
    if (!root) {
        root = new QSGNode;
        bg = new QSGSimpleRectNode;
        tex = new QSGSimpleTextureNode;
        bg->setColor(Qt::black);
        root->appendChildNode(bg);
        root->appendChildNode(tex);
    } else {
        bg = static_cast<QSGSimpleRectNode *>(root->firstChild());
        tex = static_cast<QSGSimpleTextureNode *>(bg->nextSibling());
    }

    const QRectF itemRect = boundingRect();
    bg->setRect(itemRect);

    QSize frameSize;
    if (m_dmabuf) {
        QSGTexture *slotTex = ensureSlotTexture(m_slot);
        if (!slotTex) {
            tex->setRect(QRectF());
            return root;
        }
        tex->setOwnsTexture(false);
        tex->setTexture(slotTex);
        frameSize = QSize(int(m_dmabufW), int(m_dmabufH));
    } else if (window()) {
        // 节点接管纹理所有权:换帧时自动删除旧纹理。
        tex->setOwnsTexture(true);
        tex->setTexture(window()->createTextureFromImage(m_frame, QQuickWindow::TextureIsOpaque));
        m_dirty = false;
        frameSize = m_frame.size();
    }
    if (!frameSize.isValid()) {
        return root;
    }

    const qreal iw = frameSize.width();
    const qreal ih = frameSize.height();
    const qreal scale = qMin(itemRect.width() / iw, itemRect.height() / ih);
    const QSizeF size(iw * scale, ih * scale);
    tex->setRect(QRectF(itemRect.x() + (itemRect.width() - size.width()) / 2.0,
                        itemRect.y() + (itemRect.height() - size.height()) / 2.0,
                        size.width(),
                        size.height()));
    tex->setFiltering(QSGTexture::Linear);
    debugLog(QStringLiteral("paint %1x%2 dmabuf=%3").arg(int(iw)).arg(int(ih)).arg(m_dmabuf));
    return root;
}
