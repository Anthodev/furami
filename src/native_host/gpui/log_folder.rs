//! GPUI log-folder effect.
//!
//! The runtime prepares and authorizes one folder request; the host claims that
//! delivery edge exactly once and hands the claimed `(request, path)` pair here.
//! This module never claims again: it encodes the absolute path, launches one
//! independent `xdg-open` child, and reports the launcher lifecycle through
//! `HostState::consume` on the GUI thread.
//!
//! A rejected launch (relative path or no child) reports only
//! `log_folder_result(accepted: false)`. A created child reports
//! `log_folder_result(accepted: true)` and then exactly one
//! `log_folder_finished(failed)`. Nothing here waits on the child from the GUI
//! owner, and dropping the task never kills the independent launcher.

use std::{
    os::unix::process::CommandExt,
    process::{Child, Command, Stdio},
};

use gpui::{AsyncApp, Context, WeakEntity};
use url::Url;

use super::HostView;

/// Launch-environment overrides a packaged launcher may inject: the dynamic
/// library path and the Qt/QML search paths. They are removed from the opener
/// child only, so a system tool never resolves Furami's private libraries.
/// PATH, the session bus, display and XDG variables keep the inherited values.
const SCRUBBED_ENV: [&str; 5] = [
    "LD_LIBRARY_PATH",
    "QT_PLUGIN_PATH",
    "QT_QPA_PLATFORM_PLUGIN_PATH",
    "QML_IMPORT_PATH",
    "QML2_IMPORT_PATH",
];

enum Report {
    /// Whether a child process was created. Rejections get no terminal report
    /// because no launcher exists to observe.
    Result { accepted: bool },
    /// Exactly one per accepted launcher, after its child exits.
    Finished { failed: bool },
}

/// Schedule the already-claimed request with `path`.
///
/// Must be called exactly once per successful
/// `RuntimeCoordinator::claim_log_folder_dispatch`, after that claim returned
/// true. The module reports through `HostState::consume`; a spawn or encoding
/// failure is reported as a rejection and never schedules a terminal report.
pub(super) fn launch(cx: &mut Context<HostView>, request: u64, path: String) {
    let background = cx.background_executor().clone();
    cx.spawn(async move |view, cx| {
        let Some(url) = file_url(&path) else {
            report(&view, cx, request, Report::Result { accepted: false });
            return;
        };
        // Spawn off the GUI owner; only a created child can terminate.
        let Ok(child) = background.spawn(async move { spawn(url) }).await else {
            report(&view, cx, request, Report::Result { accepted: false });
            return;
        };
        report(&view, cx, request, Report::Result { accepted: true });
        // Reap off the GUI owner. The child is independent: dropping this task
        // (for example when Furami exits) drops the handle without killing it.
        let failed = background.spawn(async move { wait(child) }).await;
        report(&view, cx, request, Report::Finished { failed });
    })
    .detach();
}

fn report(view: &WeakEntity<HostView>, cx: &mut AsyncApp, request: u64, event: Report) {
    // The foreground executor only polls while the app is alive; an exited
    // host no longer needs a report.
    let _ = view.update(cx, |view, _| {
        let mut state = view.state.borrow_mut();
        let update = match event {
            Report::Result { accepted } => state.runtime.log_folder_result(request, accepted),
            Report::Finished { failed } => state.runtime.log_folder_finished(request, failed),
        };
        state.consume(update);
    });
}

/// The single `file:` argument for the launcher, percent-encoded so spaces and
/// delimiters reach `xdg-open` verbatim. Only absolute paths encode.
fn file_url(path: &str) -> Option<String> {
    Url::from_file_path(path).ok().map(|url| url.to_string())
}

/// `xdg-open <file-url>`, with null streams and a new session for the child.
fn command(url: &str) -> Command {
    let mut command = Command::new("xdg-open");
    command
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for key in SCRUBBED_ENV {
        command.env_remove(key);
    }
    // SAFETY: this hook runs between fork and exec, where only async-signal-safe
    // calls are permitted. `setsid` and errno access are safe there and the
    // closure allocates nothing.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    command
}

fn spawn(url: String) -> Result<Child, ()> {
    command(&url).spawn().map_err(|_| ())
}

/// Wait off the GUI owner; any failure to observe a clean zero exit is a
/// launcher failure. The runtime never treats success as proof of a window.
fn wait(mut child: Child) -> bool {
    match child.wait() {
        Ok(status) => !status.success(),
        Err(_) => true,
    }
}

#[cfg(test)]
mod tests {
    use std::{ffi::OsStr, process::Command};

    use super::{SCRUBBED_ENV, file_url, wait};

    #[test]
    fn absolute_paths_become_one_encoded_file_url() {
        assert_eq!(
            file_url("/home/anthodev/furami logs#1").as_deref(),
            Some("file:///home/anthodev/furami%20logs%231")
        );
        assert_eq!(file_url("/var/log").as_deref(), Some("file:///var/log"));
        assert_eq!(file_url("relative/logs"), None);
    }

    #[test]
    fn launcher_is_exactly_xdg_open_with_the_url_and_child_only_scrubbing() {
        let command = super::command("file:///home/anthodev/furami%20logs");
        assert_eq!(command.get_program(), OsStr::new("xdg-open"));
        let args: Vec<&OsStr> = command.get_args().collect();
        assert_eq!(args, [OsStr::new("file:///home/anthodev/furami%20logs")]);
        let scrubbed: Vec<_> = command
            .get_envs()
            .map(|(key, value)| (key.to_os_string(), value.map(OsStr::to_os_string)))
            .collect();
        for key in SCRUBBED_ENV {
            assert!(
                scrubbed
                    .iter()
                    .any(|(name, value)| name.as_os_str() == OsStr::new(key) && value.is_none()),
                "{key} must be removed from the launcher child"
            );
        }
    }

    #[test]
    fn wait_reports_clean_and_failed_exits() {
        let clean = Command::new("/bin/true").spawn().expect("/bin/true");
        assert!(!wait(clean));
        let failed = Command::new("/bin/false").spawn().expect("/bin/false");
        assert!(wait(failed));
    }
}
