//! Library half of the hub updater (the `activity-server-update` binary is
//! a thin CLI over this). Split so the flow tests (tests/flow.rs) can wire
//! fakes into every dependency: no podman, no containers, no network.

pub mod logging;
pub mod podman;
pub mod state;
pub mod systemd;
pub mod updater;
