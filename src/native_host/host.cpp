#include "host.h"
#include "furami/src/ui/bridge.cxx.h"

#include <QCoreApplication>
#include <QDebug>
#include <QGuiApplication>
#include <QInputMethodEvent>
#include <QKeyEvent>
#include <QMouseEvent>
#include <QPlatformSurfaceEvent>
#include <QResizeEvent>
#include <QScreen>
#include <QQmlApplicationEngine>
#include <QQmlEngine>
#include <QThread>
#include <QSocketNotifier>
#include <QVariant>
#include <QtGui/qguiapplication_platform.h>

#include <X11/Xlib.h>
#include <xcb/shape.h>
#include <xcb/xcb.h>

#include <clocale>
#include <cerrno>
#include <cstdlib>
#include <limits>
#include <utility>

#include <fcntl.h>
#include <unistd.h>

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
                    // No winId, reparent, or nested event loop is allowed here.
                    // The exceptional owner barrier completes before Qt destroys this parent.
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

FuramiBridge::FuramiBridge(rust::Box<RuntimeCoordinator> gate)
    : m_gate(std::move(gate))
{
    assertGuiThread();
    QQmlEngine::setObjectOwnership(this, QQmlEngine::CppOwnership);
    m_captureSelected = gate_capture_selected(*m_gate);
    applyUpdate(gate_poll(*m_gate));
    // X11 may never acknowledge a WM state request. This deadline abandons
    // intent/focus hooks only: it proves neither rejection nor success.
    m_fullscreenChoiceDeadline.setSingleShot(true);
    m_fullscreenChoiceDeadline.setInterval(5000);
    connect(&m_fullscreenChoiceDeadline, &QTimer::timeout, this, [this] {
        if (m_fullscreenChoicePending || m_fullscreenFocusRestorePending)
            cancelFullscreenChoice("deadline");
    });
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
    if (m_qualificationInput) {
        m_qualificationInput->setEnabled(false);
        delete m_qualificationInput.data();
    }
    if (m_stdinFlags != -1 && ::fcntl(STDIN_FILENO, F_SETFL, m_stdinFlags) == -1)
        qWarning() << "qualification_stdin_restore_failed errno=" << errno;
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
bool FuramiBridge::presentationSuppressed() const { assertGuiThread(); return m_presentationSuppressed; }
int FuramiBridge::volumePercent() const { assertGuiThread(); return m_volumePercent; }
bool FuramiBridge::muted() const { assertGuiThread(); return m_muted; }
bool FuramiBridge::canTogglePause() const { assertGuiThread(); return m_canTogglePause; }
bool FuramiBridge::canSetGain() const { assertGuiThread(); return m_canSetGain; }
QString FuramiBridge::playbackStatus() const { assertGuiThread(); return m_playbackStatus; }
QString FuramiBridge::productPhase() const { assertGuiThread(); return m_productPhase; }
QString FuramiBridge::recoveryEvidence() const { assertGuiThread(); return m_recoveryEvidence; }
QString FuramiBridge::recoveryStage() const { assertGuiThread(); return m_recoveryStage; }
QString FuramiBridge::recoveryCandidates() const { assertGuiThread(); return m_recoveryCandidates; }
QString FuramiBridge::audioDesired() const { assertGuiThread(); return m_audioDesired; }
QString FuramiBridge::outputRows() const { assertGuiThread(); return m_outputRows; }
QString FuramiBridge::outputCatalogRevision() const
{
    assertGuiThread();
    return QString::number(m_outputCatalogRevision);
}
QString FuramiBridge::outputSelected() const { assertGuiThread(); return m_outputSelected; }
QString FuramiBridge::outputSelectedKey() const { assertGuiThread(); return m_outputSelectedKey; }
QString FuramiBridge::outputEffective() const { assertGuiThread(); return m_outputEffective; }
QString FuramiBridge::outputStatus() const { assertGuiThread(); return m_outputStatus; }
bool FuramiBridge::outputNeedsAction() const { assertGuiThread(); return m_outputNeedsAction; }
bool FuramiBridge::captureSelected() const { assertGuiThread(); return m_captureSelected; }
bool FuramiBridge::canOpen() const { assertGuiThread(); return m_canOpen; }
bool FuramiBridge::canRestart() const { assertGuiThread(); return m_canRestart; }
QString FuramiBridge::restartGeneration() const { assertGuiThread(); return QString::number(m_restartGeneration); }
QString FuramiBridge::audioStatus() const { assertGuiThread(); return m_audioStatus; }
QString FuramiBridge::audioDiagnostic() const { assertGuiThread(); return m_audioDiagnostic; }
QString FuramiBridge::audioSource() const { assertGuiThread(); return m_audioSource; }
bool FuramiBridge::textEntryActive() const { assertGuiThread(); return m_textEntryActive; }
bool FuramiBridge::panelVisible() const { assertGuiThread(); return m_panelVisible; }
bool FuramiBridge::popupOpen() const { assertGuiThread(); return m_popupOpen; }
bool FuramiBridge::quitAuthorized() const { assertGuiThread(); return m_quitAuthorized; }
bool FuramiBridge::failed() const { assertGuiThread(); return m_failed; }
QString FuramiBridge::settingsStatus() const { assertGuiThread(); return m_settingsStatus; }
QString FuramiBridge::settingsPath() const { assertGuiThread(); return m_settingsPath; }
bool FuramiBridge::settingsRefused() const { assertGuiThread(); return m_settingsRefused; }
QString FuramiBridge::savedSelection() const { assertGuiThread(); return m_savedSelection; }
QString FuramiBridge::startupReason() const { assertGuiThread(); return m_startupReason; }
bool FuramiBridge::draftDirty() const { assertGuiThread(); return m_draftDirty; }
QString FuramiBridge::closeDialog() const { assertGuiThread(); return m_closeDialog; }
QString FuramiBridge::closeRevision() const { assertGuiThread(); return QString::number(m_closeRevision); }
QString FuramiBridge::resetToken() const { assertGuiThread(); return QString::number(m_resetToken); }
bool FuramiBridge::fullscreenPreference() const { assertGuiThread(); return m_fullscreenPreference; }
bool FuramiBridge::closing() const { assertGuiThread(); return m_closing; }
int FuramiBridge::screenAvailableWidth() const
{
    assertGuiThread();
    const auto *screen = m_root ? m_root->screen() : QGuiApplication::primaryScreen();
    return screen ? screen->availableGeometry().width() : 0;
}
int FuramiBridge::screenAvailableHeight() const
{
    assertGuiThread();
    const auto *screen = m_root ? m_root->screen() : QGuiApplication::primaryScreen();
    return screen ? screen->availableGeometry().height() : 0;
}

void FuramiBridge::watchScreenGeometry()
{
    assertGuiThread();
    QObject::disconnect(m_screenGeometryConnection);
    if (m_root && m_root->screen()) {
        m_screenGeometryConnection = connect(m_root->screen(), &QScreen::availableGeometryChanged,
            this, [this](const QRect &) { emit screenGeometryChanged(); });
    }
    emit screenGeometryChanged();
}

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
    m_fullscreenObserved = root->visibility() == QWindow::FullScreen;
    qInfo().noquote() << QStringLiteral("native_root root=%1 container=videoContainer generation=0")
        .arg(m_rootXid);
    connect(root, &QWindow::widthChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QWindow::heightChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QWindow::screenChanged, this, [this] { watchScreenGeometry(); scheduleGeometryLog(); });
    watchScreenGeometry();
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
        cancelFullscreenChoice("root-destroyed");
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
    connect(root, &QWindow::visibilityChanged, this, [this](QWindow::Visibility visibility) {
        if (visibility == QWindow::Hidden || visibility == QWindow::Minimized)
            cancelFullscreenChoice("not-visible");
        else if (m_fullscreenChoicePending && m_root)
            m_root->update();
    });
    QTimer::singleShot(0, this, [this] {
        if (!m_root) return;
        if (m_fullscreenPreference && m_root->visibility() != QWindow::FullScreen) toggleFullscreen();
        applyUpdate(gate_ui_ready(*m_gate));
    });
    scheduleGeometryLog();
    return true;
}

