//! Integration tests for gdbg commands against realistic GPU profiling sessions.
//!
//! Split into sub-modules by concern: fixtures (session builders),
//! smoke (no-panic command tests), invariants (cross-command numerical
//! consistency), and parsers (parser-level and edge-case tests).

mod fixtures;
mod invariants;
mod parsers;
mod regressions;
mod smoke;
