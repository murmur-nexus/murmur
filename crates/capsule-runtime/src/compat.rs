//! Backward-compatibility shims for retired interface versions.
//!
//! Every shim here is registered as one row in `COMPAT_SHIMS.md` at the repo
//! root, which is the authoritative inventory — this module only holds the
//! code. See that file before adding or removing anything under this module.

pub(crate) mod lifecycle_v0_2;
pub(crate) mod lifecycle_v0_3;
pub(crate) mod lifecycle_v0_4;
