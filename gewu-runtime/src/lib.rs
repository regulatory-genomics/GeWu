//! Minimal specialist runtime.
//!
//! A specialist is a *declaration*: an id, a prompt, an exact tool allowlist and
//! per-run budgets. This crate turns that declaration into a [`gewu_core::Agent`]
//! from tools the caller already registered.
//!
//! It deliberately contains no tool implementations, no storage, no HTTP and no
//! knowledge of any particular application. That is what lets a host ship its own
//! `specialist.json` without pulling a framework's CLI or web UI into the build.

pub mod specialist;

pub use specialist::{
    SpecialistBuild, SpecialistDescriptor, SpecialistError, SpecialistRegistry,
};
