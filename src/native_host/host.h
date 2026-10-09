#ifndef FURAMI_NATIVE_HOST_HOST_H
#define FURAMI_NATIVE_HOST_HOST_H

#include "rust/cxx.h"

#include <QByteArray>
#include <QMetaObject>

#include <QPointer>
#include <QQuickItem>
#include <QQuickWindow>
#include <QRect>
#include <QSocketNotifier>
#include <QString>
#include <QTimer>
#include <QWindow>
#include <QtQml/qqmlregistration.h>

#include <cstdint>

namespace furami::bridge {

struct RuntimeCoordinator;
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
    Q_PROPERTY(bool presentationSuppressed READ presentationSuppressed NOTIFY stateChanged)
    Q_PROPERTY(int volumePercent READ volumePercent NOTIFY stateChanged)
    Q_PROPERTY(bool muted READ muted NOTIFY stateChanged)
    Q_PROPERTY(bool canTogglePause READ canTogglePause NOTIFY stateChanged)
    Q_PROPERTY(bool canSetGain READ canSetGain NOTIFY stateChanged)
    Q_PROPERTY(QString playbackStatus READ playbackStatus NOTIFY stateChanged)
    Q_PROPERTY(QString productPhase READ productPhase NOTIFY stateChanged)
    Q_PROPERTY(QString recoveryEvidence READ recoveryEvidence NOTIFY stateChanged)
    Q_PROPERTY(QString recoveryStage READ recoveryStage NOTIFY stateChanged)
    Q_PROPERTY(QString recoveryCandidates READ recoveryCandidates NOTIFY stateChanged)
    Q_PROPERTY(bool textEntryActive READ textEntryActive WRITE setTextEntryActive NOTIFY textEntryActiveChanged)
    Q_PROPERTY(bool panelVisible READ panelVisible WRITE setPanelVisible NOTIFY panelVisibleChanged)
    Q_PROPERTY(bool popupOpen READ popupOpen WRITE setPopupOpen NOTIFY popupOpenChanged)
    Q_PROPERTY(bool captureSelected READ captureSelected CONSTANT)
    Q_PROPERTY(bool canOpen READ canOpen NOTIFY stateChanged)
    Q_PROPERTY(bool canRestart READ canRestart NOTIFY stateChanged)
    Q_PROPERTY(QString restartGeneration READ restartGeneration NOTIFY stateChanged)
    Q_PROPERTY(QString audioStatus READ audioStatus NOTIFY stateChanged)
    Q_PROPERTY(QString audioDiagnostic READ audioDiagnostic NOTIFY stateChanged)
    Q_PROPERTY(QString audioSource READ audioSource NOTIFY stateChanged)
    Q_PROPERTY(QString audioDesired READ audioDesired NOTIFY stateChanged)
    Q_PROPERTY(QString outputRows READ outputRows NOTIFY stateChanged)
    Q_PROPERTY(QString outputCatalogRevision READ outputCatalogRevision NOTIFY stateChanged)
    Q_PROPERTY(QString outputSelected READ outputSelected NOTIFY stateChanged)
    Q_PROPERTY(QString outputSelectedKey READ outputSelectedKey NOTIFY stateChanged)
    Q_PROPERTY(QString outputEffective READ outputEffective NOTIFY stateChanged)
    Q_PROPERTY(QString outputStatus READ outputStatus NOTIFY stateChanged)
    Q_PROPERTY(bool outputNeedsAction READ outputNeedsAction NOTIFY stateChanged)
    Q_PROPERTY(QString settingsStatus READ settingsStatus NOTIFY stateChanged)
    Q_PROPERTY(QString settingsPath READ settingsPath NOTIFY stateChanged)
    Q_PROPERTY(bool settingsRefused READ settingsRefused NOTIFY stateChanged)
    Q_PROPERTY(QString savedSelection READ savedSelection NOTIFY stateChanged)
    Q_PROPERTY(QString startupReason READ startupReason NOTIFY stateChanged)
    Q_PROPERTY(bool draftDirty READ draftDirty NOTIFY stateChanged)
    Q_PROPERTY(QString closeDialog READ closeDialog NOTIFY stateChanged)
    Q_PROPERTY(QString closeRevision READ closeRevision NOTIFY stateChanged)
    Q_PROPERTY(QString resetToken READ resetToken NOTIFY stateChanged)
    Q_PROPERTY(bool fullscreenPreference READ fullscreenPreference NOTIFY stateChanged)
    Q_PROPERTY(bool closing READ closing NOTIFY stateChanged)
    Q_PROPERTY(int screenAvailableWidth READ screenAvailableWidth NOTIFY screenGeometryChanged)
    Q_PROPERTY(int screenAvailableHeight READ screenAvailableHeight NOTIFY screenGeometryChanged)

public:
    explicit FuramiBridge(rust::Box<RuntimeCoordinator> gate);
    ~FuramiBridge() override;

