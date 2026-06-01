//! asahi-brightness library crate.
//!
//! Splitting the modules out of the binary makes them importable by the binary,
//! by integration tests, and by `examples/` (notably the Phase 1 reactor, which
//! reuses `curve`/`output`/`config` instead of duplicating them). The modules
//! still reference each other via `crate::…`, which now resolves to this lib.

pub mod config;
pub mod curve;
pub mod ipc;
pub mod output;
pub mod ramp;
pub mod reactor;
pub mod sensor;
