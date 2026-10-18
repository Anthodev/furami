#include "host.h"
#include "furami/src/ui/bridge.cxx.h"

#include <QCoreApplication>
#include <QDebug>
#include <QProcess>
#include <QProcessEnvironment>
#include <QStandardPaths>
#include <QUrl>
#include <QDir>
#include <QFileInfo>
#include <QJsonObject>
#include <QJsonArray>
#include <QPluginLoader>
#include <QLibrary>
#include <QGuiApplication>
#include <QIcon>
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
#include <cmath>
#include <initializer_list>
#include <type_traits>
#include <charconv>
#include <cstring>
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

// Never inspect the message/context or retain a Qt object. Rust's process-global
// destination remains valid through Qt cleanup and the final worker drain.
void safeQtMessage(QtMsgType severity, const QMessageLogContext &, const QString &)
{
    diagnostics_qt_message(static_cast<std::int32_t>(severity));
}

struct NativeField {
    enum class Kind { Boolean, Signed, Unsigned, Real, Token };
    const char *name;
    Kind kind;
    union {
        bool boolean;
        std::int64_t signedValue;
        std::uint64_t unsignedValue;
        double real;
        const char *token;
    };
    NativeField(const char *key, bool value) : name(key), kind(Kind::Boolean), boolean(value) {}
    template<typename T, std::enable_if_t<std::is_integral_v<T> && !std::is_same_v<T, bool>, int> = 0>
    NativeField(const char *key, T value) : name(key),
        kind(std::is_signed_v<T> ? Kind::Signed : Kind::Unsigned)
    {
        if constexpr (std::is_signed_v<T>)
            signedValue = value;
        else
            unsignedValue = value;
    }
    NativeField(const char *key, double value) : name(key), kind(Kind::Real), real(value) {}
    // Only finite constants/control classes reach this constructor.
    NativeField(const char *key, const char *value) : name(key), kind(Kind::Token), token(value) {}
};

void emitNativeDiagnostic(const char *code, std::initializer_list<NativeField> fields = {}, int severity = 4)
{
    // Stack-only, bounded JSON encoding. Integer IDs never pass through a
    // floating-point Qt JSON value; to_chars preserves every u64 including MAX.
    char payload[2048];
    char *cursor = payload;
    char *const end = payload + sizeof(payload);
    bool valid = true;
    const auto append = [&](const char *text) {
        const auto length = std::strlen(text);
        if (length > static_cast<std::size_t>(end - cursor)) {
            valid = false;
            return;
        }
        std::memcpy(cursor, text, length);
        cursor += length;
    };
    const auto number = [&](auto value) {
        const auto result = std::to_chars(cursor, end, value);
        if (result.ec == std::errc())
            cursor = result.ptr;
        else
            valid = false;
    };
    append("{");
    bool first = true;
    for (const auto &field : fields) {
        if (!valid)
            break;
        if (!first)
            append(",");
        first = false;
        append("\"");
        append(field.name);
        append("\":");
        switch (field.kind) {
        case NativeField::Kind::Boolean: append(field.boolean ? "true" : "false"); break;
        case NativeField::Kind::Signed: number(field.signedValue); break;
        case NativeField::Kind::Unsigned: number(field.unsignedValue); break;
        case NativeField::Kind::Real:
            if (std::isfinite(field.real))
                number(field.real);
            else
                append("null");
            break;
        case NativeField::Kind::Token:
            append("\""); append(field.token); append("\"");
            break;
        }
    }
    append("}");
    // An impossible-for-current-schemas overflow still reaches the existing
    // rejection boundary, never a truncated JSON record or alternate channel.
    diagnostics_native_event(rust::Str(code), severity,
        valid ? rust::Str(payload, static_cast<std::size_t>(cursor - payload)) : rust::Str(""));
    // Keep Qt's existing fatal-warning/critical environment semantics. The
    // installed handler sees only severity; these fixed markers carry no data.
    if (severity == 1)
        qWarning("native_warning");
    else if (severity == 2)
        qCritical("native_critical");
}

const char *controlClass(const QString &name)
{
    // Never serialize dynamic model keys. Geometry uses a traversal ordinal.
    static const char *classes[] = {
        "sourceRow_", "outputRow_", "sizeRow_", "rateRow_",
        "captureFormatSelector_row_", "audioInputSelector_row_", "audioOutputSelector_row_",
        "renderChoice_scale_row_", "renderChoice_cscale_row_", "renderChoice_dscale_row_",
        "renderChoice_scale_antiring_row_", "renderChoice_dither_row_", "renderChoice_deband_row_",
        "renderChoice_deband_iterations_row_", "renderChoice_deband_grain_row_", "renderChoice_interpolation_row_"
    };
    for (const auto *value : classes)
        if (name.startsWith(QLatin1String(value)))
            return value;
    static const char *controls[] = {
        "menuOpenLogs", "contextDiagnosticsError",
        "furamiRoot", "videoHost", "topChromeWindow", "volumeChromeWindow", "bottomChromeWindow",
        "videoContainer", "topChromeRoot", "volumeChromeRoot", "bottomChromeRoot",
        "ribbonSource", "ribbonOutput", "ribbonResolution", "ribbonRate", "ribbonFullscreen",
        "volumeSlider", "volumeMute", "optionsTrigger",
        "menuOpenCapture", "menuPauseResume", "menuRestart", "menuReconnect", "menuSettings", "menuCloseCapture",
        "menuBack", "menuBackSection", "menuIntentError", "settingsCaptureEntry", "settingsAudioEntry",
        "settingsRendererEntry", "settingsPreferencesEntry",
        "contextIntentError", "contextDiagnostic", "contextOutputAction", "contextRecoveryStage", "recoveryUse",
        "sourceRefresh", "captureFormatSelector", "captureRefresh", "audioCaptureEnabled", "audioInputSelector",
        "audioMute", "audioOutputSelector", "audioOutputMute",
        "sizeWidthField", "sizeHeightField", "rateExactField", "sizeRateCatalogError",
        "rendererNotice", "preferencesStatus", "preferencesStartup",
        "preferencesPath", "preferencesImmediate", "preferencesReset", "preferencesRefused",
        "sourceCatalogError", "sourceCapability", "outputMissing", "outputStatus",
        "saveFailureRetry", "saveFailureClose", "resetCancel", "resetConfirm",
        "backdropTitle", "backdropDetail", "backdropDiagnostic",
        "renderChoice_scale", "renderChoice_cscale", "renderChoice_dscale", "renderChoice_scale_antiring",
        "renderChoice_dither", "renderChoice_deband", "renderChoice_deband_iterations", "renderChoice_deband_grain", "renderChoice_interpolation",
        "renderToggle_scale", "renderToggle_cscale", "renderToggle_dscale", "renderToggle_scale_antiring",
        "renderToggle_dither", "renderToggle_deband", "renderToggle_deband_iterations", "renderToggle_deband_grain", "renderToggle_interpolation",
        "renderValue_scale", "renderValue_cscale", "renderValue_dscale", "renderValue_scale_antiring",
        "renderValue_dither", "renderValue_deband", "renderValue_deband_iterations", "renderValue_deband_grain", "renderValue_interpolation"
    };
    for (const auto *value : controls)
        if (name == QLatin1String(value))
            return value;
    return "UnknownControl";
}

LaunchResult launchFailure(const QString &diagnostic)
{
    emitNativeDiagnostic("native_startup_failure", {}, 2);
    const auto bytes = diagnostic.toUtf8();
    return LaunchResult{1, rust::String(bytes.constData(), static_cast<std::size_t>(bytes.size())),
                        NativeFailureKind::Generic, rust::String()};
}

LaunchResult pluginFailure(NativeFailureKind kind, const QString &pluginClass,
                           QString detail, const QString &mediaPrefix)
{
    const auto canonicalPrefix = QFileInfo(mediaPrefix).canonicalFilePath();
    if (!canonicalPrefix.isEmpty())
        detail.replace(canonicalPrefix, QStringLiteral("<media-prefix>"));
    if (!mediaPrefix.isEmpty())
        detail.replace(mediaPrefix, QStringLiteral("<media-prefix>"));
    QString safe;
    for (const QChar character : detail) {
        if (!character.isPrint() || character == QChar::LineSeparator || character == QChar::ParagraphSeparator)
            safe += QLatin1Char(' ');
        else
            safe += character;
    }
    safe = (pluginClass + QStringLiteral(": ") + safe).left(512);
    const auto bytes = safe.toUtf8();
    return LaunchResult{1, rust::String("required Qt plugin refused"), kind,
                        rust::String(bytes.constData(), static_cast<std::size_t>(bytes.size()))};
}