    QWindow *hostWindow() const;
    QString phase() const;
    QString diagnostic() const;
    bool paused() const;
    bool presentationSuppressed() const;
    int volumePercent() const;
    bool muted() const;
    bool canTogglePause() const;
    bool canSetGain() const;
    QString playbackStatus() const;
    QString productPhase() const;
    QString recoveryEvidence() const;
    QString recoveryStage() const;
    QString recoveryCandidates() const;
    bool textEntryActive() const;
    bool panelVisible() const;
    bool popupOpen() const;
    bool captureSelected() const;
    bool canOpen() const;
    bool canRestart() const;
    QString restartGeneration() const;
    QString audioStatus() const;
    QString audioDiagnostic() const;
    QString audioSource() const;
    QString audioDesired() const;
    QString outputRows() const;
    QString outputCatalogRevision() const;
    QString outputSelected() const;
    QString outputSelectedKey() const;
    QString outputEffective() const;
    QString outputStatus() const;
    bool outputNeedsAction() const;
    QString settingsStatus() const;
    QString settingsPath() const;
    bool settingsRefused() const;
    QString savedSelection() const;
    QString startupReason() const;
    bool draftDirty() const;
    QString closeDialog() const;
    QString closeRevision() const;
    QString resetToken() const;
    bool fullscreenPreference() const;
    bool closing() const;
    int screenAvailableWidth() const;
    int screenAvailableHeight() const;
    void setTextEntryActive(bool active);
    void setPanelVisible(bool visible);
    void setPopupOpen(bool open);

    bool bindRoot(QQuickWindow *root, QQuickItem *container, QString &diagnostic);
    bool enableQualificationInput(QString &diagnostic);
    Q_INVOKABLE bool quitAuthorized() const;
    bool launchFailed() const;

    Q_INVOKABLE void openCapture();
    Q_INVOKABLE void restartCapture(const QString &expectedGeneration);
    Q_INVOKABLE void reconnectCapture(const QString &expectedGeneration);
    Q_INVOKABLE void chooseRecovery(const QString &expectedGeneration, const QString &token);
    Q_INVOKABLE void closeCapture();
    Q_INVOKABLE void togglePause();
    Q_INVOKABLE void setVolume(int percent);
    Q_INVOKABLE void setMuted(bool muted);
    Q_INVOKABLE void selectOutput(const QString &rowKey, const QString &catalogRevision);
    Q_INVOKABLE void openAndCloseDuringOpeningForProof();
    Q_INVOKABLE void forceSurfaceLossForProof();
    Q_INVOKABLE void requestApplicationClose();
    Q_INVOKABLE void decideClose(bool discard, const QString &revision);
    Q_INVOKABLE void retrySave();
    Q_INVOKABLE void closeWithoutSave();
    Q_INVOKABLE void requestSettingsReset();
    Q_INVOKABLE void decideSettingsReset(const QString &token, bool confirmed);
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
    void screenGeometryChanged();

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
    void restoreFullscreenFocus();
    void logGeometry();
    void readQualificationInput();
    void observeFullscreenChoice();
    void synchronizeFullscreenChoice();
    void cancelFullscreenChoice(const char *reason);
    void watchScreenGeometry();

