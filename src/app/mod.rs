//! Responsibility: Transitions, draft/active separation, apply and recovery.
//! Allowed dependencies: `domain`, narrow media/capture/persistence interfaces.

pub mod apply;
pub(crate) mod control;
pub(crate) mod gate;
pub mod ports;
pub mod settings;