LaunchResult preflightRequiredQtPlugins(const QString &mediaPrefix)
{
    const auto prefix = QFileInfo(mediaPrefix).canonicalFilePath();
    const QString bundle = mediaPrefix + QStringLiteral("/lib/qt6/plugins");
    const QString dev = mediaPrefix + QStringLiteral("/plugins");
    // A bundle root, when present, is authoritative even when incomplete.
    // Never repair its missing plugin with a dev or system plugin.
    const QString root = QFileInfo::exists(bundle) ? bundle : dev;
    struct Required { const char *path; const char *iid; const char *name; const char *key; };
    static constexpr Required required[] = {
        {"platforms/libqxcb.so", "org.qt-project.Qt.QPA.QPlatformIntegrationFactoryInterface.5.3", "platform", "xcb"},
        {"xcbglintegrations/libqxcb-egl-integration.so", "org.qt-project.Qt.QPA.Xcb.QXcbGlIntegrationFactoryInterface.5.5", "xcb-egl", "xcb_egl"},
        {"xcbglintegrations/libqxcb-glx-integration.so", "org.qt-project.Qt.QPA.Xcb.QXcbGlIntegrationFactoryInterface.5.5", "xcb-glx", "xcb_glx"},
        {"imageformats/libqsvg.so", "org.qt-project.Qt.QImageIOHandlerFactoryInterface", "svg-image", "svg"},
        {"iconengines/libqsvgicon.so", "org.qt-project.Qt.QIconEngineFactoryInterface", "svg-icon", "svg"}
    };
    for (const auto &plugin : required) {
        const QString name = QString::fromLatin1(plugin.name);
        const QFileInfo file(root + QLatin1Char('/') + QString::fromLatin1(plugin.path));
        if (!file.exists() || !file.isReadable() || !file.isFile())
            return pluginFailure(NativeFailureKind::QtPluginMissing, name, QStringLiteral("missing or unreadable required plugin"), mediaPrefix);
        const QString canonical = file.canonicalFilePath();
        if (prefix.isEmpty() || !canonical.startsWith(prefix + QLatin1Char('/'))
            || !canonical.startsWith(QFileInfo(root).canonicalFilePath() + QLatin1Char('/')))
            return pluginFailure(NativeFailureKind::QtPluginInvalid, name, QStringLiteral("plugin resolves outside media prefix"), mediaPrefix);
        QPluginLoader loader(canonical);
        loader.setLoadHints(QLibrary::ResolveAllSymbolsHint | QLibrary::PreventUnloadHint);
        const auto metadata = loader.metaData();
        if (metadata.isEmpty() || metadata.value(QStringLiteral("IID")).toString() != QString::fromLatin1(plugin.iid)
            || !metadata.value(QStringLiteral("MetaData")).toObject().value(QStringLiteral("Keys")).toArray().contains(QString::fromLatin1(plugin.key)))
            return pluginFailure(NativeFailureKind::QtPluginInvalid, name, QStringLiteral("invalid plugin metadata or interface"), mediaPrefix);
        if (!loader.load())
            return pluginFailure(NativeFailureKind::QtPluginLoad, name, loader.errorString(), mediaPrefix);
    }
    return LaunchResult{0, rust::String(), NativeFailureKind::NoFailure, rust::String()};
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
        if (m_bridge && event->type() == QEvent::Wheel
            && m_bridge->handleVolumeWheel(static_cast<QWheelEvent *>(event), this))
            return true;
        if (m_bridge && !m_poisoned
            && (event->type() == QEvent::Wheel || event->type() == QEvent::TouchBegin
                || event->type() == QEvent::TouchUpdate)) {
            const bool pressed = event->type() == QEvent::TouchBegin;
            m_bridge->noteNativePointerActivity(pressed);
            if (pressed && m_bridge->popupOpen()) {
                event->accept();
                return true;
            }
        }
        if (event->type() == QEvent::FocusIn || event->type() == QEvent::FocusOut) {
            emitNativeDiagnostic("input_focus_window", {{"generation", m_generation}, {"active", event->type() == QEvent::FocusIn}});
        }
        if (event->type() == QEvent::PlatformSurface) {
            auto *surface = static_cast<QPlatformSurfaceEvent *>(event);
            if (surface->surfaceEventType() == QPlatformSurfaceEvent::SurfaceCreated) {
                emitNativeDiagnostic("native_surface_created", {{"generation", m_generation}, {"poisoned", m_poisoned}});
                if (!m_poisoned && !m_authorized && m_bridge)
                    m_bridge->schedulePublication(m_generation);
                else
                    emitNativeDiagnostic("native_surface_rejected", {{"generation", m_generation}}, 1);
            } else {
                emitNativeDiagnostic("native_surface_about_to_destroy", {{"generation", m_generation}, {"host", m_previousXid}, {"authorized", m_authorized}});
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

    void mouseMoveEvent(QMouseEvent *event) override
    {
        if (!m_poisoned && m_bridge) {
            m_bridge->noteNativePointerActivity(false);
            event->accept();
        } else {
            event->ignore();
        }
    }

    void mousePressEvent(QMouseEvent *event) override
    {
        emitNativeDiagnostic("input_mouse_press", {{"generation", m_generation}, {"button", static_cast<int>(event->button())}, {"x", event->position().x()}, {"y", event->position().y()}});
        if (!m_poisoned && m_bridge) {
            m_bridge->noteNativePointerActivity(true);
            if (m_bridge->popupOpen()) {
                // A mandatory confirmation retained by QML owns this press.
                // Its dismiss hook has already restored the bottom tool focus.
                event->accept();
                return;
            }
        }
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

FuramiBridge::FuramiBridge(rust::Box<RuntimeCoordinator> gate, UiUpdate nativeEnvironment, bool qualificationVideo)
    : m_gate(std::move(gate)), m_qualificationVideo(qualificationVideo)
{
    assertGuiThread();
    QQmlEngine::setObjectOwnership(this, QQmlEngine::CppOwnership);
    applyUpdate(std::move(nativeEnvironment));
    // X11 may never acknowledge a WM state request. This deadline abandons
    // intent/focus hooks only: it proves neither rejection nor success.
    m_fullscreenChoiceDeadline.setSingleShot(true);
    m_fullscreenChoiceDeadline.setInterval(5000);
    connect(&m_fullscreenChoiceDeadline, &QTimer::timeout, this, [this] {
        if (m_fullscreenChoicePending || m_fullscreenFocusRestorePending)
            cancelFullscreenChoice("deadline");
    });
    m_chromeIdleTimer.setSingleShot(true);
    m_chromeIdleTimer.setTimerType(Qt::PreciseTimer);
    m_chromeIdleTimer.setInterval(5000);
    connect(&m_chromeIdleTimer, &QTimer::timeout, this, &FuramiBridge::chromeInactivity);
    connect(qGuiApp, &QGuiApplication::focusWindowChanged, this, &FuramiBridge::refreshChromeFamilyActive);
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
    m_chromeIdleTimer.stop();
    if (m_qualificationInput) {
        m_qualificationInput->setEnabled(false);
        delete m_qualificationInput.data();
    }
    if (m_stdinFlags != -1 && ::fcntl(STDIN_FILENO, F_SETFL, m_stdinFlags) == -1)
        emitNativeDiagnostic("qualification_stdin_restore_failed", {{"errno", errno}}, 1);
    Q_ASSERT(m_nativeGeneration == 0);
}

void FuramiBridge::assertGuiThread() const
{
    Q_ASSERT(qGuiApp && QThread::currentThread() == qGuiApp->thread());
    Q_ASSERT(QThread::currentThread() == thread());
}

// The folder is opened through the system xdg-open, which must not inherit the
// bundled AppRun's Qt search-path overrides: they would aim a system process at
// Furami's private Qt build. Exactly those five variables are dropped; every
// other inherited value (PATH, LD_PRELOAD, FURAMI_MEDIA_PREFIX, display/bus/
// session, XDG/HOME) reaches xdg-open unchanged.
void FuramiBridge::launchLogFolderOpener(const QString &path, std::uint64_t request)
{
    assertGuiThread();
    const QPointer<FuramiBridge> bridge(this);
    const auto acknowledge = [bridge, request](bool accepted) {
        if (!bridge)
            return;
        emitNativeDiagnostic("log_folder_dispatched", {{"request_id", request}, {"accepted", accepted}});
        // Startup and terminal results use the same receiver and normal-priority
        // queued metacalls. started posts first, so its ACK precedes finished.
        QMetaObject::invokeMethod(bridge.data(), [bridge, request, accepted] {
            if (bridge)
                bridge->applyUpdate(gate_log_folder_result(*bridge->m_gate, request, accepted));
        }, Qt::QueuedConnection);
    };
    const QString program = QStandardPaths::findExecutable(QStringLiteral("xdg-open"));
    if (program.isEmpty()) {
        acknowledge(false);
        return;
    }
    static const char *const appRunOverrides[] = {
        "LD_LIBRARY_PATH",
        "QT_PLUGIN_PATH",
        "QT_QPA_PLATFORM_PLUGIN_PATH",
        "QML_IMPORT_PATH",
        "QML2_IMPORT_PATH",
    };
    QProcessEnvironment environment = QProcessEnvironment::systemEnvironment();
    for (const char *name : appRunOverrides)
        environment.remove(QString::fromLatin1(name));
    // No parent/member owns this process: destroying it while Starting/Running
    // kills and waits. Only terminal signals retire it. If Furami exits first,
    // OS cleanup reclaims the watcher without cancelling the independent child.
    auto *process = new QProcess;
    process->setProgram(program);
    process->setArguments({QUrl::fromLocalFile(path).toString(QUrl::FullyEncoded)});
    process->setProcessEnvironment(environment);
    // Null streams avoid unbounded buffering and keep launcher output private.
    process->setStandardInputFile(QProcess::nullDevice());
    process->setStandardOutputFile(QProcess::nullDevice());
    process->setStandardErrorFile(QProcess::nullDevice());
#ifdef Q_OS_UNIX
    process->setUnixProcessParameters(QProcess::UnixProcessFlag::CreateNewSession
        | QProcess::UnixProcessFlag::DisconnectControllingTerminal);
#endif
    connect(process, &QProcess::started, process, [acknowledge] {
        acknowledge(true);
    });
    connect(process, &QProcess::errorOccurred, process, [process, acknowledge](QProcess::ProcessError error) {
        if (error == QProcess::FailedToStart) {
            acknowledge(false);
            process->deleteLater();
        }
        // Crashed is followed by finished(CrashExit); handle that outcome once.
    });
    connect(process, &QProcess::finished, process, [process, bridge, request](int exitCode, QProcess::ExitStatus exitStatus) {
        const bool failed = exitStatus == QProcess::CrashExit || exitCode != 0;
        if (bridge) {
            if (exitStatus == QProcess::NormalExit)
                emitNativeDiagnostic("log_folder_finished", {{"request_id", request}, {"failed", failed}, {"exit_code", exitCode}}, failed ? 1 : 4);
            else
                emitNativeDiagnostic("log_folder_finished", {{"request_id", request}, {"failed", failed}}, 1);
            // Never enter Rust/applyUpdate from a native signal or an unfinished
            // outer update. Destruction of the bridge cancels queued delivery.
            QMetaObject::invokeMethod(bridge.data(), [bridge, request, failed] {
                if (bridge)
                    bridge->applyUpdate(gate_log_folder_finished(*bridge->m_gate, request, failed));
            }, Qt::QueuedConnection);
        }
        process->deleteLater();
    });
    process->start();
}

QWindow *FuramiBridge::hostWindow() const { assertGuiThread(); return m_host.data(); }
QString FuramiBridge::phase() const { assertGuiThread(); return m_failed ? QStringLiteral("Failed") : m_actualPhase; }
QString FuramiBridge::uiSnapshot() const { assertGuiThread(); return m_uiSnapshot; }
QString FuramiBridge::diagnostic() const { assertGuiThread(); return m_diagnostic; }
QString FuramiBridge::contextDiagnostic() const { assertGuiThread(); return m_contextDiagnostic; }
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
bool FuramiBridge::canOpen() const { assertGuiThread(); return m_canOpen; }
bool FuramiBridge::canRestart() const { assertGuiThread(); return m_canRestart; }
QString FuramiBridge::restartGeneration() const { assertGuiThread(); return QString::number(m_restartGeneration); }
QString FuramiBridge::audioStatus() const { assertGuiThread(); return m_audioStatus; }
QString FuramiBridge::audioDiagnostic() const { assertGuiThread(); return m_audioDiagnostic; }
QString FuramiBridge::audioSource() const { assertGuiThread(); return m_audioSource; }
bool FuramiBridge::textEntryActive() const { assertGuiThread(); return m_textEntryActive; }
bool FuramiBridge::popupOpen() const { assertGuiThread(); return m_popupOpen; }
bool FuramiBridge::chromeFamilyActive() const { assertGuiThread(); return m_chromeFamilyActive; }
bool FuramiBridge::quitAuthorized() const { assertGuiThread(); return m_quitAuthorized; }
// Launch (not UI) failure: only unauthorized root destruction or a poisoned native lifetime is fatal to startup.
// A recoverable session failure sets m_failed for the UI but still allows the authorized loop exit to succeed.
bool FuramiBridge::launchFailed() const { assertGuiThread(); return m_bootstrapFailed || m_poisoned || m_qualificationFailure; }
QString FuramiBridge::settingsStatus() const { assertGuiThread(); return m_settingsStatus; }
QString FuramiBridge::settingsPath() const { assertGuiThread(); return m_settingsPath; }
bool FuramiBridge::settingsRefused() const { assertGuiThread(); return m_settingsRefused; }
QString FuramiBridge::savedSelection() const { assertGuiThread(); return m_savedSelection; }
QString FuramiBridge::startupReason() const { assertGuiThread(); return m_startupReason; }
QString FuramiBridge::closeDialog() const { assertGuiThread(); return m_closeDialog; }
QString FuramiBridge::resetToken() const { assertGuiThread(); return QString::number(m_resetToken); }
bool FuramiBridge::fullscreenPreference() const { assertGuiThread(); return m_fullscreenPreference; }
bool FuramiBridge::statsHiddenPreference() const { assertGuiThread(); return m_statsHiddenPreference; }
bool FuramiBridge::loggingEnabledPreference() const { assertGuiThread(); return m_loggingEnabledPreference; }
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
            this, [this](const QRect &) { emit screenGeometryChanged(); scheduleChromeGeometry(); });
    }
    emit screenGeometryChanged();
    scheduleChromeGeometry();
}

bool FuramiBridge::isChromeFamilyWindow(const QWindow *window) const
{
    // Qt's ownership chain follows native parents, then the transient parent
    // of a top-level popup. Only our registered objects establish membership.
    for (const auto *ancestor = window; ancestor; ancestor = ancestor->parent(QWindow::IncludeTransients)) {
        if (ancestor == m_root.data() || ancestor == m_host.data()
            || ancestor == m_topChromeWindow.data() || ancestor == m_volumeChromeWindow.data()
            || ancestor == m_bottomChromeWindow.data())
            return true;
    }
    return false;
}

void FuramiBridge::refreshChromeFamilyActive(QWindow *focused)
{
    assertGuiThread();
    // focusWindowChanged precedes QWindow::activeChanged: use its new pointer,
    // never the root/Tool active properties from the previous focus owner.
    const bool active = isChromeFamilyWindow(focused);
    if (m_chromeFamilyActive == active)
        return;
    m_chromeFamilyActive = active;
    if (!active) {
        m_volumeWheelRemainder = 0;
        m_chromeIdleTimer.stop();
    }
    emit chromeFamilyActiveChanged();
    if (!m_chromeFamilyActive) {
        // QML's outer gate passively invalidates popup epochs. Do not use the
        // actionable dismiss hook here: it may restore an invoker's focus.
        for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
            if (window)
                window->hide();
        }
    }
}

