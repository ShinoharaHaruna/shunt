//! The `--as` label: who is speaking/reading (§17.3).

use crate::proto::task::sanitize_slug;
use crate::server::registry::names::{self, Resolution};
use crate::server::registry::valid_name;
use anyhow::{Context, Result};

use super::{is_admin, ADMIN_LABEL};

/// The caller's `--as` label: author of messages AND key of the
/// readers/ cursor — you read under the name you speak under
/// (§17.3). Resolution: explicit `--as` > `admin`. The value is
/// sanitized like a §3.3 slug, then must also be a valid registry
/// name (it becomes `readers/<label>.json` — a `..` must not
/// escape), then run through the registry name middleware (§4.1):
/// `admin` in any case folds to `admin`, and a unique case-fold hit
/// against registered projects substitutes the canonical spelling
/// silently — membership checks, `author`, and the cursor all agree
/// on who this is. Two registered projects differing only by case
/// make the label ambiguous — a loud error, not a coin flip; a
/// label matching nothing is free-form and passes through.
pub(crate) fn resolve_label(as_flag: Option<&str>) -> Result<String> {
    let raw = as_flag.unwrap_or(ADMIN_LABEL);
    let label = sanitize_slug(raw);
    valid_name(&label).with_context(|| format!("bad --as label {raw:?} (sanitized to {label:?}): use letters, digits, '_', '.', '-' — your project name is the convention"))?;
    if is_admin(&label) {
        return Ok(ADMIN_LABEL.to_string());
    }
    match names::resolve_project(&label)? {
        Resolution::Exact(p) | Resolution::Canonical { canonical: p, .. } => Ok(p),
        Resolution::Ambiguous(projects) => anyhow::bail!("--as label {label:?} is ambiguous: registered projects {} differ only by case — rename one", projects.join(", ")),
        // Free-form labels stay legal (§17.3): a non-member warning
        // still fires downstream where the roster is known.
        Resolution::Unknown { .. } => Ok(label),
    }
}

/// A marking op's label — same resolution; the name keeps call
/// sites honest about why an identity is required (§17.3).
pub(crate) fn required_label(as_flag: Option<&str>) -> Result<String> {
    resolve_label(as_flag)
}
