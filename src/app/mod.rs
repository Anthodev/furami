//! Responsibility: Transitions, draft/active separation, apply and recovery.
//! Allowed dependencies: `domain`, narrow media/capture/persistence interfaces.

pub mod apply;
#[cfg(test)]
pub(crate) mod control;
pub(crate) mod diagnostic;
pub(crate) mod gate;
pub mod output;
pub mod ports;
pub mod settings;