void FuramiBridge::setChromeIdleArmed(bool armed)
{
    assertGuiThread();
    if (armed && m_chromeFamilyActive && m_root && m_chromeRegistered && !m_bootstrapFailed && !m_closing
        && !m_quitAuthorized && m_root->isVisible() && m_root->visibility() != QWindow::Minimized) {
        m_chromeIdleTimer.start();
    } else {
        m_chromeIdleTimer.stop();
    }
}

void FuramiBridge::dismissChromePopup()
{
    assertGuiThread();
    if (m_chromeFamilyActive && m_root && m_root->isVisible()
        && m_root->visibility() != QWindow::Minimized && m_chromeRegistered && m_popupOpen
        && !QMetaObject::invokeMethod(m_root, "dismissChromePopup", Qt::DirectConnection))
        failChrome(QStringLiteral("QML root dismissChromePopup hook disappeared"));
}

void FuramiBridge::toggleChromeSettings()
{
    assertGuiThread();
    if (m_chromeRegistered && m_chromeFamilyActive && m_root && m_root->isVisible()
        && m_root->visibility() != QWindow::Minimized && !m_closing && !m_quitAuthorized
        && !m_bootstrapFailed
        && !QMetaObject::invokeMethod(m_root, "toggleChromeSettings", Qt::DirectConnection))
        failChrome(QStringLiteral("QML root toggleChromeSettings hook disappeared"));
}

void FuramiBridge::hideChromeWindows()
{
    assertGuiThread();
    m_chromeIdleTimer.stop();
    dismissChromePopup();
    for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
        if (window)
            window->hide();
    }
}

void FuramiBridge::noteNativePointerActivity(bool pressed)
{
    assertGuiThread();
    if (!m_chromeFamilyActive || m_bootstrapFailed || m_quitAuthorized)
        return;
    emit pointerActivity();
    if (pressed)
        dismissChromePopup();
}

void FuramiBridge::failChrome(const QString &diagnostic)
{
    assertGuiThread();
    if (m_bootstrapFailed)
        return;
    m_bootstrapFailed = true;
    m_failed = true;
    m_nativeDiagnostic = diagnostic;
    m_diagnostic = diagnostic;
    m_contextDiagnostic = diagnostic;
    m_chromeIdleTimer.stop();
    for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
        if (window)
            window->hide();
    }
    emitNativeDiagnostic("native_chrome_failure", {}, 2);
    emit stateChanged();
    // A failed UI association is fatal to bootstrap, not permission to tear
    // down the permanent video parent ahead of the existing owner barrier.
    applyUpdate(gate_quit(*m_gate));
}

void FuramiBridge::registerChromeWindows(QWindow *top, QWindow *volume, QWindow *bottom)
{
    assertGuiThread();
    QQuickWindow *windows[] = {qobject_cast<QQuickWindow *>(top), qobject_cast<QQuickWindow *>(volume),
                              qobject_cast<QQuickWindow *>(bottom)};
    const char *names[] = {"topChromeWindow", "volumeChromeWindow", "bottomChromeWindow"};
    const char *roots[] = {"topChromeRoot", "volumeChromeRoot", "bottomChromeRoot"};
    QString diagnostic;
    if (m_chromeRegistered || !m_root || m_quitAuthorized || m_bootstrapFailed) {
        diagnostic = QStringLiteral("chrome registration requires one live permanent root and an empty fixed registry");
    } else if (m_root->metaObject()->indexOfMethod("syncChromeGeometry()") < 0
        || m_root->metaObject()->indexOfMethod("dismissChromePopup()") < 0
        || m_root->metaObject()->indexOfMethod("toggleChromeSettings()") < 0) {
        diagnostic = QStringLiteral("QML root chrome geometry/dismiss/settings hooks are missing");
    } else if (top == volume || top == bottom || volume == bottom) {
        diagnostic = QStringLiteral("chrome registration requires three distinct QQuickWindows");
    } else {
        for (int i = 0; i < 3; ++i) {
            auto *window = windows[i];
            auto *content = window ? window->findChild<QQuickItem *>(QLatin1String(roots[i])) : nullptr;
            if (!window || window->objectName() != QLatin1String(names[i])
                || window->parent() || window->transientParent() != m_root.data()
                || (window->flags() & Qt::WindowType_Mask) != Qt::Tool
                || !window->flags().testFlag(Qt::FramelessWindowHint)
                || !window->flags().testFlag(Qt::X11BypassWindowManagerHint)
                || !content || content->window() != window
                || content->metaObject()->indexOfMethod("focusBoundary(QVariant)") < 0) {
                diagnostic = QStringLiteral("invalid fixed chrome window %1: expected named transient top-level unmanaged frameless Tool and focusBoundary(QVariant) content")
                    .arg(QString::fromLatin1(names[i]));
                break;
            }
        }
    }
    if (!diagnostic.isEmpty()) {
        for (auto *window : {top, volume, bottom}) {
            if (window && window != m_root.data() && window != m_host.data())
                window->hide();
        }
        failChrome(diagnostic);
        return;
    }
    m_topChromeWindow = windows[0];
    m_volumeChromeWindow = windows[1];
    m_bottomChromeWindow = windows[2];
    m_chromeRegistered = true;
    refreshChromeFamilyActive(QGuiApplication::focusWindow());
    for (auto *window : windows) {
        connect(window, &QObject::destroyed, this, [this] {
            if (m_root && !m_quitAuthorized)
                failChrome(QStringLiteral("registered chrome QQuickWindow was destroyed before authorized root teardown"));
        });
        connect(window, &QWindow::visibleChanged, this, [this](bool) { scheduleGeometryLog(); });
    }
    emitNativeDiagnostic("native_chrome_registered", {{"root", m_rootXid}, {"count", 3}});
    scheduleChromeGeometry();
}