bool FuramiBridge::enableQualificationInput(QString &diagnostic)
{
    assertGuiThread();
    const int flags = ::fcntl(STDIN_FILENO, F_GETFL);
    if (flags == -1 || ::fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK) == -1) {
        diagnostic = QStringLiteral("cannot make opt-in qualification stdin nonblocking: errno %1").arg(errno);
        return false;
    }
    m_stdinFlags = flags;
    m_qualificationLine.reserve(256);
    m_qualificationInput = new QSocketNotifier(STDIN_FILENO, QSocketNotifier::Read, this);
    connect(m_qualificationInput.data(), &QSocketNotifier::activated, this,
        [this](QSocketDescriptor, QSocketNotifier::Type) { readQualificationInput(); });
    qInfo().noquote() << "qualification_stdin_enabled grammar=snapshot|draft-video|draft-identity|draft-audio|draft-source|open|apply|restart|reconnect|volume|mute|close|quit state=phase,apply,attempt,cleanup revision=required_for_draft_apply";
    return true;
}

void FuramiBridge::readQualificationInput()
{
    assertGuiThread();
    char bytes[512];
    const auto count = ::read(STDIN_FILENO, bytes, sizeof(bytes));
    if (count == -1 && (errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR))
        return;
    if (count <= 0) {
        m_qualificationInput->setEnabled(false);
        if (count == -1)
            qWarning() << "qualification_stdin_read_failed errno=" << errno;
        else
            qInfo() << "qualification_stdin_eof";
        m_qualificationLine.clear();
        return;
    }
    // One bounded read per callback. Oversize lines are discarded whole, not
    // interpreted as command fragments. No inventory event submits controls.
    for (ssize_t index = 0; index < count; ++index) {
        const char byte = bytes[index];
        if (byte == '\n') {
            if (!m_dropQualificationLine && !m_qualificationLine.isEmpty()) {
                const auto command = QString::fromUtf8(m_qualificationLine).toUtf8();
                qInfo().noquote() << "qualification_command" << QString::fromUtf8(command);
                if (command == "fullscreen") toggleFullscreen();
                else if (command == "application-close") requestApplicationClose();
                else if (command == "reset-confirm") decideSettingsReset(QString::number(m_resetToken), true);
                else {
                    if (command == "close-discard" || command.startsWith("quit ")) observeFullscreenChoice();
                    applyUpdate(gate_qualification_command(*m_gate, asRust(command)));
                }
            }
            m_qualificationLine.clear();
            m_dropQualificationLine = false;
        } else if (!m_dropQualificationLine) {
            if (m_qualificationLine.size() == 256) {
                m_dropQualificationLine = true;
                m_qualificationLine.clear();
                qWarning() << "qualification_command_rejected reason=line_exceeds_256_bytes";
            } else {
                m_qualificationLine.append(byte);
            }
        }
    }
}

