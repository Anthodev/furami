#include "host.h"
#include "furami/src/ui/bridge.cxx.h"

#include <QCoreApplication>
#include <QDebug>
#include <QGuiApplication>
#include <QInputMethodEvent>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QPlatformSurfaceEvent>
#include <QQmlApplicationEngine>
#include <QQmlEngine>
#include <QThread>
#include <QVariant>
#include <QtGui/qguiapplication_platform.h>

#include <X11/Xlib.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>

#include <clocale>
#include <cstdlib>
#include <limits>
#include <utility>

// Xlib event constants otherwise collide with scoped Qt event names.
#undef KeyPress
#undef KeyRelease
#undef FocusIn
#undef FocusOut
#undef Expose
#undef None
#undef Status
#undef Bool

namespace furami::bridge {
namespace {

QString fromRust(const rust::String &value)
{
    return QString::fromUtf8(value.data(), static_cast<qsizetype>(value.size()));
}

rust::Str asRust(const QByteArray &value)
{
    return rust::Str(value.constData(), static_cast<std::size_t>(value.size()));
}

QString phaseName(GatePhase phase)
{
    switch (phase) {
    case GatePhase::Idle: return QStringLiteral("Idle");
    case GatePhase::WaitingSurface: return QStringLiteral("WaitingSurface");
    case GatePhase::Opening: return QStringLiteral("Opening");
    case GatePhase::Ready: return QStringLiteral("Ready");
    case GatePhase::Stopping: return QStringLiteral("Stopping");
    case GatePhase::Releasing: return QStringLiteral("Releasing");
    case GatePhase::Failed: return QStringLiteral("Failed");
    case GatePhase::QuitReady: return QStringLiteral("QuitReady");
    }
    return QStringLiteral("Unknown");
}

QString statusName(SubmitStatus status)
{
    switch (status) {
    case SubmitStatus::Accepted: return QStringLiteral("Accepted");
    case SubmitStatus::StaleGeneration: return QStringLiteral("StaleGeneration");
    case SubmitStatus::NotReady: return QStringLiteral("NotReady");
    case SubmitStatus::Closing: return QStringLiteral("Closing");
    case SubmitStatus::CapacityExceeded: return QStringLiteral("CapacityExceeded");
    }
    return QStringLiteral("Unknown");
}

QString boolean(bool value)
{
    return value ? QStringLiteral("true") : QStringLiteral("false");
}

LaunchResult launchFailure(const QString &diagnostic)
{
    qCritical().noquote() << "native_startup_failure" << diagnostic;
    const auto bytes = diagnostic.toUtf8();
    return LaunchResult{1, rust::String(bytes.constData(), static_cast<std::size_t>(bytes.size()))};
}

} // namespace

class HostWindow final : public QWindow {
public:
    HostWindow(FuramiBridge *bridge, std::uint64_t generation)
        : m_bridge(bridge), m_generation(generation)
    {
        setObjectName(QStringLiteral("videoHost"));
        setTitle(QStringLiteral("Furami native video host"));
        setCursor(Qt::ArrowCursor);
    }

    void authorizeDestruction() { m_authorized = true; }
    void poison() { m_poisoned = true; }
    bool poisoned() const { return m_poisoned; }
    void rememberXid(std::uint64_t xid) { m_previousXid = xid; }

protected:
    bool event(QEvent *event) override
    {
        Q_ASSERT(QThread::currentThread() == thread());
        if (event->type() == QEvent::FocusIn || event->type() == QEvent::FocusOut) {
            qInfo().noquote() << QStringLiteral("input_focus_window source=host event=%1 generation=%2")
                .arg(event->type() == QEvent::FocusIn ? QStringLiteral("FocusIn") : QStringLiteral("FocusOut"))
                .arg(m_generation);
        }
        if (event->type() == QEvent::PlatformSurface) {
            auto *surface = static_cast<QPlatformSurfaceEvent *>(event);
            if (surface->surfaceEventType() == QPlatformSurfaceEvent::SurfaceCreated) {
                qInfo().noquote() << QStringLiteral("native_surface_created generation=%1 poisoned=%2")
                    .arg(m_generation).arg(boolean(m_poisoned));
                if (!m_poisoned && !m_authorized && m_bridge)
                    m_bridge->schedulePublication(m_generation);
                else
                    qWarning().noquote() << QStringLiteral("native_surface_rejected generation=%1 reason=poisoned_or_releasing")
                        .arg(m_generation);
            } else {
                qInfo().noquote() << QStringLiteral("native_surface_about_to_destroy generation=%1 host=%2 authorized=%3")
                    .arg(m_generation).arg(m_previousXid).arg(boolean(m_authorized));
                if (!m_authorized && !m_poisoned) {
                    // No winId, reparent, wait, or nested event loop is allowed here.
                    m_poisoned = true;
                    if (m_bridge)
                        m_bridge->surfaceLost(m_generation, m_previousXid);
                }
            }
        }
        if (event->type() == QEvent::InputMethod && m_bridge && !m_poisoned) {
            m_bridge->forwardToRoot(event, "host_input_method");
            return true;
        }
        return QWindow::event(event);
    }

