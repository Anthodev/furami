//! Screen inhibition is an edge-triggered desktop effect, not an owner barrier.

use std::{
    future::{Future, poll_fn},
    pin::pin,
    sync::mpsc::{self, Receiver, TryRecvError},
    task::Poll,
    time::Duration,
};

use gpui::BackgroundExecutor;
use zbus::Connection;

const SERVICE: &str = "org.freedesktop.ScreenSaver";
const PATH: &str = "/org/freedesktop/ScreenSaver";
const BUS_TIMEOUT: Duration = Duration::from_secs(5);

struct Cookie {
    connection: Connection,
    value: u32,
}

pub(super) struct IdleInhibitor {
    background: BackgroundExecutor,
    state: Eligibility<Cookie>,
    pending: Option<Receiver<Option<Cookie>>>,
}

impl IdleInhibitor {
    pub(super) fn new(background: BackgroundExecutor) -> Self {
        Self {
            background,
            state: Eligibility::default(),
            pending: None,
        }
    }

    /// Apply the latest checked owner/native facts and drain any acquisition
    /// result. Call this on the GUI owner's tick even if eligibility is stable.
    pub(super) fn reconcile(&mut self, wanted: bool) {
        let change = self.state.reconcile(wanted, self.pending.is_some());
        self.apply(change);
        let Some(pending) = &self.pending else {
            return;
        };
        let cookie = match pending.try_recv() {
            Ok(cookie) => cookie,
            Err(TryRecvError::Empty) => return,
            Err(TryRecvError::Disconnected) => None,
        };
        self.pending = None;
        let change = self.state.complete(cookie);
        self.apply(change);
    }

    fn apply(&mut self, change: Change<Cookie>) {
        match change {
            Change::None => {}
            Change::Acquire => self.acquire(),
            Change::Release(cookie) => self.release(cookie),
        }
    }

    fn acquire(&mut self) {
        let (tx, rx) = mpsc::channel();
        self.pending = Some(rx);
        let background = self.background.clone();
        self.background
            .spawn(async move {
                let cookie = match inhibit(&background).await {
                    Ok(cookie) => Some(cookie),
                    Err(error) => {
                        tracing::warn!(event_code = "display_idle_inhibit_failed", %error);
                        None
                    }
                };
                // If the GUI owner has exited, drop the issuing connection.
                let _ = tx.send(cookie);
            })
            .detach();
    }

    fn release(&self, cookie: Cookie) {
        let background = self.background.clone();
        self.background
            .spawn(async move {
                let result = bounded(&background, async {
                    let reply = cookie
                        .connection
                        .call_method(
                            Some(SERVICE),
                            PATH,
                            Some(SERVICE),
                            "UnInhibit",
                            &(cookie.value,),
                        )
                        .await?;
                    reply.body().deserialize::<()>()
                })
                .await;
                if let Err(error) = result {
                    tracing::warn!(event_code = "display_idle_uninhibit_failed", %error);
                }
                // The connection belongs to this cookie alone. Dropping it
                // also covers a failed or timed-out release.
            })
            .detach();
    }
}

async fn inhibit(background: &BackgroundExecutor) -> zbus::Result<Cookie> {
    // Limit a connection to one cookie so a malformed/timed-out reply can
    // disconnect an unknown remote inhibition without disturbing a later one.
    let connection = bounded(background, Connection::session()).await?;
    let value = bounded(background, async {
        let reply = connection
            .call_method(
                Some(SERVICE),
                PATH,
                Some(SERVICE),
                "Inhibit",
                &("dev.antho.furami", "Playing video"),
            )
            .await?;
        reply.body().deserialize::<u32>()
    })
    .await?;
    Ok(Cookie { connection, value })
}

async fn bounded<T>(
    background: &BackgroundExecutor,
    operation: impl Future<Output = zbus::Result<T>>,
) -> zbus::Result<T> {
    let mut deadline = pin!(background.timer(BUS_TIMEOUT));
    let mut operation = pin!(operation);
    poll_fn(|cx| {
        if deadline.as_mut().poll(cx).is_ready() {
            Poll::Ready(Err(zbus::Error::Failure(
                "D-Bus operation timed out".into(),
            )))
        } else {
            operation.as_mut().poll(cx)
        }
    })
    .await
}