void FuramiBridge::openCapture()
{
    assertGuiThread();
    if (!m_canOpen || m_bootstrapFailed)
        return;
    qInfo().noquote() << "input_intent action=OpenCapture source=panel";
    applyUpdate(gate_open(*m_gate));
}

void FuramiBridge::restartCapture(const QString &expectedGeneration)
{
    assertGuiThread();
    bool valid = false;
    const auto generation = expectedGeneration.toULongLong(&valid);
    if (!valid || !m_canRestart || m_bootstrapFailed)
        return;
    qInfo().noquote() << QStringLiteral("input_intent action=RestartCapture source=panel generation=%1 current_attempt=%2 can_restart=%3")
        .arg(generation).arg(m_restartGeneration).arg(boolean(m_canRestart));
    applyUpdate(gate_restart(*m_gate, generation));
}

void FuramiBridge::reconnectCapture(const QString &expectedGeneration)
{
    assertGuiThread();
    bool valid = false;
    const auto generation = expectedGeneration.toULongLong(&valid);
    if (!valid || m_bootstrapFailed)
        return;
    qInfo().noquote() << QStringLiteral("input_intent action=ReconnectCapture source=panel generation=%1 current_attempt=%2")
        .arg(generation).arg(m_restartGeneration);
    applyUpdate(gate_reconnect(*m_gate, generation));
}

void FuramiBridge::chooseRecovery(const QString &expectedGeneration, const QString &token)
{
    assertGuiThread();
    bool validGeneration = false;
    const auto generation = expectedGeneration.toULongLong(&validGeneration);
    // Token layout mirrors the candidate entries published through
    // m_recoveryCandidates: "watch:epoch:candidate|description|...".
    const QStringList identity = token.section(QLatin1Char('|'), 0, 0).split(QLatin1Char(':'));
    bool validWatch = false;
    bool validEpoch = false;
    bool validCandidate = false;
    const auto watch = identity.value(0).toULongLong(&validWatch);
    const auto epoch = identity.value(1).toULongLong(&validEpoch);
    const auto candidate = identity.value(2).toULongLong(&validCandidate);
    if (!validGeneration || !validWatch || !validEpoch || !validCandidate || m_bootstrapFailed)
        return;
    qInfo().noquote() << QStringLiteral("input_intent action=ChooseRecovery source=panel generation=%1 watch=%2 epoch=%3 candidate=%4")
        .arg(generation).arg(watch).arg(epoch).arg(candidate);
    applyUpdate(gate_choose_recovery(*m_gate, generation, watch, epoch, candidate));
}

