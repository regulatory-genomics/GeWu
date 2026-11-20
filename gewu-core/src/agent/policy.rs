//! Execution-time restrictions on an agent's advertised tools.
//!
//! Tools stay in the tool list so request prefixes (and provider caches)
//! remain stable across modes; a policy only refuses to *execute* the tools
//! a mode must not use. Policy values are host configuration: this type is
//! intentionally mode-agnostic, so new modes are data, not new code.

use std::collections::BTreeSet;

/// Which advertised tools may not run, and the message shown when one is
/// refused.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    denied: BTreeSet<String>,
    reason: Option<String>,
}

impl ToolPolicy {
    /// No restrictions.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Refuses `names`, answering with `reason` when one is called.
    pub fn deny(
        names: impl IntoIterator<Item = impl Into<String>>,
        reason: impl Into<String>,
    ) -> Self {
        Self {
            denied: names.into_iter().map(Into::into).collect(),
            reason: Some(reason.into()),
        }
    }

    /// Whether `name` is advertised but refused.
    pub fn is_denied(&self, name: &str) -> bool {
        self.denied.contains(name)
    }

    /// The refusal message for `name`, or `Ok(())` when it may run.
    pub fn check(&self, name: &str) -> Result<(), String> {
        if !self.is_denied(name) {
            return Ok(());
        }
        Err(self
            .reason
            .clone()
            .unwrap_or_else(|| format!("{name} is not available in the current mode.")))
    }

    /// Names refused by this policy.
    pub fn denied(&self) -> impl Iterator<Item = &str> {
        self.denied.iter().map(String::as_str)
    }
}
