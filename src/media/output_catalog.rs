//! Application-lifetime output catalog. All Pulse waits stay on this worker.
//! Removal evidence is sticky across coalesced catalog publications.

use crate::{
    capture::linux::pulse::OutputSubscription,
    domain::{
        capture::AudioError,
        output::{LiveSinkTarget, SinkCatalog},
    },
};
use parking_lot::Mutex;
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::{self, JoinHandle},
    time::Duration,
};

#[derive(Clone, Debug)]
pub struct CatalogObservation {
    pub catalog: Result<SinkCatalog, AudioError>,
    pub selected_target_removed: bool,
}

struct Shared {
    stop: Arc<AtomicBool>,
    selected: Mutex<Option<LiveSinkTarget>>,
    latest: Mutex<Option<Result<SinkCatalog, AudioError>>>,
    removed: AtomicBool,
}

/// Value-only, nonblocking supervisor; no Pulse object crosses this boundary.
#[doc(hidden)]
pub struct OutputCatalogWatch {
    shared: Arc<Shared>,
    worker: Option<JoinHandle<Result<(), AudioError>>>,
    #[cfg(test)]
    delayed_release: Option<(u32, std::sync::mpsc::Sender<()>)>,
}

impl OutputCatalogWatch {
    pub fn start() -> Result<Self, AudioError> {
        let shared = Arc::new(Shared {
            stop: Arc::new(AtomicBool::new(false)),
            selected: Mutex::new(None),
            latest: Mutex::new(None),
            removed: AtomicBool::new(false),
        });
        let state = Arc::clone(&shared);
        let worker = thread::Builder::new()
            .name("output-catalog".into())
            .spawn(move || {
                let mut result = watch(&state);
                if result == Err(AudioError::Cancelled) && state.stop.load(Ordering::Acquire) {
                    result = Ok(());
                }
                if let Err(error) = &result {
                    *state.latest.lock() = Some(Err(error.clone()));
                }
                result
            })
            .map_err(|error| AudioError::Unavailable(format!("output catalog worker: {error}")))?;
        Ok(Self {
            shared,
            worker: Some(worker),
            #[cfg(test)]
            delayed_release: None,
        })
    }

    pub fn set_selected_target(&self, target: Option<LiveSinkTarget>) {
        // New registration (including explicit reselect of the same target).
        // Call for an actual target registration change or explicit selection,
        // never merely a silence/revision change. Retain the last watched live
        // target through catalog errors so its removal cannot be coalesced away.
        // Worker loss publication holds this same mutex, so stale registration
        // evidence cannot be written after the new registration's reset.
        let mut selected = self.shared.selected.lock();
        self.shared.removed.store(false, Ordering::Release);
        *selected = target;
    }

    pub fn poll(&self) -> Option<CatalogObservation> {
        let catalog = self.shared.latest.lock().take()?;
        // Do not consume removal while its corresponding snapshot is still in
        // flight. Error snapshots also carry the sticky evidence explicitly.
        let removed = self.shared.removed.swap(false, Ordering::AcqRel);
        Some(CatalogObservation {
            catalog,
            selected_target_removed: removed,
        })
    }

    pub fn stop(&self) {
        self.shared.stop.store(true, Ordering::Release);
    }

    pub fn try_join(&mut self) -> Option<Result<(), AudioError>> {
        #[cfg(test)]
        if let Some((remaining, _)) = self.delayed_release.as_mut() {
            if *remaining > 0 {
                *remaining -= 1;
            } else if let Some((_, release)) = self.delayed_release.take() {
                let _ = release.send(());
            }
        }
        if !self.worker.as_ref()?.is_finished() {
            return None;
        }
        Some(
            self.worker
                .take()
                .expect("checked worker")
                .join()
                .map_err(|_| AudioError::Control("output catalog worker panicked".into()))
                .and_then(|outcome| outcome),
        )
    }

    /// Native-free close fixture: first `polls` join attempts leave a real worker
    /// behind its channel barrier; subsequent attempts use the ordinary join.
    #[cfg(test)]
    pub(crate) fn for_test_delayed_releases(polls: u32) -> Self {
        let shared = Arc::new(Shared {
            stop: Arc::new(AtomicBool::new(false)),
            selected: Mutex::new(None),
            latest: Mutex::new(None),
            removed: AtomicBool::new(false),
        });
        let (release, barrier) = std::sync::mpsc::channel();
        let (started, waiting) = std::sync::mpsc::sync_channel(0);
        let worker = thread::spawn(move || {
            started
                .send(())
                .map_err(|_| AudioError::Control("test catalog startup barrier closed".into()))?;
            barrier.recv().map_err(|_| {
                AudioError::Control("test catalog retirement barrier closed".into())
            })?;
            Ok(())
        });
        // The worker has started and cannot finish until its real receiver is
        // released. No counter fabricates a pending or successful join result.
        waiting.recv().expect("test catalog worker started");
        Self {
            shared,
            worker: Some(worker),
            delayed_release: Some((polls, release)),
        }
    }
}