    rust::Box<RuntimeCoordinator> m_gate;
    QTimer m_pollTimer;
    QPointer<HostWindow> m_host;
    QPointer<QSocketNotifier> m_qualificationInput;
    QByteArray m_qualificationLine;
    int m_stdinFlags = -1;
    bool m_dropQualificationLine = false;
    QPointer<QQuickWindow> m_root;
    QPointer<QQuickItem> m_container;
    QString m_actualPhase = QStringLiteral("Idle");
    QRect m_windowedGeometry;
    QWindow::Visibility m_windowedVisibility = QWindow::Windowed;
    QPointer<QQuickItem> m_fullscreenFocusItem;
    bool m_fullscreenNativeFocus = false;
    bool m_fullscreenFocusRestorePending = false;
    bool m_fullscreenNativeTransitionObserved = false;
    bool m_fullscreenObserved = false;
    bool m_fullscreenRequested = false;
    bool m_fullscreenChoiceSynchronized = false;
    QTimer m_fullscreenChoiceDeadline;
    std::uint64_t m_fullscreenTransition = 0;
    QMetaObject::Connection m_fullscreenSyncConnection;
    QMetaObject::Connection m_fullscreenSwapConnection;
    QString m_diagnostic;
    QString m_nativeDiagnostic;
    std::uint64_t m_generation = 0;
    std::uint64_t m_restartGeneration = 0;
    bool m_canRestart = false;
    bool m_canOpen = false;
    QString m_audioStatus = QStringLiteral("Disabled");
    QString m_audioDiagnostic;
    QString m_audioSource;
    QString m_audioDesired;
    QString m_productPhase = QStringLiteral("Stopped");
    QString m_recoveryEvidence;
    QString m_recoveryStage;
    QString m_recoveryCandidates;
    QMetaObject::Connection m_screenGeometryConnection;
    std::uint64_t m_nativeGeneration = 0;
    std::uint64_t m_rootXid = 0;
    std::uint64_t m_lastHostXid = 0;
    std::uint64_t m_closeDuringOpening = 0;
    bool m_closeOnNextOpening = false;
    bool m_failed = false;
    bool m_captureSelected = false;
    bool m_bootstrapFailed = false;
    bool m_paused = false;
    // Per-host latch: a prepared-paused owner is never shown, even during
    // Resume validation/teardown. Only its fresh Live successor may present.
    bool m_presentationSuppressed = false;
    int m_volumePercent = 100;
    bool m_muted = false;
    QString m_outputRows;
    std::uint64_t m_outputCatalogRevision = 0;
    QString m_outputSelected = QStringLiteral("auto");
    QString m_outputSelectedKey = QStringLiteral("auto");
    QString m_outputEffective;
    QString m_outputStatus;
    bool m_outputNeedsAction = false;
    bool m_canTogglePause = false;
    bool m_canSetGain = false;
    QString m_playbackStatus = QStringLiteral("Unavailable");
    QString m_settingsStatus;
    QString m_settingsPath;
    bool m_settingsRefused = false;
    QString m_savedSelection;
    QString m_startupReason;
    bool m_draftDirty = false;
    QString m_closeDialog;
    std::uint64_t m_closeRevision = 0;
    std::uint64_t m_resetToken = 0;
    bool m_fullscreenPreference = false;
    bool m_fullscreenChoicePending = false;
    bool m_closing = false;
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

LaunchResult run_qt_application(rust::Box<RuntimeCoordinator> gate, rust::Str display, bool qualificationStdin);

} // namespace furami::bridge

#endif // FURAMI_NATIVE_HOST_HOST_H