void FuramiBridge::scheduleChromeGeometry()
{
    assertGuiThread();
    if (m_chromeGeometryPending || !m_root || !m_chromeRegistered || m_bootstrapFailed || m_quitAuthorized)
        return;
    m_chromeGeometryPending = true;
    QTimer::singleShot(0, this, [this] {
        m_chromeGeometryPending = false;
        if (!m_root || !m_chromeRegistered || m_bootstrapFailed || m_quitAuthorized)
            return;
        for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
            if (!window || window->parent() || window->transientParent() != m_root.data()
                || (window->flags() & Qt::WindowType_Mask) != Qt::Tool
                || !window->flags().testFlag(Qt::FramelessWindowHint)
                || !window->flags().testFlag(Qt::X11BypassWindowManagerHint)) {
                failChrome(QStringLiteral("registered chrome lost its fixed transient top-level unmanaged Tool association"));
                return;
            }
        }
        if (!QMetaObject::invokeMethod(m_root, "syncChromeGeometry", Qt::DirectConnection))
            failChrome(QStringLiteral("QML root syncChromeGeometry hook disappeared"));
    });
}

void FuramiBridge::focusNextChrome(const QString &region, bool backward)
{
    assertGuiThread();
    if (!m_chromeFamilyActive || !m_chromeRegistered || !m_root || m_popupOpen || m_bootstrapFailed || m_quitAuthorized
        || !m_root->isVisible() || m_root->visibility() == QWindow::Minimized)
        return;
    QQuickWindow *target = nullptr;
    const char *contentName = nullptr;
    if (region == QLatin1String("video")) {
        target = backward ? m_bottomChromeWindow.data() : m_topChromeWindow.data();
        contentName = backward ? "bottomChromeRoot" : "topChromeRoot";
    } else if (region == QLatin1String("top")) {
        if (!backward) {
            target = m_volumeChromeWindow.data();
            contentName = "volumeChromeRoot";
        }
    } else if (region == QLatin1String("volume")) {
        target = backward ? m_topChromeWindow.data() : m_bottomChromeWindow.data();
        contentName = backward ? "topChromeRoot" : "bottomChromeRoot";
    } else if (region == QLatin1String("bottom")) {
        if (backward) {
            target = m_volumeChromeWindow.data();
            contentName = "volumeChromeRoot";
        }
    } else {
        failChrome(QStringLiteral("unknown chrome focus region"));
        return;
    }
    if (!contentName) {
        focusVideo();
        return;
    }
    auto *content = target ? target->findChild<QQuickItem *>(QLatin1String(contentName)) : nullptr;
    if (!content || !QMetaObject::invokeMethod(content, "focusBoundary", Qt::DirectConnection,
            Q_ARG(QVariant, QVariant(backward)))) {
        failChrome(QStringLiteral("registered chrome focus boundary disappeared"));
        return;
    }
    target->requestActivate();
    emitNativeDiagnostic("input_focus_handoff", {{"object_name", controlClass(target->objectName())}, {"backward", backward}, {"root", m_rootXid}, {"generation", m_generation}});
}

void FuramiBridge::setTextEntryActive(bool active)
{
    assertGuiThread();
    if (m_textEntryActive == active)
        return;
    m_textEntryActive = active;
    emitNativeDiagnostic("input_text_focus", {{"active", active}, {"generation", m_generation}});
    emit textEntryActiveChanged();
}