    void keyPressEvent(QKeyEvent *event) override
    {
        if (m_poisoned || !m_bridge) {
            event->ignore();
            return;
        }
        if (!m_bridge->handleKey(event, "host"))
            m_bridge->forwardToRoot(event, "host_key");
    }

    void keyReleaseEvent(QKeyEvent *event) override
    {
        if (!m_poisoned && m_bridge)
            m_bridge->forwardToRoot(event, "host_key_release");
        else
            event->ignore();
    }

    void mousePressEvent(QMouseEvent *event) override
    {
        qInfo().noquote() << QStringLiteral("input_event source=host kind=mouse_press generation=%1 button=%2 x=%3 y=%4")
            .arg(m_generation).arg(static_cast<int>(event->button()))
            .arg(event->position().x()).arg(event->position().y());
        if (!m_poisoned && m_bridge && event->button() == Qt::LeftButton) {
            m_bridge->focusVideo();
            event->accept();
        } else {
            event->ignore();
        }
    }

private:
    QPointer<FuramiBridge> m_bridge;
    std::uint64_t m_generation;
    std::uint64_t m_previousXid = 0;
    bool m_authorized = false;
    bool m_poisoned = false;
};

FuramiBridge::FuramiBridge(rust::Str mediaPrefix)
    : m_gate(new_gate(mediaPrefix))
{
    assertGuiThread();
    QQmlEngine::setObjectOwnership(this, QQmlEngine::CppOwnership);
    m_pollTimer.setInterval(16);
    connect(&m_pollTimer, &QTimer::timeout, this, [this] {
        assertGuiThread();
        applyUpdate(gate_poll(*m_gate));
    });
    m_pollTimer.start();
    qGuiApp->installEventFilter(this);
}

FuramiBridge::~FuramiBridge()
{
    assertGuiThread();
    m_pollTimer.stop();
    Q_ASSERT(m_nativeGeneration == 0);
}

void FuramiBridge::assertGuiThread() const
{
    Q_ASSERT(qGuiApp && QThread::currentThread() == qGuiApp->thread());
    Q_ASSERT(QThread::currentThread() == thread());
}

QWindow *FuramiBridge::hostWindow() const { assertGuiThread(); return m_host.data(); }
QString FuramiBridge::phase() const { assertGuiThread(); return m_failed ? QStringLiteral("Failed") : m_actualPhase; }
QString FuramiBridge::diagnostic() const { assertGuiThread(); return m_diagnostic; }
bool FuramiBridge::paused() const { assertGuiThread(); return m_paused; }
bool FuramiBridge::ended() const { assertGuiThread(); return m_ended; }
bool FuramiBridge::textEntryActive() const { assertGuiThread(); return m_textEntryActive; }
bool FuramiBridge::panelVisible() const { assertGuiThread(); return m_panelVisible; }
bool FuramiBridge::popupOpen() const { assertGuiThread(); return m_popupOpen; }
bool FuramiBridge::quitAuthorized() const { assertGuiThread(); return m_quitAuthorized; }
bool FuramiBridge::failed() const { assertGuiThread(); return m_failed; }

void FuramiBridge::setTextEntryActive(bool active)
{
    assertGuiThread();
    if (m_textEntryActive == active)
        return;
    m_textEntryActive = active;
    qInfo().noquote() << QStringLiteral("input_text_focus active=%1 generation=%2")
        .arg(boolean(active)).arg(m_generation);
    emit textEntryActiveChanged();
}

void FuramiBridge::setPanelVisible(bool visible)
{
    assertGuiThread();
    if (m_panelVisible == visible || (!visible && m_popupOpen))
        return;
    m_panelVisible = visible;
    qInfo().noquote() << QStringLiteral("qt_panel visible=%1 root=%2 generation=%3")
        .arg(boolean(visible)).arg(m_rootXid).arg(m_generation);
    emit panelVisibleChanged();
    if (!visible)
        focusVideo();
    scheduleGeometryLog();
}

void FuramiBridge::setPopupOpen(bool open)
{
    assertGuiThread();
    if (m_popupOpen == open)
        return;
    m_popupOpen = open;
    qInfo().noquote() << QStringLiteral("qt_panel_popup open=%1 policy=panel_only_no_scrim root=%2")
        .arg(boolean(open)).arg(m_rootXid);
    emit popupOpenChanged();
    scheduleGeometryLog();
}

bool FuramiBridge::bindRoot(QQuickWindow *root, QQuickItem *container, QString &diagnostic)
{
    assertGuiThread();
    if (!root || !container || container->window() != root) {
        diagnostic = QStringLiteral("QML root or permanent videoContainer association is missing");
        return false;
    }
    m_root = root;
    m_container = container;
    m_rootXid = static_cast<std::uint64_t>(root->winId());
    if (!m_rootXid || !root->handle()) {
        diagnostic = QStringLiteral("QML root has no valid native X11 surface");
        return false;
    }
    root->setCursor(Qt::ArrowCursor);
    qInfo().noquote() << QStringLiteral("native_root root=%1 container=videoContainer generation=0")
        .arg(m_rootXid);
    connect(root, &QWindow::widthChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QWindow::heightChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QWindow::screenChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QQuickWindow::activeFocusItemChanged, this, [this] {
        if (!m_root)
            return;
        const auto *item = m_root->activeFocusItem();
        const auto *window = QGuiApplication::focusWindow();
        qInfo().noquote() << QStringLiteral("input_focus_observed root=%1 root_active=%2 item=%3 native_window=%4 generation=%5")
            .arg(m_rootXid).arg(boolean(m_root->isActive()))
            .arg(item ? item->objectName() : QStringLiteral("none"))
            .arg(window ? window->objectName() : QStringLiteral("none")).arg(m_generation);
    });
    connect(container, &QQuickItem::widthChanged, this, [this] { scheduleGeometryLog(); });
    connect(container, &QQuickItem::heightChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QObject::destroyed, this, [this] {
        assertGuiThread();
        m_root.clear();
        m_container.clear();
        qInfo().noquote() << QStringLiteral("native_root_destroyed root=%1 generation=%2 authorized=%3")
            .arg(m_rootXid).arg(m_generation).arg(boolean(m_quitAuthorized));
        if (!m_quitAuthorized) {
            // Root loss is a failed bootstrap even when no media owner exists.
            // Active owners still follow their real stop acknowledgement protocol.
            m_bootstrapFailed = true;
            m_failed = true;
            m_nativeDiagnostic = QStringLiteral("application root QObject was destroyed without authorized application close");
            m_diagnostic = m_nativeDiagnostic;
            emit stateChanged();
            qCritical().noquote() << QStringLiteral("native_root_destroyed_uncontrolled root=%1 generation=%2")
                .arg(m_rootXid).arg(m_generation);
            if (m_nativeGeneration)
                failNative(m_nativeGeneration, QStringLiteral("application root was destroyed before owner acknowledgement"));
            applyUpdate(gate_quit(*m_gate));
        }
    });
    scheduleGeometryLog();
    return true;
}

void FuramiBridge::openProofSource()
{
    assertGuiThread();
    if (m_actualPhase != QStringLiteral("Idle") || m_failed)
        return;
    qInfo().noquote() << "input_intent action=OpenProofSource source=panel";
    applyUpdate(gate_open(*m_gate));
}

void FuramiBridge::closeProofSession()
{
    assertGuiThread();
    qInfo().noquote() << QStringLiteral("input_intent action=CloseSession source=panel generation=%1 phase=%2")
        .arg(m_generation).arg(m_actualPhase);
    applyUpdate(gate_close(*m_gate, m_generation, false));
}

void FuramiBridge::openAndCloseDuringOpeningForProof()
{
    assertGuiThread();
    if (m_actualPhase != QStringLiteral("Idle") || m_failed)
        return;
    qInfo().noquote() << "input_intent action=OpenAndCloseDuringOpening source=panel";
    auto update = gate_open(*m_gate);
    if (update.create_native)
        m_closeDuringOpening = update.generation;
    applyUpdate(std::move(update));
}

void FuramiBridge::forceSurfaceLossForProof()
{
    assertGuiThread();
    if (m_actualPhase != QStringLiteral("Ready") || m_failed || !m_host || m_poisoned)
        return;
    qInfo().noquote() << QStringLiteral("input_intent action=ForceSurfaceLoss source=panel root=%1 host=%2 generation=%3")
        .arg(m_rootXid).arg(m_lastHostXid).arg(m_generation);
    // This is deliberately unauthorized. Only the real platform event reports loss.
    m_host->destroy();
}

void FuramiBridge::requestApplicationClose()
{
    assertGuiThread();
    if (m_quitAuthorized)
        return;
    qInfo().noquote() << QStringLiteral("input_intent action=ApplicationClose root=%1 generation=%2 phase=%3")
        .arg(m_rootXid).arg(m_generation).arg(m_actualPhase);
    applyUpdate(gate_quit(*m_gate));
}

void FuramiBridge::togglePanel()
{
    assertGuiThread();
    setPanelVisible(!m_panelVisible);
}

void FuramiBridge::toggleFullscreen()
{
    assertGuiThread();
    if (!m_root)
        return;
    const bool wasFullscreen = m_root->visibility() == QWindow::FullScreen;
    qInfo().noquote() << QStringLiteral("input_intent action=ToggleFullscreen root=%1 generation=%2 fullscreen=%3")
        .arg(m_rootXid).arg(m_generation).arg(boolean(!wasFullscreen));
    if (wasFullscreen)
        m_root->showNormal();
    else
        m_root->showFullScreen();
    scheduleGeometryLog();
}

void FuramiBridge::focusVideo()
{
    assertGuiThread();
    if (!m_host || m_poisoned || m_failed || !m_container)
        return;
    m_container->forceActiveFocus(Qt::MouseFocusReason);
    m_host->requestActivate();
    qInfo().noquote() << QStringLiteral("input_intent action=FocusVideo root=%1 host=%2 generation=%3 cursor=Qt_ArrowCursor")
        .arg(m_rootXid).arg(m_lastHostXid).arg(m_generation);
}

bool FuramiBridge::handleKey(QKeyEvent *event, const char *source)
{
    assertGuiThread();
    qInfo().noquote() << QStringLiteral("input_event source=%1 kind=key_press key=%2 repeat=%3 text_entry=%4 popup=%5 generation=%6")
        .arg(QString::fromLatin1(source)).arg(event->key()).arg(boolean(event->isAutoRepeat()))
        .arg(boolean(m_textEntryActive)).arg(boolean(m_popupOpen)).arg(m_generation);
    const int key = event->key();
    if (qstrcmp(source, "host") == 0 && m_root && m_container
        && (key == Qt::Key_Tab || key == Qt::Key_Backtab)
        && !(event->modifiers() & (Qt::ControlModifier | Qt::AltModifier | Qt::MetaModifier))) {
        const bool forward = key == Qt::Key_Tab && !event->modifiers().testFlag(Qt::ShiftModifier);
        auto *target = m_container->nextItemInFocusChain(forward);
        if (target && target != m_container.data() && target->window() == m_root.data()) {
            const auto reason = forward ? Qt::TabFocusReason : Qt::BacktabFocusReason;
            // A native child deactivates the root. Record the real Qt chain target,
            // then request native focus back; root FocusIn activates that target.
            target->forceActiveFocus(reason);
            m_root->requestActivate();
            qInfo().noquote() << QStringLiteral("input_focus_handoff source=host direction=%1 target=%2 root=%3 generation=%4")
                .arg(forward ? QStringLiteral("forward") : QStringLiteral("backward"))
                .arg(target->objectName()).arg(m_rootXid).arg(m_generation);
            event->accept();
            return true;
        }
    }
    if (m_textEntryActive || m_popupOpen)
        return false;
    // Panel controls retain normal Qt Space activation. Playback owns Space only
    // while native video, or its root-side container focus, owns keyboard input.
    if (key == Qt::Key_Space && qstrcmp(source, "host") != 0
        && (!m_root || m_root->activeFocusItem() != m_container.data()))
        return false;
    if (key != Qt::Key_Space && key != Qt::Key_F11 && key != Qt::Key_Escape)
        return false;
    if (event->isAutoRepeat()) {
        event->accept();
        return true;
    }
    if (key == Qt::Key_Space) {
        const auto status = gate_pause(*m_gate, m_generation);
        qInfo().noquote() << QStringLiteral("input_intent action=TogglePause source=%1 generation=%2 status=%3")
            .arg(QString::fromLatin1(source)).arg(m_generation).arg(statusName(status));
        // Overload/failure is visible through the same nonblocking coordinator poll.
        applyUpdate(gate_poll(*m_gate));
    } else if (key == Qt::Key_F11) {
        toggleFullscreen();
    } else if (m_root && m_root->visibility() == QWindow::FullScreen) {
        toggleFullscreen();
    } else {
        qInfo().noquote() << QStringLiteral("input_intent action=HidePanel source=%1 generation=%2")
            .arg(QString::fromLatin1(source)).arg(m_generation);
        setPanelVisible(false);
    }
    event->accept();
    return true;
}

void FuramiBridge::forwardToRoot(QEvent *event, const char *source)
{
    assertGuiThread();
    if (!m_root || m_poisoned) {
        event->ignore();
        return;
    }
    qInfo().noquote() << QStringLiteral("input_forward source=%1 event=%2 text_entry=%3 generation=%4")
        .arg(QString::fromLatin1(source)).arg(static_cast<int>(event->type()))
        .arg(boolean(m_textEntryActive)).arg(m_generation);
    QCoreApplication::sendEvent(m_root, event);
}

bool FuramiBridge::eventFilter(QObject *watched, QEvent *event)
{
    assertGuiThread();
    if (event->type() == QEvent::Quit && watched == qGuiApp && !m_quitAuthorized) {
        qInfo().noquote() << "qt_quit_veto";
        requestApplicationClose();
        return true;
    }
    if (watched == m_root) {
        if (event->type() == QEvent::FocusIn || event->type() == QEvent::FocusOut) {
            qInfo().noquote() << QStringLiteral("input_focus_window source=root event=%1 root=%2 generation=%3")
                .arg(event->type() == QEvent::FocusIn ? QStringLiteral("FocusIn") : QStringLiteral("FocusOut"))
                .arg(m_rootXid).arg(m_generation);
        }
        if (event->type() == QEvent::Close && !m_quitAuthorized) {
            event->ignore();
            qInfo().noquote() << QStringLiteral("qt_root_close_veto root=%1 generation=%2")
                .arg(m_rootXid).arg(m_generation);
            requestApplicationClose();
            return true;
        }
        if (event->type() == QEvent::KeyPress)
            return handleKey(static_cast<QKeyEvent *>(event), "root");
        if (event->type() == QEvent::Resize || event->type() == QEvent::WindowStateChange)
            scheduleGeometryLog();
    }
    return QObject::eventFilter(watched, event);
}

void FuramiBridge::applyUpdate(UiUpdate update)
{
    assertGuiThread();
    if (!update.changed)
        return;
    m_actualPhase = phaseName(update.phase);
    m_generation = update.generation;
    m_failed = update.failed || m_bootstrapFailed;
    m_paused = update.paused;
    m_ended = update.ended;
    m_diagnostic = fromRust(update.diagnostic);
    if (m_failed && !m_nativeDiagnostic.isEmpty()) {
        m_diagnostic = m_diagnostic.isEmpty() ? m_nativeDiagnostic
            : m_nativeDiagnostic + QStringLiteral("; ") + m_diagnostic;
    }
    qInfo().noquote() << QStringLiteral("ui_phase phase=%1 visible_phase=%2 generation=%3 failed=%4 paused=%5 ended=%6 diagnostic=%7")
        .arg(m_actualPhase, phase()).arg(m_generation).arg(boolean(m_failed))
        .arg(boolean(m_paused)).arg(boolean(m_ended)).arg(m_diagnostic);
    emit stateChanged();
    scheduleGeometryLog();
    // The reducer emits effects in this order. Native effects never invent an ack.
    if (update.create_native)
        createNative(update.generation);
    if (update.release_native) {
        qInfo().noquote() << QStringLiteral("ui_stop_ack generation=%1 root=%2 host=%3")
            .arg(update.generation).arg(m_rootXid).arg(m_lastHostXid);
        if (m_insideSurfaceCallback) {
            const auto generation = update.generation;
            QTimer::singleShot(0, this, [this, generation] { releaseNative(generation); });
        } else {
            releaseNative(update.generation);
        }
    }
    if (update.quit) {
        Q_ASSERT(m_nativeGeneration == 0);
        m_quitAuthorized = true;
        qInfo().noquote() << QStringLiteral("qt_quit_authorized generation=%1 root=%2")
            .arg(update.generation).arg(m_rootXid);
        QTimer::singleShot(0, qGuiApp, &QCoreApplication::quit);
    }
}

void FuramiBridge::createNative(std::uint64_t generation)
{
    assertGuiThread();
    if (!generation || m_nativeGeneration || m_host) {
        failNative(generation, QStringLiteral("native creation requires no prior generation/host"));
        return;
    }
    m_nativeGeneration = generation;
    if (!m_root || !m_container) {
        failNative(generation, QStringLiteral("native creation lost permanent root/container"));
        return;
    }
    m_published = false;
    m_poisoned = false;
    m_lastHostXid = 0;
    auto *host = new HostWindow(this, generation);
    QQmlEngine::setObjectOwnership(host, QQmlEngine::CppOwnership);
    m_host = host;
    connect(host, &QObject::destroyed, this, [this, generation] {
        assertGuiThread();
        qInfo().noquote() << QStringLiteral("native_object_destroyed generation=%1 host=%2 authorized=%3")
            .arg(generation).arg(m_lastHostXid).arg(boolean(m_releasing));
        if (!m_releasing && generation == m_nativeGeneration) {
            m_host.clear();
            // WindowContainer first receives its own windowDestroyed notification.
            QTimer::singleShot(0, this, [this] { emit hostWindowChanged(); });
            if (!m_poisoned)
                failNative(generation, QStringLiteral("host QObject was destroyed before owner acknowledgement"));
        }
    });
    qInfo().noquote() << QStringLiteral("native_create generation=%1 root=%2")
        .arg(generation).arg(m_rootXid);
    emit hostWindowChanged();
    schedulePublication(generation);
}

void FuramiBridge::schedulePublication(std::uint64_t generation)
{
    assertGuiThread();
    QTimer::singleShot(0, this, [this, generation] { publishNative(generation); });
}

void FuramiBridge::publishNative(std::uint64_t generation)
{
    assertGuiThread();
    if (generation != m_nativeGeneration || generation != m_generation || m_published || m_poisoned
        || m_failed || m_actualPhase != QStringLiteral("WaitingSurface")) {
        qInfo().noquote() << QStringLiteral("native_publication_rejected generation=%1 current=%2 phase=%3 poisoned=%4")
            .arg(generation).arg(m_generation).arg(m_actualPhase).arg(boolean(m_poisoned));
        return;
    }
    if (!m_host || !m_root || !m_container) {
        failNative(generation, QStringLiteral("native publication lost root/container/host QObject"));
        return;
    }
    // WindowContainer may not have polished yet. SurfaceCreated posts another callback.
    if (!m_host->handle())
        return;
    if (m_host->poisoned() || m_host->parent() != m_root.data()
        || m_container->window() != m_root.data()
        || m_container->property("window").value<QWindow *>() != m_host.data()) {
        failNative(generation, QStringLiteral("native publication rejected incorrect Qt root/container parent association"));
        return;
    }
#if QT_CONFIG(xcb)
    auto *native = qGuiApp->nativeInterface<QNativeInterface::QX11Application>();
    if (!native || !native->display()) {
        failNative(generation, QStringLiteral("Qt X11 display disappeared before native publication"));
        return;
    }
    const auto xid = static_cast<std::uint64_t>(m_host->winId());
    if (!xid || xid > std::numeric_limits<std::uint32_t>::max()) {
        failNative(generation, QStringLiteral("native host XID is not a nonzero 32-bit X11 window ID"));
        return;
    }
    Window treeRoot = 0;
    Window parent = 0;
    Window *children = nullptr;
    unsigned int count = 0;
    const int queried = XQueryTree(native->display(), static_cast<Window>(xid), &treeRoot, &parent, &children, &count);
    if (children)
        XFree(children);
    if (!queried || static_cast<std::uint64_t>(parent) != m_rootXid) {
        failNative(generation, QStringLiteral("XQueryTree did not confirm host directly beneath intended application root"));
        return;
    }
    m_published = true;
    m_lastHostXid = xid;
    m_host->rememberXid(xid);
    qInfo().noquote() << QStringLiteral("native_publish root=%1 host=%2 generation=%3 x11_parent=%4")
        .arg(m_rootXid).arg(xid).arg(generation).arg(static_cast<std::uint64_t>(parent));
    auto update = gate_surface_ready(*m_gate, generation, xid);
    const bool closeDuringOpening = m_closeDuringOpening == generation && update.phase == GatePhase::Opening;
    applyUpdate(std::move(update));
    if (closeDuringOpening) {
        m_closeDuringOpening = 0;
        auto close = gate_close(*m_gate, generation, false);
        qInfo().noquote() << QStringLiteral("proof_close_during_opening generation=%1 observed_phase=Opening accepted=%2")
            .arg(generation).arg(boolean(close.phase == GatePhase::Stopping && !close.failed));
        applyUpdate(std::move(close));
    }
    scheduleGeometryLog();
#else
    failNative(generation, QStringLiteral("Qt was built without the xcb native interface"));
#endif
}

void FuramiBridge::surfaceLost(std::uint64_t generation, std::uint64_t previousXid)
{
    assertGuiThread();
    if (generation != m_nativeGeneration || m_releasing || m_poisoned)
        return;
    m_poisoned = true;
    m_closeDuringOpening = 0;
    m_nativeDiagnostic = QStringLiteral("uncontrolled native surface loss for generation %1, previous XID %2")
        .arg(generation).arg(previousXid);
    qCritical().noquote() << QStringLiteral("native_surface_loss generation=%1 host=%2 root=%3 phase=%4 controlled=false")
        .arg(generation).arg(previousXid).arg(m_rootXid).arg(m_actualPhase);
    m_insideSurfaceCallback = true;
    applyUpdate(gate_surface_lost(*m_gate, generation));
    m_insideSurfaceCallback = false;
}

void FuramiBridge::failNative(std::uint64_t generation, const QString &diagnostic)
{
    assertGuiThread();
    if (m_host)
        m_host->poison();
    m_poisoned = true;
    m_nativeDiagnostic = diagnostic;
    qCritical().noquote() << QStringLiteral("native_failure generation=%1 root=%2 host=%3 diagnostic=%4")
        .arg(generation).arg(m_rootXid).arg(m_lastHostXid).arg(diagnostic);
    applyUpdate(gate_surface_lost(*m_gate, generation));
}

void FuramiBridge::releaseNative(std::uint64_t generation)
{
    assertGuiThread();
    if (!generation || generation != m_nativeGeneration || m_actualPhase != QStringLiteral("Releasing") || m_releasing) {
        qCritical().noquote() << QStringLiteral("native_release_rejected generation=%1 current=%2 phase=%3")
            .arg(generation).arg(m_nativeGeneration).arg(m_actualPhase);
        return;
    }
    m_releasing = true;
    QPointer<HostWindow> surviving = m_host;
    if (surviving) {
        surviving->authorizeDestruction();
        qInfo().noquote() << QStringLiteral("native_destroy_begin generation=%1 host=%2 root=%3 parent_preserved=%4")
            .arg(generation).arg(m_lastHostXid).arg(m_rootXid)
            .arg(boolean(surviving->parent() == m_root.data()));
        surviving->hide();
        surviving->destroy();
    }
    m_host.clear();
    emit hostWindowChanged();
    // Clear only after platform destruction, while QPointer guards parent-driven deletion.
    if (m_container)
        m_container->setProperty("window", QVariant::fromValue<QWindow *>(nullptr));
    if (surviving)
        delete surviving.data();
    qInfo().noquote() << QStringLiteral("native_destroy generation=%1 host=%2 root=%3 authorized=true poisoned=%4")
        .arg(generation).arg(m_lastHostXid).arg(m_rootXid).arg(boolean(m_poisoned));
    m_nativeGeneration = 0;
    m_releasing = false;
    m_published = false;
    m_closeDuringOpening = 0;
    applyUpdate(gate_native_released(*m_gate, generation));
}

void FuramiBridge::scheduleGeometryLog()
{
    assertGuiThread();
    if (m_geometryLogPending || !m_root)
        return;
    m_geometryLogPending = true;
    // Read layout only after a real Qt frame, never force native geometry from logging.
    connect(m_root, &QQuickWindow::frameSwapped, this, [this] {
        m_geometryLogPending = false;
        logGeometry();
    }, static_cast<Qt::ConnectionType>(Qt::QueuedConnection | Qt::SingleShotConnection));
    m_root->update();
}

void FuramiBridge::logGeometry()
{
    assertGuiThread();
    if (!m_root || !m_container)
        return;
    const auto video = m_container->mapRectToScene(QRectF(0, 0, m_container->width(), m_container->height()));
    const qreal dpr = m_root->devicePixelRatio();
    qInfo().noquote() << QStringLiteral("qt_geometry root=%1 generation=%2 root_width=%3 root_height=%4 video_x=%5 video_y=%6 video_width=%7 video_height=%8 panel=%9 dpr=%10")
        .arg(m_rootXid).arg(m_generation).arg(m_root->width()).arg(m_root->height())
        .arg(video.x()).arg(video.y()).arg(video.width()).arg(video.height()).arg(boolean(m_panelVisible)).arg(dpr);
    const char *names[] = {"openProof", "closeProof", "closeDuringOpenProof", "forceSurfaceLossProof",
                          "proofText", "togglePanelProof", "fullscreenProof", "popupOpenProof",
                          "closePopupProof", "videoContainer", "proofPanel"};
    for (const auto *name : names) {
        auto *item = m_root->findChild<QQuickItem *>(QString::fromLatin1(name));
        if (!item)
            continue;
        const auto rect = item->mapRectToScene(QRectF(0, 0, item->width(), item->height()));
        qInfo().noquote() << QStringLiteral("proof_control object=%1 x=%2 y=%3 width=%4 height=%5 visible=%6 enabled=%7")
            .arg(QString::fromLatin1(name)).arg(qRound(rect.x() * dpr)).arg(qRound(rect.y() * dpr))
            .arg(qRound(rect.width() * dpr)).arg(qRound(rect.height() * dpr))
            .arg(boolean(item->isVisible())).arg(boolean(item->isEnabled()));
    }
}

LaunchResult run_qt_application(rust::Str media_prefix, rust::Str display)
{
    const QByteArray captured(display.data(), static_cast<qsizetype>(display.size()));
    if (captured.isEmpty() || qgetenv("DISPLAY").isEmpty())
        return launchFailure(QStringLiteral("missing X11 DISPLAY; Furami requires XWayland/xcb"));
    if (qgetenv("DISPLAY") != captured)
        return launchFailure(QStringLiteral("DISPLAY changed before Qt initialization"));
    if (!qputenv("QT_QPA_PLATFORM", "xcb") || !qputenv("LC_NUMERIC", "C") || !std::setlocale(LC_NUMERIC, "C"))
        return launchFailure(QStringLiteral("cannot establish xcb and LC_NUMERIC=C before Qt initialization"));
    if (!XInitThreads())
        return launchFailure(QStringLiteral("XInitThreads failed before Qt initialization"));

    int argc = 1;
    char program[] = "furami";
    char *argv[] = {program, nullptr};
    QGuiApplication application(argc, argv);
    QObject::connect(&application, &QObject::destroyed, [] {
        qInfo().noquote() << "qt_application_destroy";
    });
    application.setApplicationName(QStringLiteral("Furami"));
    application.setQuitOnLastWindowClosed(false);
    if (QString::fromLatin1(qVersion()) != QStringLiteral("6.11.2"))
        return launchFailure(QStringLiteral("runtime Qt version differs from frozen 6.11.2: %1").arg(QString::fromLatin1(qVersion())));
#if QT_CONFIG(xcb)
    auto *native = application.nativeInterface<QNativeInterface::QX11Application>();
    if (!native || !native->display() || !native->connection())
        return launchFailure(QStringLiteral("Qt xcb native X11 display/connection is missing"));
    const QByteArray platform = QGuiApplication::platformName().toUtf8();
    const QByteArray qtDisplay = XDisplayString(native->display());
    const QByteArray current = qgetenv("DISPLAY");
    const auto comparison = display_check(asRust(platform), asRust(captured), asRust(qtDisplay), asRust(current));
    if (!comparison.empty())
        return launchFailure(fromRust(comparison));
    xcb_generic_error_t *shapeError = nullptr;
    const auto shapeCookie = xcb_shape_query_version(native->connection());
    auto *shape = xcb_shape_query_version_reply(native->connection(), shapeCookie, &shapeError);
    const bool shapeValid = shape && !shapeError && (shape->major_version > 1 || (shape->major_version == 1 && shape->minor_version >= 1));
    const unsigned int major = shape ? shape->major_version : 0;
    const unsigned int minor = shape ? shape->minor_version : 0;
    std::free(shapeError);
    std::free(shape);
    if (!shapeValid)
        return launchFailure(QStringLiteral("X11 SHAPE >=1.1 is required for mpv cursor/input passthrough"));
    qInfo().noquote() << QStringLiteral("native_startup qt=%1 platform=%2 captured_display=%3 qt_display=%4 current_display=%5 shape=%6.%7")
        .arg(QString::fromLatin1(qVersion()), QString::fromUtf8(platform), QString::fromUtf8(captured),
             QString::fromUtf8(qtDisplay), QString::fromUtf8(current)).arg(major).arg(minor);
#else
    return launchFailure(QStringLiteral("Qt was built without the required xcb native interface"));
#endif

    // Construction order keeps bridge/timer alive until after the QML engine/root dies.
    FuramiBridge bridge(media_prefix);
    QQmlApplicationEngine engine;
    QObject::connect(&engine, &QObject::destroyed, &application, [] {
        qInfo().noquote() << "qt_engine_destroy";
    });
    engine.setInitialProperties({{QStringLiteral("bridge"), QVariant::fromValue(&bridge)}});
    QPointer<QQuickWindow> root;
    QString rootDiagnostic;
    bool creationComplete = false;
    QObject::connect(&engine, &QQmlApplicationEngine::objectCreated, &bridge,
        [&](QObject *object, const QUrl &) {
            creationComplete = true;
            root = qobject_cast<QQuickWindow *>(object);
            if (!root)
                rootDiagnostic = QStringLiteral("embedded Main.qml did not create a QQuickWindow root");
        });
    QObject::connect(&engine, &QQmlApplicationEngine::objectCreationFailed, &bridge,
        [&](const QUrl &) {
            creationComplete = true;
            rootDiagnostic = QStringLiteral("failed to create QML root object from embedded Main.qml");
        });
    engine.load(QUrl(QStringLiteral("qrc:/qt/qml/dev/antho/furami/qml/Main.qml")));
    if (!creationComplete || !root || engine.rootObjects().size() != 1)
        return launchFailure(rootDiagnostic.isEmpty() ? QStringLiteral("QML root creation is missing or still pending") : rootDiagnostic);
    auto *container = root->findChild<QQuickItem *>(QStringLiteral("videoContainer"));
    if (!bridge.bindRoot(root, container, rootDiagnostic))
        return launchFailure(rootDiagnostic);
    int exitCode = application.exec();
    if (!bridge.quitAuthorized()) {
        // An unsolicited loop exit cannot discard a live owner or its native parent.
        qCritical().noquote() << "qt_uncontrolled_event_loop_exit requesting_owner_cleanup";
        bridge.requestApplicationClose();
        application.exec();
        exitCode = 1;
    }
    qInfo().noquote() << QStringLiteral("qt_event_loop_exit code=%1 owner_cleanup_authorized=%2")
        .arg(exitCode).arg(boolean(bridge.quitAuthorized()));
    if (bridge.failed())
        return launchFailure(bridge.diagnostic());
    if (exitCode)
        return launchFailure(QStringLiteral("Qt event loop exited with status %1").arg(exitCode));
    return LaunchResult{0, rust::String()};
}

} // namespace furami::bridge