impl Drop for OutputCatalogWatch {
    fn drop(&mut self) {
        self.stop();
        #[cfg(test)]
        if let Some((_, release)) = self.delayed_release.take() {
            let _ = release.send(());
        }
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn selected_lost(
    target: &LiveSinkTarget,
    removed: &std::collections::BTreeSet<u32>,
    catalog: &SinkCatalog,
) -> bool {
    removed.contains(&target.pulse_index) || !catalog.sinks.iter().any(|row| row.target == *target)
}

fn watch(shared: &Shared) -> Result<(), AudioError> {
    let mut pulse = OutputSubscription::connect(Arc::clone(&shared.stop))?;
    let mut revision = 0u64;
    let mut previous = None;
    while !shared.stop.load(Ordering::Acquire) {
        let dirty = pulse.poll_dirty()?;
        let removed = pulse.take_removed();
        if let Some(selected) = shared.selected.lock().as_ref()
            && removed.contains(&selected.pulse_index)
        {
            shared.removed.store(true, Ordering::Release);
        }
        if dirty {
            revision = revision
                .checked_add(1)
                .ok_or_else(|| AudioError::Control("output catalog revision exhausted".into()))?;
            let catalog = pulse.snapshot(revision);
            let during_scan = pulse.take_removed();
            if let Some(selected) = shared.selected.lock().as_ref() {
                let lost = during_scan.contains(&selected.pulse_index)
                    || catalog
                        .as_ref()
                        .is_ok_and(|catalog| selected_lost(selected, &removed, catalog));
                if lost {
                    shared.removed.store(true, Ordering::Release);
                }
            }
            if let Ok(catalog) = &catalog {
                previous = Some(catalog.clone());
            }
            *shared.latest.lock() = Some(catalog);
        } else if let (Some(selected), Some(catalog)) =
            (shared.selected.lock().as_ref(), previous.as_ref())
            && selected_lost(selected, &removed, catalog)
        {
            shared.removed.store(true, Ordering::Release);
            let mut latest = shared.latest.lock();
            if latest.is_none() {
                *latest = Some(Ok(catalog.clone()));
            }
        }
        thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::output::{SinkIdentity, SinkObservation};
    use std::{collections::BTreeSet, num::NonZeroU64};

    fn target(serial: u64) -> LiveSinkTarget {
        LiveSinkTarget::new(
            SinkIdentity::new("chosen".into(), vec![]).unwrap(),
            NonZeroU64::new(serial).unwrap(),
            7,
        )
        .unwrap()
    }

    #[test]
    fn remove_return_in_one_snapshot_still_reports_selected_loss() {
        let selected = target(50);
        let catalog = SinkCatalog {
            revision: 2,
            sinks: vec![SinkObservation {
                target: selected.clone(),
                description: "chosen".into(),
                eligible: true,
            }],
            default_sink: None,
        };
        assert!(selected_lost(&selected, &BTreeSet::from([7]), &catalog));
        assert!(!selected_lost(&selected, &BTreeSet::from([9]), &catalog));
    }

    #[test]
    fn replacement_at_same_index_does_not_restore_old_live_target() {
        let catalog = SinkCatalog {
            revision: 3,
            sinks: vec![SinkObservation {
                target: target(51),
                description: "chosen".into(),
                eligible: true,
            }],
            default_sink: None,
        };
        assert!(selected_lost(&target(50), &BTreeSet::new(), &catalog));
    }

    #[test]
    fn sticky_removal_waits_for_publication_and_new_registration_clears_old_evidence() {
        let watch = OutputCatalogWatch {
            shared: Arc::new(Shared {
                stop: Arc::new(AtomicBool::new(false)),
                selected: Mutex::new(Some(target(50))),
                latest: Mutex::new(None),
                removed: AtomicBool::new(true),
            }),
            worker: None,
            delayed_release: None,
        };
        assert!(watch.poll().is_none());
        assert!(watch.shared.removed.load(Ordering::Acquire));
        *watch.shared.latest.lock() = Some(Ok(SinkCatalog {
            revision: 3,
            sinks: vec![],
            default_sink: None,
        }));
        assert!(watch.poll().unwrap().selected_target_removed);
        watch.shared.removed.store(true, Ordering::Release);
        watch.set_selected_target(Some(target(51)));
        assert!(!watch.shared.removed.load(Ordering::Acquire));
        assert_eq!(*watch.shared.selected.lock(), Some(target(51)));
    }
}