struct Eligibility<T> {
    wanted: bool,
    deferred_edge: bool,
    cookie: Option<T>,
}

impl<T> Default for Eligibility<T> {
    fn default() -> Self {
        Self {
            wanted: false,
            deferred_edge: false,
            cookie: None,
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
enum Change<T> {
    None,
    Acquire,
    Release(T),
}

impl<T> Eligibility<T> {
    fn reconcile(&mut self, wanted: bool, pending: bool) -> Change<T> {
        let rising = wanted && !self.wanted;
        self.wanted = wanted;
        if !wanted {
            self.deferred_edge = false;
            return self.cookie.take().map_or(Change::None, Change::Release);
        }
        if rising {
            // Keep only the current eligibility epoch. A successful pending
            // call already satisfies it; failure leaves it one acquisition.
            self.deferred_edge = pending;
            if !pending && self.cookie.is_none() {
                return Change::Acquire;
            }
        }
        Change::None
    }

    fn complete(&mut self, cookie: Option<T>) -> Change<T> {
        debug_assert!(self.cookie.is_none());
        let deferred_edge = std::mem::take(&mut self.deferred_edge);
        match (self.wanted, cookie) {
            (false, cookie) => cookie.map_or(Change::None, Change::Release),
            (true, None) if deferred_edge => Change::Acquire,
            (true, cookie) => {
                self.cookie = cookie;
                Change::None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Change, Eligibility};

    #[test]
    fn gpui_host_idle_inhibitor_requires_a_new_edge_after_failure() {
        let mut state = Eligibility::default();
        assert_eq!(state.reconcile(true, false), Change::Acquire);
        assert_eq!(state.reconcile(true, true), Change::None);
        assert_eq!(state.complete(None::<u32>), Change::None);
        assert_eq!(state.reconcile(true, false), Change::None);
        assert_eq!(state.reconcile(false, false), Change::None);
        assert_eq!(state.reconcile(true, false), Change::Acquire);
    }

    #[test]
    fn gpui_host_idle_inhibitor_retires_zero_once() {
        let mut state = Eligibility::default();
        assert_eq!(state.reconcile(true, false), Change::Acquire);
        assert_eq!(state.complete(Some(0_u32)), Change::None);
        assert_eq!(state.reconcile(false, false), Change::Release(0));
        assert_eq!(state.reconcile(false, false), Change::None);
    }

    #[test]
    fn gpui_host_idle_inhibitor_returns_a_late_cookie_for_release() {
        let mut state = Eligibility::default();
        assert_eq!(state.reconcile(true, false), Change::Acquire);
        assert_eq!(state.reconcile(false, true), Change::None);
        assert_eq!(state.complete(Some(17_u32)), Change::Release(17));
        assert_eq!(state.reconcile(false, false), Change::None);
    }

    #[test]
    fn gpui_host_idle_inhibitor_uses_pending_acquisition_if_eligibility_restores() {
        let mut state = Eligibility::default();
        assert_eq!(state.reconcile(true, false), Change::Acquire);
        assert_eq!(state.reconcile(false, true), Change::None);
        assert_eq!(state.reconcile(true, true), Change::None);
        assert_eq!(state.complete(Some(23_u32)), Change::None);
        assert_eq!(state.reconcile(false, false), Change::Release(23));
    }

    #[test]
    fn gpui_host_idle_inhibitor_honors_one_deferred_edge_after_pending_failure() {
        let mut state = Eligibility::default();
        assert_eq!(state.reconcile(true, false), Change::Acquire);
        assert_eq!(state.reconcile(false, true), Change::None);
        assert_eq!(state.reconcile(true, true), Change::None);
        assert_eq!(state.complete(None::<u32>), Change::Acquire);
        assert_eq!(state.complete(None), Change::None);
        assert_eq!(state.reconcile(true, false), Change::None);
    }

    #[test]
    fn gpui_host_idle_inhibitor_drops_the_deferred_edge_when_eligibility_ends() {
        let mut state = Eligibility::default();
        assert_eq!(state.reconcile(true, false), Change::Acquire);
        assert_eq!(state.reconcile(false, true), Change::None);
        assert_eq!(state.reconcile(true, true), Change::None);
        assert_eq!(state.reconcile(false, true), Change::None);
        assert_eq!(state.complete(None::<u32>), Change::None);
    }
}