void FuramiBridge::setPopupOpen(bool open)
{
    assertGuiThread();
    if (m_popupOpen == open)
        return;
    m_popupOpen = open;
    if (open)
        m_volumeWheelRemainder = 0;
    emitNativeDiagnostic("qt_chrome_popup", {{"open", open}, {"root", m_rootXid}});
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
    refreshChromeFamilyActive(QGuiApplication::focusWindow());
    root->setCursor(Qt::ArrowCursor);
    m_fullscreenObserved = root->visibility() == QWindow::FullScreen;
    emitNativeDiagnostic("native_root", {{"root", m_rootXid}});
    connect(root, &QWindow::widthChanged, this, [this] { scheduleGeometryLog(); scheduleChromeGeometry(); });
    connect(root, &QWindow::heightChanged, this, [this] { scheduleGeometryLog(); scheduleChromeGeometry(); });
    connect(root, &QWindow::xChanged, this, [this] { scheduleChromeGeometry(); });
    connect(root, &QWindow::yChanged, this, [this] { scheduleChromeGeometry(); });
    connect(root, &QWindow::screenChanged, this, [this] { watchScreenGeometry(); scheduleGeometryLog(); });
    watchScreenGeometry();
    connect(root, &QQuickWindow::activeFocusItemChanged, this, [this] {
        if (!m_root)
            return;
        const auto *item = m_root->activeFocusItem();
        emitNativeDiagnostic("input_focus_observed", {{"root", m_rootXid}, {"active", m_root->isActive()}, {"object_name", controlClass(item ? item->objectName() : QString())}, {"generation", m_generation}});
    });
    connect(container, &QQuickItem::widthChanged, this, [this] { scheduleGeometryLog(); });
    connect(container, &QQuickItem::heightChanged, this, [this] { scheduleGeometryLog(); });
    connect(root, &QObject::destroyed, this, [this] {
        assertGuiThread();
        m_chromeIdleTimer.stop();
        for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
            if (window)
                window->hide();
        }
        cancelFullscreenChoice("root-destroyed");
        m_root.clear();
        m_container.clear();
        refreshChromeFamilyActive(QGuiApplication::focusWindow());
        emitNativeDiagnostic("native_root_destroyed", {{"root", m_rootXid}, {"generation", m_generation}, {"authorized", m_quitAuthorized}});
        if (!m_quitAuthorized) {
            // Root loss is a failed bootstrap even when no media owner exists.
            // Active owners still follow their real stop acknowledgement protocol.
            m_bootstrapFailed = true;
            m_failed = true;
            m_nativeDiagnostic = QStringLiteral("application root QObject was destroyed without authorized application close");
            m_diagnostic = m_nativeDiagnostic;
            m_contextDiagnostic = m_nativeDiagnostic;
            emit stateChanged();
            emitNativeDiagnostic("native_root_destroyed_uncontrolled", {{"root", m_rootXid}, {"generation", m_generation}}, 2);
            if (m_nativeGeneration)
                failNative(m_nativeGeneration, QStringLiteral("application root was destroyed before owner acknowledgement"));
            applyUpdate(gate_quit(*m_gate));
        }
    });
    connect(root, &QWindow::visibilityChanged, this, [this](QWindow::Visibility visibility) {
        if (visibility == QWindow::Hidden || visibility == QWindow::Minimized) {
            cancelFullscreenChoice("not-visible");
            hideChromeWindows();
        } else if (m_fullscreenChoicePending && m_root) {
            m_root->update();
        }
        scheduleChromeGeometry();
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
    emitNativeDiagnostic("qualification_stdin_enabled");
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
            emitNativeDiagnostic("qualification_stdin_read_failed", {{"errno", errno}}, 1);
        else
            emitNativeDiagnostic("qualification_stdin_eof");
        m_qualificationLine.clear();
        return;
    }
    // One bounded read per callback. Oversize lines are discarded whole, not
    // interpreted as command fragments. No inventory event submits controls.
    for (ssize_t index = 0; index < count; ++index) {
        const char byte = bytes[index];
        if (byte == '\n') {
            if (!m_dropQualificationLine && !m_qualificationLine.isEmpty()) {
                if (!m_qualificationLine.isValidUtf8()) {
                    emitNativeDiagnostic("qualification_invalid_utf8", {}, 1);
                } else {
                    const auto &command = m_qualificationLine;
                    const bool allowedQualificationCommand = command == "snapshot" || command == "fullscreen"
                        || command == "application-close" || command.startsWith("pause ");
                    if (m_qualificationVideo && !allowedQualificationCommand) {
                        applyUpdate(gate_qualification_command(*m_gate, asRust(command)));
                        m_qualificationLine.clear();
                        m_dropQualificationLine = false;
                        continue;
                    }
                    if (command == "fullscreen") toggleFullscreen();
                    else if (command == "application-close") requestApplicationClose();
                    else if (command == "reset-confirm") decideSettingsReset(QString::number(m_resetToken), true);
                    else if (command == "open-close-during-opening") openAndCloseDuringOpeningForProof();
                    else if (command == "force-surface-loss") forceSurfaceLossForProof();
                    else {
                        if (command == "close-discard" || command.startsWith("quit ")) observeFullscreenChoice();
                        applyUpdate(gate_qualification_command(*m_gate, asRust(command)));
                        if (command == "snapshot")
                            scheduleGeometryLog();
                    }
                }
            }
            m_qualificationLine.clear();
            m_dropQualificationLine = false;
        } else if (!m_dropQualificationLine) {
            if (m_qualificationLine.size() == 65'536) {
                m_dropQualificationLine = true;
                m_qualificationLine.clear();
                emitNativeDiagnostic("qualification_line_oversized", {}, 1);
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
    emitNativeDiagnostic("intent_open_capture");
    applyUpdate(gate_open(*m_gate));
}

void FuramiBridge::openLogFolder()
{
    assertGuiThread();
    applyUpdate(gate_open_log_folder(*m_gate));
}

void FuramiBridge::refreshCatalogs()
{
    assertGuiThread();
    applyUpdate(gate_refresh_catalogs(*m_gate));
}

void FuramiBridge::selectCapture(const QString &selectionRevision, const QString &catalogRevision, const QString &rowKey)
{
    assertGuiThread();
    applyUpdate(gate_select_capture(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(rowKey.toUtf8())));
}

void FuramiBridge::selectSize(const QString &selectionRevision, const QString &catalogRevision, const QString &rowKey)
{
    assertGuiThread();
    applyUpdate(gate_select_size(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(rowKey.toUtf8())));
}

void FuramiBridge::setSize(const QString &selectionRevision, const QString &catalogRevision, const QString &width, const QString &height)
{
    assertGuiThread();
    applyUpdate(gate_set_size(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(width.toUtf8()), asRust(height.toUtf8())));
}

void FuramiBridge::selectRate(const QString &selectionRevision, const QString &catalogRevision, const QString &rowKey)
{
    assertGuiThread();
    applyUpdate(gate_select_rate(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(rowKey.toUtf8())));
}

void FuramiBridge::setRate(const QString &selectionRevision, const QString &catalogRevision, const QString &exactRate)
{
    assertGuiThread();
    applyUpdate(gate_set_rate(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(exactRate.toUtf8())));
}

void FuramiBridge::selectFormat(const QString &selectionRevision, const QString &catalogRevision, const QString &rowKey)
{
    assertGuiThread();
    applyUpdate(gate_select_format(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(rowKey.toUtf8())));
}

void FuramiBridge::setAudioEnabled(const QString &selectionRevision, bool enabled)
{
    assertGuiThread();
    applyUpdate(gate_set_audio_enabled(*m_gate, asRust(selectionRevision.toUtf8()), enabled));
}

void FuramiBridge::selectAudioInput(const QString &selectionRevision, const QString &catalogRevision, const QString &rowKey)
{
    assertGuiThread();
    applyUpdate(gate_select_audio_input(*m_gate, asRust(selectionRevision.toUtf8()), asRust(catalogRevision.toUtf8()), asRust(rowKey.toUtf8())));
}

void FuramiBridge::setScale(const QString &selectionRevision, const QString &kernel)
{
    assertGuiThread();
    applyUpdate(gate_set_scale(*m_gate, asRust(selectionRevision.toUtf8()), asRust(kernel.toUtf8())));
}

void FuramiBridge::setCscale(const QString &selectionRevision, const QString &kernel)
{
    assertGuiThread();
    applyUpdate(gate_set_cscale(*m_gate, asRust(selectionRevision.toUtf8()), asRust(kernel.toUtf8())));
}

void FuramiBridge::setDscale(const QString &selectionRevision, const QString &kernel)
{
    assertGuiThread();
    applyUpdate(gate_set_dscale(*m_gate, asRust(selectionRevision.toUtf8()), asRust(kernel.toUtf8())));
}

void FuramiBridge::setScaleAntiring(const QString &selectionRevision, double value)
{
    assertGuiThread();
    applyUpdate(gate_set_scale_antiring(*m_gate, asRust(selectionRevision.toUtf8()), value));
}

void FuramiBridge::setDither(const QString &selectionRevision, const QString &mode)
{
    assertGuiThread();
    applyUpdate(gate_set_dither(*m_gate, asRust(selectionRevision.toUtf8()), asRust(mode.toUtf8())));
}

void FuramiBridge::setDebandEnabled(const QString &selectionRevision, bool enabled)
{
    assertGuiThread();
    applyUpdate(gate_set_deband_enabled(*m_gate, asRust(selectionRevision.toUtf8()), enabled));
}

void FuramiBridge::setDebandIterations(const QString &selectionRevision, int value)
{
    assertGuiThread();
    applyUpdate(gate_set_deband_iterations(*m_gate, asRust(selectionRevision.toUtf8()), value));
}

void FuramiBridge::setDebandGrain(const QString &selectionRevision, double value)
{
    assertGuiThread();
    applyUpdate(gate_set_deband_grain(*m_gate, asRust(selectionRevision.toUtf8()), value));
}

void FuramiBridge::setInterpolation(const QString &selectionRevision, bool enabled)
{
    assertGuiThread();
    applyUpdate(gate_set_interpolation(*m_gate, asRust(selectionRevision.toUtf8()), enabled));
}

void FuramiBridge::restartCapture(const QString &expectedGeneration)
{
    assertGuiThread();
    bool valid = false;
    const auto generation = expectedGeneration.toULongLong(&valid);
    if (!valid || !m_canRestart || m_bootstrapFailed)
        return;
    emitNativeDiagnostic("intent_restart_capture", {{"generation", generation}, {"attempt_id", m_restartGeneration}, {"accepted", m_canRestart}});
    applyUpdate(gate_restart(*m_gate, generation));
}

void FuramiBridge::reconnectCapture(const QString &expectedGeneration)
{
    assertGuiThread();
    bool valid = false;
    const auto generation = expectedGeneration.toULongLong(&valid);
    if (!valid || m_bootstrapFailed)
        return;
    emitNativeDiagnostic("intent_reconnect_capture", {{"generation", generation}, {"attempt_id", m_restartGeneration}});
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
    emitNativeDiagnostic("intent_choose_recovery", {{"generation", generation}, {"watch", watch}, {"epoch", epoch}, {"candidate", candidate}});
    applyUpdate(gate_choose_recovery(*m_gate, generation, watch, epoch, candidate));
}

void FuramiBridge::closeCapture()
{
    assertGuiThread();
    emitNativeDiagnostic("intent_close_session", {{"generation", m_generation}});
    applyUpdate(gate_close(*m_gate, m_generation, false));
}

void FuramiBridge::togglePause()
{
    assertGuiThread();
    if (!m_canTogglePause || m_bootstrapFailed)
        return;
    const auto status = gate_pause(*m_gate, m_generation);
    emitNativeDiagnostic("intent_toggle_pause", {{"generation", m_generation}, {"accepted", status == SubmitStatus::Accepted}});
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::setVolume(int percent)
{
    assertGuiThread();
    const auto status = gate_set_volume(*m_gate, percent);
    emitNativeDiagnostic("intent_set_volume", {{"volume_percent", percent}, {"accepted", status == SubmitStatus::Accepted}});
    // Every admission outcome publishes the authoritative retained preference.
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::setMuted(bool muted)
{
    assertGuiThread();
    const auto status = gate_set_muted(*m_gate, muted);
    emitNativeDiagnostic("intent_set_muted", {{"muted", muted}, {"accepted", status == SubmitStatus::Accepted}});
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::selectOutput(const QString &rowKey, const QString &catalogRevision)
{
    assertGuiThread();
    bool revisionValid = false;
    const auto revision = catalogRevision.toULongLong(&revisionValid);
    if (!revisionValid) {
        emitNativeDiagnostic("intent_select_output", {{"accepted", false}});
        return;
    }
    const auto status = gate_select_output(*m_gate, asRust(rowKey.toUtf8()), revision);
    emitNativeDiagnostic("intent_select_output", {{"revision", revision}, {"accepted", status == SubmitStatus::Accepted}});
    // Every admission outcome republishes the authoritative projection,
    // including the unchanged selection after a stale/failed attempt.
    applyUpdate(gate_poll(*m_gate));
}

void FuramiBridge::openAndCloseDuringOpeningForProof()
{
    assertGuiThread();
    if (!m_qualificationInput || !m_canOpen || m_bootstrapFailed)
        return;
    emitNativeDiagnostic("intent_open_close_during_opening");
    m_closeOnNextOpening = true;
    applyUpdate(gate_open(*m_gate));
}

void FuramiBridge::forceSurfaceLossForProof()
{
    assertGuiThread();
    if (!m_qualificationInput || m_actualPhase != QStringLiteral("Ready") || m_failed || !m_host || m_poisoned)
        return;
    emitNativeDiagnostic("intent_force_surface_loss", {{"root", m_rootXid}, {"host", m_lastHostXid}, {"generation", m_generation}});
    // This is deliberately unauthorized. Only the real platform event reports loss.
    m_host->destroy();
}

void FuramiBridge::requestApplicationClose()
{
    assertGuiThread();
    if (m_quitAuthorized)
        return;
    emitNativeDiagnostic("intent_application_close", {{"root", m_rootXid}, {"generation", m_generation}});
    // Only an already-observed and synchronized transition may commit here.
    // Otherwise shutdown retains the last confirmed preference, not a request.
    observeFullscreenChoice();
    cancelFullscreenChoice("application-close");
    applyUpdate(gate_request_application_close(*m_gate));
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
    emitNativeDiagnostic("fullscreen_preference_readback", {{"effective", m_fullscreenObserved}, {"root", m_rootXid}, {"transition", m_fullscreenTransition}});
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
        emitNativeDiagnostic("fullscreen_transition_cancelled", {{"reason", reason}, {"requested", m_fullscreenRequested}, {"observed", m_fullscreenObserved}, {"retained_preference", m_fullscreenPreference}, {"root", m_rootXid}, {"transition", m_fullscreenTransition}, {"focus_restore_pending", m_fullscreenFocusRestorePending}});
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

void FuramiBridge::setStatsHidden(bool hidden)
{
    assertGuiThread();
    applyUpdate(gate_set_stats_hidden(*m_gate, hidden));
}

void FuramiBridge::setLoggingEnabled(bool enabled)
{
    assertGuiThread();
    applyUpdate(gate_set_logging_enabled(*m_gate, enabled));
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
    emitNativeDiagnostic("intent_toggle_fullscreen", {{"root", m_rootXid}, {"generation", m_generation}, {"fullscreen", !wasFullscreen}});
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
            auto *focused = QGuiApplication::focusWindow();
            m_fullscreenFocusWindow = m_topChromeWindow && focused == m_topChromeWindow.data() ? m_topChromeWindow.data()
                : m_volumeChromeWindow && focused == m_volumeChromeWindow.data() ? m_volumeChromeWindow.data()
                : m_bottomChromeWindow && focused == m_bottomChromeWindow.data() ? m_bottomChromeWindow.data() : m_root.data();
            m_fullscreenFocusItem = m_fullscreenFocusWindow->activeFocusItem();
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
    if (!m_chromeFamilyActive || m_quitAuthorized || m_popupOpen)
        return;
    auto *targetWindow = m_fullscreenFocusWindow.data();
    if (!targetWindow || !targetWindow->isVisible()
        || (targetWindow != m_root.data() && targetWindow != m_topChromeWindow.data()
            && targetWindow != m_volumeChromeWindow.data() && targetWindow != m_bottomChromeWindow.data()))
        targetWindow = m_root.data();
    auto *target = m_fullscreenFocusItem.data();
    if (!target || target->window() != targetWindow || !target->isVisible() || !target->isEnabled()) {
        targetWindow = m_root.data();
        target = m_container.data();
    }
    if (target) {
        target->forceActiveFocus(Qt::OtherFocusReason);
        targetWindow->requestActivate();
    }
    // Only the current published host may regain native focus; no saved host/XID.
    if (m_fullscreenNativeFocus && m_root->isActive() && m_host && m_published
        && !m_poisoned && !m_host->poisoned() && m_host->parent() == m_root.data()
        && m_container && m_container->property("window").value<QWindow *>() == m_host.data())
        m_host->requestActivate();
    emitNativeDiagnostic("fullscreen_focus_restored", {{"root", m_rootXid}, {"object_name", controlClass(target ? target->objectName() : QString())}, {"native_previously_focused", m_fullscreenNativeFocus}, {"generation", m_generation}});
}

void FuramiBridge::focusVideo()
{
    assertGuiThread();
    if (m_popupOpen)
        return;
    if (!m_chromeFamilyActive || !m_root || !m_container || m_quitAuthorized || m_closing || m_bootstrapFailed
        || !m_root->isVisible() || m_root->visibility() == QWindow::Minimized)
        return;
    m_container->forceActiveFocus(Qt::MouseFocusReason);
    if (m_host && m_published && !m_poisoned && !m_failed && !m_host->poisoned()
        && m_host->parent() == m_root.data()
        && m_container->property("window").value<QWindow *>() == m_host.data())
        m_host->requestActivate();
    else
        m_root->requestActivate();
    emitNativeDiagnostic("intent_focus_video", {{"root", m_rootXid}, {"host", m_lastHostXid}, {"generation", m_generation}});
}

bool FuramiBridge::handleKey(QKeyEvent *event, const char *source)
{
    assertGuiThread();
    emitNativeDiagnostic("input_key_press", {{"source", source}, {"repeat", event->isAutoRepeat()}, {"text_entry", m_textEntryActive}, {"popup", m_popupOpen}, {"generation", m_generation}});
    const int key = event->key();
    const bool videoFocus = m_root && m_container
        && (qstrcmp(source, "host") == 0
            || (qstrcmp(source, "root") == 0 && m_root->isActive()
                && (!m_root->activeFocusItem() || m_root->activeFocusItem() == m_container.data())));
    if (!m_popupOpen && videoFocus && m_chromeRegistered
        && (key == Qt::Key_Tab || key == Qt::Key_Backtab)
        && !(event->modifiers() & (Qt::ControlModifier | Qt::AltModifier | Qt::MetaModifier))) {
        const bool backward = key == Qt::Key_Backtab || event->modifiers().testFlag(Qt::ShiftModifier);
        focusNextChrome(QStringLiteral("video"), backward);
        event->accept();
        return true;
    }
    if (key != Qt::Key_Space && key != Qt::Key_F11 && key != Qt::Key_Escape)
        return false;
    if (event->modifiers() != Qt::NoModifier)
        return false;
    if (m_textEntryActive) {
        if (key == Qt::Key_Space)
            return false;
        event->accept();
        return true;
    }
    if (key == Qt::Key_Space && m_popupOpen)
        return false;
    if (key == Qt::Key_Space) {
        const bool hostOwnsFocus = m_host && !m_poisoned
            && QGuiApplication::focusWindow() == m_host.data();
        const bool containerOwnsFocus = m_root && m_root->isActive() && m_container
            && m_root->activeFocusItem() == m_container.data();
        // A stale host-origin event is not permission to bypass a focused control.
        if (qstrcmp(source, "host") == 0 ? !(hostOwnsFocus || containerOwnsFocus) : !containerOwnsFocus)
            return false;
    }
    if (event->isAutoRepeat()) {
        event->accept();
        return true;
    }
    if (key == Qt::Key_Space) {
        togglePause();
    } else if (key == Qt::Key_F11) {
        toggleFullscreen();
    } else if (m_popupOpen) {
        dismissChromePopup();
    } else {
        toggleChromeSettings();
    }
    event->accept();
    return true;
}

bool FuramiBridge::handleVolumeWheel(QWheelEvent *event, QWindow *source)
{
    assertGuiThread();
    if (!m_chromeRegistered || !m_chromeFamilyActive || !m_root || !m_root->isVisible()
        || m_root->visibility() == QWindow::Minimized || m_closing || m_quitAuthorized
        || m_bootstrapFailed || !canSetGain() || m_popupOpen || event->modifiers() != Qt::NoModifier) {
        m_volumeWheelRemainder = 0;
        return false;
    }
    const bool currentHost = m_host && source == m_host.data() && !m_poisoned && !m_host->poisoned();
    const bool overVideo = source == m_root.data() && m_container && m_container->window() == m_root.data()
        && QRectF(0, 0, m_container->width(), m_container->height()).contains(m_container->mapFromScene(event->position()));
    const bool overVolume = m_volumeChromeWindow && source == m_volumeChromeWindow.data()
        && m_volumeChromeWindow->isVisible()
        && QRectF(0, 0, m_volumeChromeWindow->width(), m_volumeChromeWindow->height()).contains(event->position());
    if (!currentHost && !overVideo && !overVolume) {
        m_volumeWheelRemainder = 0;
        return false;
    }
    if (event->phase() == Qt::ScrollBegin)
        m_volumeWheelRemainder = 0;
    const int delta = event->angleDelta().y();
    if (delta == 0) {
        m_volumeWheelRemainder = 0;
        return false;
    }
    const std::int64_t angle = static_cast<std::int64_t>(m_volumeWheelRemainder) + delta;
    const std::int64_t notches = angle / 120;
    m_volumeWheelRemainder = static_cast<int>(angle % 120);
    const int currentVolume = volumePercent();
    const int target = static_cast<int>(qBound<std::int64_t>(std::int64_t{0}, currentVolume + notches * 5, std::int64_t{100}));
    if ((delta > 0 && target == 100) || (delta < 0 && target == 0) || event->phase() == Qt::ScrollEnd)
        m_volumeWheelRemainder = 0;
    if (target != currentVolume)
        setVolume(target);
    noteNativePointerActivity(false);
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
    emitNativeDiagnostic("input_forward", {{"source", source}, {"event_type", static_cast<int>(event->type())}, {"text_entry", m_textEntryActive}, {"generation", m_generation}});
    QCoreApplication::sendEvent(m_root, event);
}

bool FuramiBridge::eventFilter(QObject *watched, QEvent *event)
{
    assertGuiThread();
    if (event->type() == QEvent::Quit && watched == qGuiApp && !m_quitAuthorized) {
        emitNativeDiagnostic("qt_quit_veto");
        requestApplicationClose();
        return true;
    }
    auto *chrome = watched == m_topChromeWindow ? m_topChromeWindow.data()
        : watched == m_volumeChromeWindow ? m_volumeChromeWindow.data()
        : watched == m_bottomChromeWindow ? m_bottomChromeWindow.data() : nullptr;
    if (chrome) {
        if (event->type() == QEvent::FocusIn || event->type() == QEvent::FocusOut) {
            emitNativeDiagnostic("input_focus_window", {{"object_name", controlClass(chrome->objectName())}, {"active", event->type() == QEvent::FocusIn}, {"root", m_rootXid}, {"generation", m_generation}});
        }
        if (event->type() == QEvent::WindowStateChange) {
            if (!m_root || !m_root->isVisible() || m_root->visibility() == QWindow::Minimized)
                hideChromeWindows();
            else if (chrome->windowState() != Qt::WindowNoState)
                chrome->setWindowState(Qt::WindowNoState);
            scheduleChromeGeometry();
        }
        if (event->type() == QEvent::Close && !m_quitAuthorized) {
            event->ignore();
            dismissChromePopup();
            if (m_popupOpen)
                return true;
            chrome->hide();
            m_chromeIdleTimer.stop();
            focusVideo();
            return true;
        }
        if (event->type() == QEvent::KeyPress)
            return handleKey(static_cast<QKeyEvent *>(event), "chrome");
        if (chrome == m_volumeChromeWindow.data() && event->type() == QEvent::Wheel
            && handleVolumeWheel(static_cast<QWheelEvent *>(event), chrome))
            return true;
        // An unmanaged window gets no compositor click-to-focus, so perform the
        // activation the WM would have done before the still-delivered event.
        if ((event->type() == QEvent::MouseButtonPress || event->type() == QEvent::TouchBegin)
            && QGuiApplication::focusWindow() != chrome && m_chromeFamilyActive && m_root && m_root->isVisible()
            && m_root->visibility() != QWindow::Minimized && !m_closing && !m_bootstrapFailed
            && !m_quitAuthorized)
            chrome->requestActivate();
        if (event->type() == QEvent::MouseMove || event->type() == QEvent::MouseButtonPress
            || event->type() == QEvent::Wheel || event->type() == QEvent::TouchBegin
            || event->type() == QEvent::TouchUpdate)
            noteNativePointerActivity(false);
        if (event->type() == QEvent::Move || event->type() == QEvent::Resize) {
            scheduleChromeGeometry();
            scheduleGeometryLog();
        }
        if (event->type() == QEvent::Show) {
            if (!m_chromeFamilyActive || !m_root || !m_root->isVisible() || m_root->visibility() == QWindow::Minimized
                || m_quitAuthorized || m_bootstrapFailed)
                chrome->hide();
            else
                chrome->raise();
        }
    }
    if (watched == m_root) {
        if (event->type() == QEvent::Wheel
            && handleVolumeWheel(static_cast<QWheelEvent *>(event), m_root.data()))
            return true;
        if (event->type() == QEvent::MouseMove || event->type() == QEvent::MouseButtonPress
            || event->type() == QEvent::Wheel || event->type() == QEvent::TouchBegin
            || event->type() == QEvent::TouchUpdate) {
            const bool pressed = event->type() == QEvent::MouseButtonPress || event->type() == QEvent::TouchBegin;
            noteNativePointerActivity(pressed);
            if (pressed && m_popupOpen) {
                event->accept();
                return true;
            }
        }
        if (event->type() == QEvent::FocusIn || event->type() == QEvent::FocusOut) {
            emitNativeDiagnostic("input_focus_window", {{"object_name", "furamiRoot"}, {"active", event->type() == QEvent::FocusIn}, {"root", m_rootXid}, {"generation", m_generation}});
        }
        if (event->type() == QEvent::Close && !m_quitAuthorized) {
            event->ignore();
            emitNativeDiagnostic("qt_root_close_veto", {{"root", m_rootXid}, {"generation", m_generation}});
            requestApplicationClose();
            return true;
        }
        if (event->type() == QEvent::Close && m_quitAuthorized)
            hideChromeWindows();
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
                    emitNativeDiagnostic("fullscreen_native_state_observed", {{"requested", m_fullscreenRequested}, {"effective", m_fullscreenObserved}, {"matched", m_fullscreenNativeTransitionObserved}, {"root", m_rootXid}, {"transition", m_fullscreenTransition}});
                    if (m_fullscreenNativeTransitionObserved)
                        synchronizeFullscreenChoice();
                }
            }
        }
        if (event->type() == QEvent::Move || event->type() == QEvent::Resize || event->type() == QEvent::WindowStateChange) {
            if ((m_fullscreenChoicePending || m_fullscreenFocusRestorePending)
                && m_fullscreenNativeTransitionObserved)
                m_root->update();
            scheduleGeometryLog();
            scheduleChromeGeometry();
        }
    }
    return QObject::eventFilter(watched, event);
}

void FuramiBridge::applyUpdate(UiUpdate update)
{
    assertGuiThread();
    m_qualificationFailure = m_qualificationFailure || (m_qualificationVideo && update.terminal_failure);
    if (!update.changed)
        return;
    const bool productStateChanged = m_actualPhase != phaseName(update.phase)
        || m_generation != update.generation || m_restartGeneration != update.restart_generation
        || m_failed != (update.failed || m_bootstrapFailed) || m_paused != update.paused
        || m_volumePercent != update.volume_percent || m_muted != update.muted
        || m_canRestart != (update.can_restart && !m_bootstrapFailed)
        || m_canTogglePause != (update.can_toggle_pause && !m_bootstrapFailed)
        || m_canSetGain != (update.can_set_gain && !m_bootstrapFailed);
    if (update.restart_generation != m_restartGeneration && !m_bootstrapFailed)
        m_nativeDiagnostic.clear();
    m_actualPhase = phaseName(update.phase);
    if (!update.ui_snapshot.empty())
        m_uiSnapshot = fromRust(update.ui_snapshot);
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
    m_closeDialog = fromRust(update.close_dialog);
    m_resetToken = update.reset_token;
    m_fullscreenPreference = update.fullscreen;
    m_statsHiddenPreference = update.stats_hidden;
    m_loggingEnabledPreference = update.logging_enabled;
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
    m_contextDiagnostic = fromRust(update.context_diagnostic);
    if (m_failed && !m_nativeDiagnostic.isEmpty()) {
        m_diagnostic = m_diagnostic.isEmpty() ? m_nativeDiagnostic
            : m_nativeDiagnostic + QStringLiteral("; ") + m_diagnostic;
        m_contextDiagnostic = m_contextDiagnostic.isEmpty() ? m_nativeDiagnostic
            : m_nativeDiagnostic + QStringLiteral("; ") + m_contextDiagnostic;
    }
    // Diagnostic health/counters alone must never enqueue another event.
    if (productStateChanged)
        emitNativeDiagnostic("ui_phase", {{"gate_phase", static_cast<int>(update.phase)}, {"generation", m_generation}, {"restart_generation", m_restartGeneration}, {"failed", m_failed}, {"paused", m_paused}, {"volume_percent", m_volumePercent}, {"muted", m_muted}, {"can_restart", m_canRestart}, {"can_toggle_pause", m_canTogglePause}, {"can_set_gain", m_canSetGain}});
    emit stateChanged();
    if (productStateChanged)
        scheduleGeometryLog();
    // The reducer emits effects in this order. Native effects never invent an ack.
    if (update.create_native)
        createNative(update.generation);
    if (update.release_native) {
        emitNativeDiagnostic("ui_stop_ack", {{"generation", update.generation}, {"root", m_rootXid}, {"host", m_lastHostXid}});
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
        hideChromeWindows();
        emitNativeDiagnostic("qt_quit_authorized", {{"generation", update.generation}, {"root", m_rootXid}});
        QTimer::singleShot(0, qGuiApp, &QCoreApplication::quit);
    }
    if (update.log_folder_request != 0
        && gate_claim_log_folder_dispatch(*m_gate, update.log_folder_request)) {
        launchLogFolderOpener(fromRust(update.log_folder_path), update.log_folder_request);
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
    refreshChromeFamilyActive(QGuiApplication::focusWindow());
    connect(host, &QObject::destroyed, this, [this, generation] {
        assertGuiThread();
        emitNativeDiagnostic("native_object_destroyed", {{"generation", generation}, {"host", m_lastHostXid}, {"authorized", m_releasing}});
        if (!m_releasing && generation == m_nativeGeneration) {
            m_host.clear();
            refreshChromeFamilyActive(QGuiApplication::focusWindow());
            // WindowContainer first receives its own windowDestroyed notification.
            QTimer::singleShot(0, this, [this] { emit hostWindowChanged(); });
            if (!m_poisoned)
                failNative(generation, QStringLiteral("host QObject was destroyed before owner acknowledgement"));
        }
    });
    emitNativeDiagnostic("native_create", {{"generation", generation}, {"root", m_rootXid}});
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
        emitNativeDiagnostic("native_publication_rejected", {{"generation", generation}, {"current", m_generation}, {"poisoned", m_poisoned}});
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
    emitNativeDiagnostic("native_publish", {{"root", m_rootXid}, {"host", xid}, {"generation", generation}, {"x11_parent", static_cast<std::uint64_t>(parent)}});
    auto update = gate_surface_ready(*m_gate, generation, xid);
    const bool closeDuringOpening = m_closeDuringOpening == generation && update.phase == GatePhase::Opening;
    applyUpdate(std::move(update));
    if (closeDuringOpening) {
        m_closeDuringOpening = 0;
        auto close = gate_close(*m_gate, generation, false);
        emitNativeDiagnostic("proof_close_during_opening", {{"generation", generation}, {"accepted", close.phase == GatePhase::Stopping && !close.failed}});
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
    emitNativeDiagnostic("native_surface_loss", {{"generation", generation}, {"host", previousXid}, {"root", m_rootXid}, {"controlled", false}}, 2);
    m_insideSurfaceCallback = true;
    applyUpdate(gate_surface_lost(*m_gate, generation));
    emitNativeDiagnostic("native_loss_barrier_begin", {{"generation", generation}, {"host", previousXid}});
    const QString diagnostic = fromRust(gate_wait_for_owner_ack(*m_gate, generation));
    if (!diagnostic.isEmpty()) {
        emitNativeDiagnostic("native_loss_barrier_failed", {{"generation", generation}, {"host", previousXid}}, 3);
        qFatal("native_loss_barrier_failed");
    }
    emitNativeDiagnostic("native_loss_barrier_complete", {{"generation", generation}, {"host", previousXid}});
    m_insideSurfaceCallback = false;
}

void FuramiBridge::failNative(std::uint64_t generation, const QString &diagnostic)
{
    assertGuiThread();
    if (m_host)
        m_host->poison();
    m_poisoned = true;
    m_nativeDiagnostic = diagnostic;
    emitNativeDiagnostic("native_failure", {{"generation", generation}, {"root", m_rootXid}, {"host", m_lastHostXid}}, 2);
    applyUpdate(gate_surface_lost(*m_gate, generation));
}

void FuramiBridge::releaseNative(std::uint64_t generation)
{
    assertGuiThread();
    if (!generation || generation != m_nativeGeneration || m_actualPhase != QStringLiteral("Releasing") || m_releasing) {
        emitNativeDiagnostic("native_release_rejected", {{"generation", generation}, {"current", m_nativeGeneration}}, 2);
        return;
    }
    m_releasing = true;
    QPointer<HostWindow> surviving = m_host;
    if (surviving) {
        surviving->authorizeDestruction();
        emitNativeDiagnostic("native_destroy_begin", {{"generation", generation}, {"host", m_lastHostXid}, {"root", m_rootXid}, {"parent_preserved", surviving->parent() == m_root.data()}});
        if (surviving->handle()) {
            surviving->hide();
            surviving->destroy();
        }
    }
    m_host.clear();
    refreshChromeFamilyActive(QGuiApplication::focusWindow());
    emit hostWindowChanged();
    // Clear only after platform destruction, while QPointer guards parent-driven deletion.
    if (m_container)
        m_container->setProperty("window", QVariant::fromValue<QWindow *>(nullptr));
    if (surviving)
        delete surviving.data();
    emitNativeDiagnostic("native_destroy", {{"generation", generation}, {"host", m_lastHostXid}, {"root", m_rootXid}, {"authorized", true}, {"poisoned", m_poisoned}});
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
    emitNativeDiagnostic("qt_geometry", {{"root", m_rootXid}, {"generation", m_generation}, {"root_width", m_root->width()}, {"root_height", m_root->height()}, {"video_x", video.x()}, {"video_y", video.y()}, {"video_width", video.width()}, {"video_height", video.height()}, {"dpr", dpr}});
    for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
        if (!window)
            continue;
        const auto local = m_root->mapFromGlobal(window->position());
        emitNativeDiagnostic("qt_chrome_geometry", {{"object_name", controlClass(window->objectName())}, {"root", m_rootXid}, {"x", local.x()}, {"y", local.y()}, {"global_x", window->x()}, {"global_y", window->y()}, {"width", window->width()}, {"height", window->height()}, {"visible", window->isVisible()}, {"native_parent_null", window->parent() == nullptr}, {"transient_root", window->transientParent() == m_root.data()}});
    }
    std::uint64_t controlOrdinal = 0;
    const auto logControl = [&](const QString &name, QQuickItem *item) {
        if (!item || !item->window())
            return;
        const auto scene = item->mapRectToScene(QRectF(0, 0, item->width(), item->height()));
        const auto origin = m_root->mapFromGlobal(item->window()->mapToGlobal(scene.topLeft()));
        const QRectF rect(origin, scene.size());
        emitNativeDiagnostic("ui_control", {{"object_name", controlClass(name)}, {"ordinal", controlOrdinal++}, {"x", qRound(rect.x() * dpr)}, {"y", qRound(rect.y() * dpr)}, {"width", qRound(rect.width() * dpr)}, {"height", qRound(rect.height() * dpr)}, {"visible", item->isVisible() && item->window()->isVisible()}, {"enabled", item->isEnabled()}});
    };
    const char *names[] = {
        "videoContainer", "topChromeRoot", "volumeChromeRoot", "bottomChromeRoot",
        "ribbonSource", "ribbonOutput", "ribbonResolution", "ribbonRate", "ribbonFullscreen",
        "volumeSlider", "volumeMute", "optionsTrigger",
        "menuOpenCapture", "menuPauseResume", "menuRestart", "menuReconnect", "menuSettings", "menuCloseCapture",
        "menuBack", "menuBackSection", "menuIntentError",
        "settingsCaptureEntry", "settingsAudioEntry", "settingsRendererEntry", "settingsPreferencesEntry",
        "contextIntentError", "contextDiagnostic",
        "menuOpenLogs", "contextDiagnosticsError",
        "contextOutputAction", "contextRecoveryStage", "recoveryUse",
        "sourceRefresh", "captureFormatSelector", "captureRefresh",
        "audioCaptureEnabled", "audioInputSelector", "audioMute", "audioOutputSelector", "audioOutputMute",
        "renderChoice_scale", "renderChoice_cscale", "renderChoice_dscale", "renderChoice_scale_antiring",
        "renderChoice_dither", "renderChoice_deband", "renderChoice_deband_iterations", "renderChoice_deband_grain", "renderChoice_interpolation",
        "renderToggle_scale", "renderToggle_cscale", "renderToggle_dscale", "renderToggle_scale_antiring",
        "renderToggle_dither", "renderToggle_deband", "renderToggle_deband_iterations", "renderToggle_deband_grain", "renderToggle_interpolation",
        "renderValue_scale", "renderValue_cscale", "renderValue_dscale", "renderValue_scale_antiring",
        "renderValue_dither", "renderValue_deband", "renderValue_deband_iterations", "renderValue_deband_grain", "renderValue_interpolation",
        "sizeWidthField", "sizeHeightField", "rateExactField", "sizeRateCatalogError",
        "rendererNotice",
        "preferencesStatus", "preferencesStartup", "preferencesPath", "preferencesImmediate",
        "preferencesReset", "preferencesRefused",
        "sourceCatalogError", "sourceCapability", "outputMissing", "outputStatus",
        "saveFailureRetry", "saveFailureClose", "resetCancel", "resetConfirm",
        "backdropTitle", "backdropDetail", "backdropDiagnostic"
    };
    for (const auto *name : names) {
        auto *item = m_root->findChild<QQuickItem *>(QLatin1String(name));
        if (!item) {
            for (auto *window : {m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
                if (window)
                    item = window->findChild<QQuickItem *>(QLatin1String(name));
                if (item)
                    break;
            }
        }
        logControl(QString::fromLatin1(name), item);
    }
    // Real product rows carry model-derived suffixes; one visual-tree walk over the owned
    // subtrees finds them without a per-name recursive search or an inspector registry.
    // Model-driven controls are QML Repeater delegates: they exist only in the visual tree
    // (QQuickItem::childItems()) and are absent from QObject::children(), so a QObject walk
    // never sees them. Each item has exactly one parent item, so nothing logs twice.
    const char *dynamicPrefixes[] = {
        "sourceRow_", "outputRow_", "sizeRow_", "rateRow_",
        "filterToggle_", "filterAdd_", "filterRemove_", "filterMoveUp_", "filterMoveDown_",
        "filterNumber_", "filterChoice_", "filterInputError_",
        "captureFormatSelector_row_", "audioInputSelector_row_", "audioOutputSelector_row_",
        "renderChoice_scale_row_", "renderChoice_cscale_row_", "renderChoice_dscale_row_",
        "renderChoice_scale_antiring_row_", "renderChoice_dither_row_", "renderChoice_deband_row_",
        "renderChoice_deband_iterations_row_", "renderChoice_deband_grain_row_", "renderChoice_interpolation_row_"
    };
    const auto ownsWindow = [&](QQuickWindow *window) {
        return window == m_root.data() || window == m_topChromeWindow.data()
            || window == m_volumeChromeWindow.data() || window == m_bottomChromeWindow.data();
    };
    const auto walk = [&](const auto &self, QQuickItem *node) -> void {
        for (QQuickItem *child : node->childItems()) {
            const QString &objectName = child->objectName();
            if (!objectName.isEmpty() && ownsWindow(child->window())) {
                for (const auto *prefix : dynamicPrefixes) {
                    if (objectName.startsWith(QLatin1String(prefix))) {
                        logControl(objectName, child);
                        break;
                    }
                }
            }
            self(self, child);
        }
    };
    for (auto *window : {m_root.data(), m_topChromeWindow.data(), m_volumeChromeWindow.data(), m_bottomChromeWindow.data()}) {
        if (window && window->contentItem())
            walk(walk, window->contentItem());
    }
}

LaunchResult run_qt_application(rust::Box<RuntimeCoordinator> gate, rust::Str display, rust::Str mediaPrefix, rust::Str applicationVersion, bool qualificationStdin, bool qualificationVideo)
{
    qInstallMessageHandler(safeQtMessage);
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
    auto plugins = preflightRequiredQtPlugins(QString::fromUtf8(mediaPrefix.data(), static_cast<qsizetype>(mediaPrefix.size())));
    if (plugins.exit_code != 0)
        return plugins;

    int argc = 1;
    char program[] = "furami";
    char *argv[] = {program, nullptr};
    QGuiApplication application(argc, argv);
    QObject::connect(&application, &QObject::destroyed, [] {
        emitNativeDiagnostic("qt_application_destroy");
    });
    application.setApplicationName(QStringLiteral("Furami"));
    // The compiled Cargo package version crosses the CXX boundary as a borrowed
    // string and is exposed through Qt's own metadata, so QML reads
    // Qt.application.version without any release literal or runtime file read.
    application.setApplicationVersion(QString::fromUtf8(applicationVersion.data(), static_cast<qsizetype>(applicationVersion.size())));
    // One application-level icon: the root window and the chrome windows inherit
    // it, and Qt rasterizes the embedded SVG through the preflighted icon engine.
    application.setWindowIcon(QIcon(QStringLiteral(":/qt/qml/dev/antho/furami/assets/logo.svg")));
    application.setQuitOnLastWindowClosed(false);
    UiUpdate nativeEnvironment;
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
    emitNativeDiagnostic("native_startup", {{"shape_major", major}, {"shape_minor", minor}});
    const QByteArray qtVersion(qVersion());
    const QByteArray xVendor(XServerVendor(native->display()));
    nativeEnvironment = gate_native_environment(*gate, asRust(qtVersion), asRust(platform), asRust(xVendor),
        static_cast<std::uint32_t>(XVendorRelease(native->display())),
        static_cast<std::uint32_t>(ProtocolVersion(native->display())),
        static_cast<std::uint32_t>(ProtocolRevision(native->display())), major, minor);
#else
    return launchFailure(QStringLiteral("Qt was built without the required xcb native interface"));
#endif

    // Construction order keeps bridge/timer alive until after the QML engine/root dies.
    FuramiBridge bridge(std::move(gate), std::move(nativeEnvironment), qualificationVideo);
    QQmlApplicationEngine engine;
    QObject::connect(&engine, &QObject::destroyed, &application, [] {
        emitNativeDiagnostic("qt_engine_destroy");
    });
    // Keep Tools hidden until bindRoot creates their permanent native transient parent.
    engine.setInitialProperties({{QStringLiteral("bridge"), QVariant::fromValue(&bridge)},
                                 {QStringLiteral("visible"), false}});
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
    root->show();
    int exitCode = application.exec();
    if (!bridge.quitAuthorized()) {
        // An unsolicited loop exit cannot discard a live owner or its native parent.
        emitNativeDiagnostic("qt_uncontrolled_event_loop_exit", {}, 2);
        bridge.requestApplicationClose();
        application.exec();
        exitCode = 1;
    }
    emitNativeDiagnostic("qt_event_loop_exit", {{"exit_code", exitCode}, {"owner_cleanup_authorized", bridge.quitAuthorized()}});
    if (bridge.launchFailed()) {
        if (qualificationVideo)
            return LaunchResult{1, rust::String("qualification failed after owner drain"),
                                NativeFailureKind::Qualification, rust::String()};
        return launchFailure(bridge.diagnostic());
    }
    if (exitCode)
        return launchFailure(QStringLiteral("Qt event loop exited with status %1").arg(exitCode));
    return LaunchResult{0, rust::String(), NativeFailureKind::NoFailure, rust::String()};
}

} // namespace furami::bridge
