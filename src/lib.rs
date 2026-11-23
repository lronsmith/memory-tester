//! memory-tester: a tiny Windows RAM tester.
//!
//! Module map:
//! - [`types`]   shared data types (the contract between modules)
//! - [`engine`]  allocates memory and runs the write/verify test patterns
//! - [`smbios`]  reads which memory sticks are installed
//! - [`verdict`] turns a test result into a per-stick status
//! - [`history`] remembers per-stick results between runs

pub mod engine;
pub mod history;
pub mod smbios;
pub mod types;
pub mod verdict;