void FuramiBridge::closeCapture()
{
    assertGuiThread();
    qInfo().noquote() << QStringLiteral("input_intent action=CloseSession source=panel generation=%1 phase=%2")
        .arg(m_generation).arg(m_actualPhase);
    applyUpdate(gate_close(*m_gate, m_generation, false));
}

void FuramiBridge::togglePause()
{
    assertGuiThread();
    if (!m_canTogglePause || m_bootstrapFailed)
        return;
    const auto status = gate_pause(*m_gate, m_generation);
    qInfo().noquote() << QStringLiteral("input_intent action=TogglePause generation=%1 status=%2")
        .arg(m_generation).arg(statusName(status));
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::setVolume(int percent)
{
    assertGuiThread();
    const auto status = gate_set_volume(*m_gate, percent);
    qInfo().noquote() << QStringLiteral("input_intent action=SetVolume percent=%1 status=%2")
        .arg(percent).arg(statusName(status));
    // Every admission outcome publishes the authoritative retained preference.
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::setMuted(bool muted)
{
    assertGuiThread();
    const auto status = gate_set_muted(*m_gate, muted);
    qInfo().noquote() << QStringLiteral("input_intent action=SetMuted muted=%1 status=%2")
        .arg(boolean(muted)).arg(statusName(status));
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::selectOutput(const QString &rowKey, const QString &catalogRevision)
{
    assertGuiThread();
    bool revisionValid = false;
    const auto revision = catalogRevision.toULongLong(&revisionValid);
    if (!revisionValid) {
        qInfo().noquote() << QStringLiteral("input_intent action=SelectOutput row=%1 status=RejectedRevision")
            .arg(rowKey);
        return;
    }
    const auto status = gate_select_output(*m_gate, asRust(rowKey.toUtf8()), revision);
    qInfo().noquote() << QStringLiteral("input_intent action=SelectOutput row=%1 revision=%2 status=%3")
        .arg(rowKey).arg(revision).arg(statusName(status));
    // Every admission outcome republishes the authoritative projection,
    // including the unchanged selection after a stale/failed attempt.
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::openAndCloseDuringOpeningForProof()
{
    assertGuiThread();
    if (!m_canOpen || m_bootstrapFailed)
        return;
    qInfo().noquote() << "input_intent action=OpenAndCloseDuringOpening source=panel";
    m_closeOnNextOpening = true;
    applyUpdate(gate_open(*m_gate));
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
    // Only an already-observed and synchronized transition may commit here.
    // Otherwise shutdown retains the last confirmed preference, not a request.
    observeFullscreenChoice();
    cancelFullscreenChoice("application-close");
    applyUpdate(gate_request_application_close(*m_gate));
}

void FuramiBridge::decideClose(bool discard, const QString &revision)
{
    assertGuiThread();
    bool valid = false;
    const auto value = revision.toULongLong(&valid);
    if (valid) applyUpdate(gate_decide_close(*m_gate, discard, value));
}
void FuramiBridge::retrySave() { assertGuiThread(); applyUpdate(gate_retry_save(*m_gate)); }
void FuramiBridge::closeWithoutSave() { assertGuiThread(); applyUpdate(gate_close_without_save(*m_gate)); }
void FuramiBridge::requestSettingsReset() { assertGuiThread(); applyUpdate(gate_request_reset(*m_gate)); }
void FuramiBridge::decideSettingsReset(const QString &token, bool confirmed)
{
    assertGuiThread();
    bool valid = false;
    const auto value = token.toULongLong(&valid);
    if (valid && confirmed) observeFullscreenChoice();
    if (valid) applyUpdate(gate_decide_reset(*m_gate, value, confirmed));
}

void FuramiBridge::observeFullscreenChoice()
{
    assertGuiThread();
    if (!m_root || !m_fullscreenChoicePending || !m_fullscreenNativeTransitionObserved
        || !m_fullscreenChoiceSynchronized || m_quitAuthorized || m_closing)
        return;
    const auto visibility = m_root->visibility();
    if (visibility == QWindow::Minimized || visibility == QWindow::Hidden
        || (visibility == QWindow::FullScreen) != m_fullscreenRequested)
        return;
    m_fullscreenChoicePending = false;
    if (!m_fullscreenFocusRestorePending)
        m_fullscreenChoiceDeadline.stop();
    qInfo().noquote() << QStringLiteral("fullscreen_preference_readback effective=%1 root=%2 transition=%3")
        .arg(boolean(m_fullscreenObserved)).arg(m_rootXid).arg(m_fullscreenTransition);
    applyUpdate(gate_set_fullscreen(*m_gate, m_fullscreenObserved));
}

void FuramiBridge::synchronizeFullscreenChoice()
{
    assertGuiThread();
    if (!m_root || (!m_fullscreenChoicePending && !m_fullscreenFocusRestorePending)
        || !m_fullscreenNativeTransitionObserved)
        return;
    const auto transition = m_fullscreenTransition;
    auto *root = m_root.data();
    m_fullscreenSyncConnection = connect(root, &QQuickWindow::beforeSynchronizing, this,
        [this, root, transition] {
            // Qt blocks the GUI thread during this DirectConnection signal.
            // Only a frame synchronized after the matching native state event
            // may arm readback; a pre-WM/in-flight frame cannot acknowledge it.
            if (transition != m_fullscreenTransition
                || (!m_fullscreenChoicePending && !m_fullscreenFocusRestorePending)
                || !m_fullscreenNativeTransitionObserved
                || root->visibility() == QWindow::Hidden
                || root->visibility() == QWindow::Minimized
                || (root->visibility() == QWindow::FullScreen) != m_fullscreenRequested)
                return;
            // Preference readiness depends on native fullscreen state, not on
            // the WM reproducing a saved rectangle after a work-area change.
            const bool choiceReady = m_fullscreenChoicePending;
            const bool focusReady = m_fullscreenFocusRestorePending
                && root->visibility() == (m_windowedVisibility == QWindow::Maximized
                    ? QWindow::Maximized : QWindow::Windowed)
                && root->geometry() == m_windowedGeometry;
            if (!choiceReady && !focusReady)
                return;
            if (choiceReady)
                m_fullscreenChoiceSynchronized = true;
            QObject::disconnect(m_fullscreenSyncConnection);
            m_fullscreenSwapConnection = connect(root, &QQuickWindow::frameSwapped, this,
                [this, transition, focusReady] {
                    if (transition != m_fullscreenTransition)
                        return;
                    observeFullscreenChoice();
                    if (focusReady)
                        restoreFullscreenFocus();
                    if (m_fullscreenFocusRestorePending)
                        synchronizeFullscreenChoice();
                }, static_cast<Qt::ConnectionType>(Qt::QueuedConnection | Qt::SingleShotConnection));
        }, Qt::DirectConnection);
    root->update();
}

void FuramiBridge::cancelFullscreenChoice(const char *reason)
{
    assertGuiThread();
    if (m_fullscreenChoicePending || m_fullscreenFocusRestorePending) {
        qInfo().noquote() << QStringLiteral("fullscreen_transition_cancelled reason=%1 requested=%2 observed=%3 retained_preference=%4 root=%5 transition=%6 focus_restore_pending=%7")
            .arg(QString::fromLatin1(reason)).arg(boolean(m_fullscreenRequested))
            .arg(boolean(m_fullscreenObserved)).arg(boolean(m_fullscreenPreference))
            .arg(m_rootXid).arg(m_fullscreenTransition).arg(boolean(m_fullscreenFocusRestorePending));
    }
    m_fullscreenChoiceDeadline.stop();
    ++m_fullscreenTransition;
    QObject::disconnect(m_fullscreenSyncConnection);
    QObject::disconnect(m_fullscreenSwapConnection);
    m_fullscreenChoicePending = false;
    m_fullscreenChoiceSynchronized = false;
    m_fullscreenFocusRestorePending = false;
    m_fullscreenNativeTransitionObserved = false;
}

void FuramiBridge::togglePanel()
{
    assertGuiThread();
    setPanelVisible(!m_panelVisible);
}

void FuramiBridge::toggleFullscreen()
{
    assertGuiThread();
    if (!m_root || m_closing)
        return;
    // During a burst, toggle the latest intent, not a stale native event.
    // After cancellation, follow Qt visibility/the existing button label:
    // a subsequent explicit Leave cancels an unacknowledged optimistic Enter.
    const bool preserveWindowed = m_fullscreenChoicePending || m_fullscreenFocusRestorePending;
    const bool wasFullscreen = m_fullscreenChoicePending ? m_fullscreenRequested
        : m_root->visibility() == QWindow::FullScreen;
    cancelFullscreenChoice("superseded");
    m_fullscreenRequested = !wasFullscreen;
    m_fullscreenChoicePending = true;
    m_fullscreenChoiceDeadline.start();
    qInfo().noquote() << QStringLiteral("input_intent action=ToggleFullscreen root=%1 generation=%2 fullscreen=%3")
        .arg(m_rootXid).arg(m_generation).arg(boolean(!wasFullscreen));
    if (wasFullscreen) {
        m_fullscreenFocusRestorePending = true;
        m_fullscreenNativeTransitionObserved = false;
        if (m_windowedVisibility == QWindow::Maximized) {
            m_root->showMaximized();
        } else {
            m_root->showNormal();
            m_root->setGeometry(m_windowedGeometry);
        }
    } else {
        m_fullscreenFocusRestorePending = false;
        if (!preserveWindowed && (m_root->visibility() == QWindow::Windowed
            || m_root->visibility() == QWindow::Maximized)) {
            m_windowedGeometry = m_root->geometry();
            m_windowedVisibility = m_root->visibility();
            m_fullscreenFocusItem = m_root->activeFocusItem();
            m_fullscreenNativeFocus = m_host && QGuiApplication::focusWindow() == m_host.data();
        }
        m_root->showFullScreen();
    }
    m_root->update();
    scheduleGeometryLog();
}

void FuramiBridge::restoreFullscreenFocus()
{
    assertGuiThread();
    if (!m_fullscreenFocusRestorePending || !m_root
        || m_root->visibility() != (m_windowedVisibility == QWindow::Maximized
            ? QWindow::Maximized : QWindow::Windowed)
        || m_root->geometry() != m_windowedGeometry)
        return;
    // The exit's native transition and matching layout have synchronized and swapped.
    // Consume the restoration even when inactive: never steal focus later.
    m_fullscreenFocusRestorePending = false;
    if (!m_fullscreenChoicePending)
        m_fullscreenChoiceDeadline.stop();
    if (!m_root->isActive() || m_quitAuthorized)
        return;
    auto *target = m_fullscreenFocusItem.data();
    if (!target || target->window() != m_root.data() || !target->isVisible() || !target->isEnabled())
        target = m_container.data();
    if (target)
        target->forceActiveFocus(Qt::OtherFocusReason);
    // Only the current published host may regain native focus; no saved host/XID.
    if (m_fullscreenNativeFocus && m_root->isActive() && m_host && m_published
        && !m_poisoned && !m_host->poisoned() && m_host->parent() == m_root.data()
        && m_container && m_container->property("window").value<QWindow *>() == m_host.data())
        m_host->requestActivate();
    qInfo().noquote() << QStringLiteral("fullscreen_focus_restored root=%1 item=%2 native_previously_focused=%3 generation=%4")
        .arg(m_rootXid).arg(target ? target->objectName() : QStringLiteral("none"))
        .arg(boolean(m_fullscreenNativeFocus)).arg(m_generation);
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
    if (m_popupOpen)
        return false;
    if (key == Qt::Key_Space) {
        if (m_textEntryActive || event->modifiers() != Qt::NoModifier)
            return false;
        const bool hostOwnsFocus = m_host && !m_poisoned
            && QGuiApplication::focusWindow() == m_host.data();
        const bool containerOwnsFocus = m_root && m_root->isActive() && m_container
            && m_root->activeFocusItem() == m_container.data();
        // A stale host-origin event is not permission to bypass a focused control.
        if (qstrcmp(source, "host") == 0 ? !(hostOwnsFocus || containerOwnsFocus) : !containerOwnsFocus)
            return false;
    } else if (event->modifiers() != Qt::NoModifier) {
        return false;
    }
    if (key != Qt::Key_Space && key != Qt::Key_F11 && key != Qt::Key_Escape)
        return false;
    if (event->isAutoRepeat()) {
        event->accept();
        return true;
    }
    if (key == Qt::Key_Space) {
        togglePause();
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
        if (event->type() == QEvent::WindowStateChange && event->spontaneous()) {
            if (!m_fullscreenChoicePending && m_fullscreenFocusRestorePending)
                cancelFullscreenChoice("native-state-superseded");
            // Qt emits request-side visibility/state signals synchronously.
            // Its spontaneous state event instead follows the WM observation,
            // with windowStates()/visibility() already updated by Qt.
            ++m_fullscreenTransition;
            QObject::disconnect(m_fullscreenSyncConnection);
            QObject::disconnect(m_fullscreenSwapConnection);
            m_fullscreenChoiceSynchronized = false;
            m_fullscreenNativeTransitionObserved = false;
            const auto visibility = m_root->visibility();
            if (visibility == QWindow::Hidden || visibility == QWindow::Minimized) {
                cancelFullscreenChoice("not-visible");
            } else if (!m_quitAuthorized && !m_closing) {
                m_fullscreenObserved = visibility == QWindow::FullScreen;
                if (!m_fullscreenChoicePending && m_fullscreenObserved != m_fullscreenPreference) {
                    // A late acknowledgement or a WM-initiated choice remains
                    // an actual choice after an earlier request was canceled.
                    m_fullscreenRequested = m_fullscreenObserved;
                    m_fullscreenChoicePending = true;
                    m_fullscreenChoiceDeadline.start();
                }
                if (m_fullscreenChoicePending) {
                    m_fullscreenNativeTransitionObserved = m_fullscreenObserved == m_fullscreenRequested;
                    qInfo().noquote() << QStringLiteral("fullscreen_native_state_observed requested=%1 effective=%2 matched=%3 root=%4 transition=%5")
                        .arg(boolean(m_fullscreenRequested)).arg(boolean(m_fullscreenObserved))
                        .arg(boolean(m_fullscreenNativeTransitionObserved)).arg(m_rootXid).arg(m_fullscreenTransition);
                    if (m_fullscreenNativeTransitionObserved)
                        synchronizeFullscreenChoice();
                }
            }
        }
        if (event->type() == QEvent::Resize || event->type() == QEvent::WindowStateChange) {
            if ((m_fullscreenChoicePending || m_fullscreenFocusRestorePending)
                && m_fullscreenNativeTransitionObserved)
                m_root->update();
            scheduleGeometryLog();
        }
    }
    return QObject::eventFilter(watched, event);
}

void FuramiBridge::applyUpdate(UiUpdate update)
{
    assertGuiThread();
    if (!update.changed)
        return;
    if (update.restart_generation != m_restartGeneration && !m_bootstrapFailed)
        m_nativeDiagnostic.clear();
    m_actualPhase = phaseName(update.phase);
    m_generation = update.generation;
    m_restartGeneration = update.restart_generation;
    m_canRestart = update.can_restart && !m_bootstrapFailed;
    m_canOpen = update.can_open && !m_bootstrapFailed;
    if (m_closeOnNextOpening && update.create_native) {
        m_closeDuringOpening = update.generation;
        m_closeOnNextOpening = false;
    } else if (m_closeOnNextOpening && update.can_open) {
        m_closeOnNextOpening = false;
    }
    m_audioStatus = fromRust(update.audio_status);
    m_audioDiagnostic = fromRust(update.audio_diagnostic);
    m_audioSource = fromRust(update.audio_source);
    m_audioDesired = fromRust(update.audio_desired);
    m_productPhase = fromRust(update.product_phase);
    m_recoveryEvidence = fromRust(update.recovery_evidence);
    m_recoveryStage = fromRust(update.recovery_stage);
    m_recoveryCandidates = fromRust(update.candidates);
    m_failed = update.failed || m_bootstrapFailed;
    m_paused = update.paused;
    if (update.create_native && !m_nativeGeneration && !m_host) {
        // Creation is already correlated with the new physical attempt.
        // Set this before QML receives its hostWindow, not after readiness.
        m_presentationSuppressed = update.prepared_paused;
    } else if (update.generation == m_nativeGeneration && update.prepared_paused) {
        m_presentationSuppressed = true;
    }
    if (m_host && (m_presentationSuppressed || m_productPhase == QStringLiteral("Disconnected")))
        m_host->hide();
    m_volumePercent = update.volume_percent;
    m_muted = update.muted;
    m_canTogglePause = update.can_toggle_pause && !m_bootstrapFailed;
    m_canSetGain = update.can_set_gain && !m_bootstrapFailed;
    m_playbackStatus = fromRust(update.playback_status);
    m_settingsStatus = fromRust(update.settings_status);
    m_settingsPath = fromRust(update.settings_path);
    m_settingsRefused = update.settings_refused;
    m_savedSelection = fromRust(update.saved_selection);
    m_startupReason = fromRust(update.startup_reason);
    m_draftDirty = update.draft_dirty;
    m_closeDialog = fromRust(update.close_dialog);
    m_closeRevision = update.close_revision;
    m_resetToken = update.reset_token;
    m_fullscreenPreference = update.fullscreen;
    m_closing = update.closing;
    m_outputRows = fromRust(update.output_rows);
    m_outputCatalogRevision = update.output_catalog_revision;
    m_outputSelected = fromRust(update.output_selected);
    m_outputSelectedKey = fromRust(update.output_selected_key);
    m_outputEffective = fromRust(update.output_effective);
    m_outputStatus = fromRust(update.output_status);
    m_outputNeedsAction = update.output_needs_action;
    if (m_closing)
        cancelFullscreenChoice("application-close");
    m_diagnostic = fromRust(update.diagnostic);
    if (m_failed && !m_nativeDiagnostic.isEmpty()) {
        m_diagnostic = m_diagnostic.isEmpty() ? m_nativeDiagnostic
            : m_nativeDiagnostic + QStringLiteral("; ") + m_diagnostic;
    }
    qInfo().noquote() << QStringLiteral("ui_phase phase=%1 visible_phase=%2 product_phase=%3 generation=%4 failed=%5 paused=%6 restart_generation=%7 can_restart=%8 audio_status=%9 audio_source=%10 audio_desired=%11 audio_diagnostic=%12 recovery_evidence=%13 recovery_stage=%14 diagnostic=%15 playback_status=%16 volume=%17 muted=%18 can_toggle_pause=%19 can_set_gain=%20")
        .arg(m_actualPhase, phase(), m_productPhase).arg(m_generation).arg(boolean(m_failed))
        .arg(boolean(m_paused))
        .arg(m_restartGeneration).arg(boolean(m_canRestart)).arg(m_audioStatus)
        .arg(m_audioSource, m_audioDesired, m_audioDiagnostic)
        .arg(m_recoveryEvidence, m_recoveryStage, m_diagnostic)
        .arg(m_playbackStatus).arg(m_volumePercent).arg(boolean(m_muted))
        .arg(boolean(m_canTogglePause)).arg(boolean(m_canSetGain));
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
    if (m_presentationSuppressed) {
        // A hidden WindowContainer still owns parent/geometry, but does not
        // create the QWindow by showing it. Polish that same binding and
        // explicitly create its UNMAPPED native surface for the owner.
        // Never clear/rebind the container or recreate a published surface.
        m_container->ensurePolished();
        if (m_container->isVisible() || host->isVisible() || host->parent() != m_root.data()) {
            failNative(generation, QStringLiteral("prepared-paused host must remain hidden beneath its permanent root"));
            return;
        }
        host->create();
    }
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
    qInfo().noquote() << QStringLiteral("native_loss_barrier_begin generation=%1 host=%2")
        .arg(generation).arg(previousXid);
    const QString diagnostic = fromRust(gate_wait_for_owner_ack(*m_gate, generation));
    if (!diagnostic.isEmpty()) {
        const QByteArray failure = diagnostic.toUtf8();
        qFatal("native_loss_barrier_failed generation=%llu host=%llu diagnostic=%s",
            static_cast<unsigned long long>(generation),
            static_cast<unsigned long long>(previousXid), failure.constData());
    }
    qInfo().noquote() << QStringLiteral("native_loss_barrier_complete generation=%1 host=%2")
        .arg(generation).arg(previousXid);
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
        if (surviving->handle()) {
            surviving->hide();
            surviving->destroy();
        }
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
    const char *names[] = {"openProof", "restartCapture", "closeProof", "closeDuringOpenProof", "forceSurfaceLossProof",
                          "togglePlayback", "playbackVolume", "playbackMute",
                          "proofText", "togglePanelProof", "fullscreenProof", "popupOpenProof",
                          "closePopupProof", "videoContainer", "proofPanel", "savedSelection",
                          "startupRestoreReason", "settingsStatus", "resetSettings",
                          "cancelDirtyClose", "discardDirtyClose", "retrySettingsSave",
                          "closeWithoutSaving", "cancelSettingsReset", "confirmSettingsReset"};
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

LaunchResult run_qt_application(rust::Box<RuntimeCoordinator> gate, rust::Str display, bool qualificationStdin)
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

    if (Display *probe = XOpenDisplay(captured.constData()); probe == nullptr)
        return launchFailure(QStringLiteral("DISPLAY '%1' is unreachable: install/enable XWayland in your Wayland session, or point DISPLAY at a reachable X11 display").arg(QString::fromUtf8(captured)));
    else
        XCloseDisplay(probe);

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
    FuramiBridge bridge(std::move(gate));
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
    if (qualificationStdin && !bridge.enableQualificationInput(rootDiagnostic))
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
