#ifndef FURAMI_NATIVE_HOST_HOST_H
#define FURAMI_NATIVE_HOST_HOST_H

#include "rust/cxx.h"

#include <QByteArray>

#include <QPointer>
#include <QQuickItem>
#include <QQuickWindow>
#include <QSocketNotifier>
#include <QString>
#include <QTimer>
#include <QWindow>
#include <QtQml/qqmlregistration.h>

#include <cstdint>

namespace furami::bridge {

struct GateCoordinator;
struct LaunchResult;
struct UiUpdate;
class HostWindow;

// Qt targets and the opaque coordinator remain owned by the GUI bootstrap.
class FuramiBridge : public QObject {
    Q_OBJECT
    QML_NAMED_ELEMENT(FuramiBridge)
    QML_UNCREATABLE("Owned by Furami bootstrap")
    Q_PROPERTY(QWindow *hostWindow READ hostWindow NOTIFY hostWindowChanged)
    Q_PROPERTY(QString phase READ phase NOTIFY stateChanged)
    Q_PROPERTY(QString diagnostic READ diagnostic NOTIFY stateChanged)
    Q_PROPERTY(bool paused READ paused NOTIFY stateChanged)
    Q_PROPERTY(bool ended READ ended NOTIFY stateChanged)
    Q_PROPERTY(bool textEntryActive READ textEntryActive WRITE setTextEntryActive NOTIFY textEntryActiveChanged)
    Q_PROPERTY(bool panelVisible READ panelVisible WRITE setPanelVisible NOTIFY panelVisibleChanged)
    Q_PROPERTY(bool popupOpen READ popupOpen WRITE setPopupOpen NOTIFY popupOpenChanged)
    Q_PROPERTY(bool captureSelected READ captureSelected CONSTANT)
    Q_PROPERTY(bool canRestart READ canRestart NOTIFY stateChanged)
    Q_PROPERTY(QString restartGeneration READ restartGeneration NOTIFY stateChanged)
    Q_PROPERTY(QString audioStatus READ audioStatus NOTIFY stateChanged)
    Q_PROPERTY(QString audioDiagnostic READ audioDiagnostic NOTIFY stateChanged)
    Q_PROPERTY(QString audioSource READ audioSource NOTIFY stateChanged)
    Q_PROPERTY(bool audioEnabled READ audioEnabled NOTIFY stateChanged)

public:
    explicit FuramiBridge(rust::Box<GateCoordinator> gate);
    ~FuramiBridge() override;

    QWindow *hostWindow() const;
    QString phase() const;
    QString diagnostic() const;
    bool paused() const;
    bool ended() const;
    bool textEntryActive() const;
    bool panelVisible() const;
    bool popupOpen() const;
    bool captureSelected() const;
    bool canRestart() const;
    QString restartGeneration() const;
    QString audioStatus() const;
    QString audioDiagnostic() const;
    QString audioSource() const;
    bool audioEnabled() const;
    void setTextEntryActive(bool active);
    void setPanelVisible(bool visible);
    void setPopupOpen(bool open);

    bool bindRoot(QQuickWindow *root, QQuickItem *container, QString &diagnostic);
    bool enableQualificationInput(QString &diagnostic);
    Q_INVOKABLE bool quitAuthorized() const;
    bool failed() const;

    Q_INVOKABLE void openCapture();
    Q_INVOKABLE void restartCapture(const QString &expectedGeneration);
    Q_INVOKABLE void closeCapture();
    Q_INVOKABLE void openAndCloseDuringOpeningForProof();
    Q_INVOKABLE void forceSurfaceLossForProof();
    Q_INVOKABLE void requestApplicationClose();
    Q_INVOKABLE void togglePanel();
    Q_INVOKABLE void toggleFullscreen();
    Q_INVOKABLE void focusVideo();

    void schedulePublication(std::uint64_t generation);
    void surfaceLost(std::uint64_t generation, std::uint64_t previousXid);
    bool handleKey(QKeyEvent *event, const char *source);
    void forwardToRoot(QEvent *event, const char *source);

signals:
    void hostWindowChanged();
    void stateChanged();
    void textEntryActiveChanged();
    void panelVisibleChanged();
    void popupOpenChanged();

protected:
    bool eventFilter(QObject *watched, QEvent *event) override;

private:
    void assertGuiThread() const;
    void applyUpdate(UiUpdate update);
    void createNative(std::uint64_t generation);
    void publishNative(std::uint64_t generation);
    void releaseNative(std::uint64_t generation);
    void failNative(std::uint64_t generation, const QString &diagnostic);
    void scheduleGeometryLog();
    void logGeometry();
    void readQualificationInput();

    rust::Box<GateCoordinator> m_gate;
    QTimer m_pollTimer;
    QPointer<HostWindow> m_host;
    QPointer<QSocketNotifier> m_qualificationInput;
    QByteArray m_qualificationLine;
    int m_stdinFlags = -1;
    bool m_dropQualificationLine = false;
    QPointer<QQuickWindow> m_root;
    QPointer<QQuickItem> m_container;
    QString m_actualPhase = QStringLiteral("Idle");
    QString m_diagnostic;
    QString m_nativeDiagnostic;
    std::uint64_t m_generation = 0;
    std::uint64_t m_restartGeneration = 0;
    bool m_canRestart = false;
    QString m_audioStatus = QStringLiteral("Disabled");
    QString m_audioDiagnostic;
    QString m_audioSource;
    bool m_audioEnabled = false;
    std::uint64_t m_nativeGeneration = 0;
    std::uint64_t m_rootXid = 0;
    std::uint64_t m_lastHostXid = 0;
    std::uint64_t m_closeDuringOpening = 0;
    bool m_failed = false;
    bool m_captureSelected = false;
    bool m_bootstrapFailed = false;
    bool m_paused = false;
    bool m_ended = false;
    bool m_textEntryActive = false;
    bool m_panelVisible = true;
    bool m_popupOpen = false;
    bool m_published = false;
    bool m_poisoned = false;
    bool m_releasing = false;
    bool m_quitAuthorized = false;
    bool m_geometryLogPending = false;
    bool m_insideSurfaceCallback = false;
};

LaunchResult run_qt_application(rust::Box<GateCoordinator> gate, rust::Str display, bool qualificationStdin);

} // namespace furami::bridge

#endif // FURAMI_NATIVE_HOST_HOST_H
