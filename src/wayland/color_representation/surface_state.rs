//! Per-`WlSurface` color-representation state.
//!
//! Two pieces of state per surface that has a `wp_color_representation_surface_v1`:
//!
//! 1. [`ColorRepresentationSurfaceCachedState`] — double-buffered alpha mode +
//!    color coefficients + range + chroma location. Goes through smithay's
//!    [`compositor::Cacheable`] so `set_alpha_mode` / `set_coefficients_and_range`
//!    / `set_chroma_location` write into `pending` and `wl_surface.commit`
//!    snapshots to `current`.
//! 2. [`ColorRepresentationSurfaceData`] — non-double-buffered marker so the
//!    manager's `surface_exists` error can fire when a client tries to bind a
//!    second `wp_color_representation_surface_v1` to the same surface.
//!
//! Unlike `wp_color_management_v1` there are no destructor events emitted from
//! dispatch handlers, so no flush_pending machinery is needed.

use std::sync::atomic::{AtomicBool, Ordering};

use wayland_protocols::wp::color_representation::v1::server::wp_color_representation_surface_v1::{
    AlphaMode, ChromaLocation, Coefficients, Range,
};
use wayland_server::DisplayHandle;

use crate::wayland::compositor::Cacheable;

/// Per-surface double-buffered color-representation state.
///
/// Each field is `Option<T>` because clients aren't required to set every
/// property — `None` means "the client hasn't said". A renderer that needs the
/// information should fall back to format-implicit defaults (typically
/// premultiplied alpha + BT.709 + Full + Type 0 for RGB pixel formats).
#[derive(Debug, Clone, Default)]
pub struct ColorRepresentationSurfaceCachedState {
    /// Alpha pre-multiplication mode the client claims its buffer uses.
    pub alpha_mode: Option<AlphaMode>,
    /// YCbCr color coefficients (BT.709, BT.2020, etc.). Only meaningful for
    /// YUV pixel formats.
    pub coefficients: Option<Coefficients>,
    /// YCbCr value range (full vs limited). Only meaningful for YUV.
    pub range: Option<Range>,
    /// Sub-sampling chroma location. Only meaningful for sub-sampled YUV
    /// (4:2:0 / 4:2:2).
    pub chroma_location: Option<ChromaLocation>,
}

impl Cacheable for ColorRepresentationSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        self.clone()
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Per-surface state for the `wp_color_representation_surface_v1` resource
/// lifecycle. Currently just a boolean tracking whether one is bound, so the
/// manager's `surface_exists` error can fire on a duplicate `get_surface` call.
#[derive(Debug)]
pub struct ColorRepresentationSurfaceData {
    is_resource_attached: AtomicBool,
}

impl ColorRepresentationSurfaceData {
    /// Create a fresh marker with the resource flag clear.
    pub fn new() -> Self {
        Self {
            is_resource_attached: AtomicBool::new(false),
        }
    }

    /// Atomically check-and-set the attached flag. Returns `true` if a resource
    /// was already attached (the caller must post `surface_exists`), `false` if
    /// the slot was free and this call took it.
    pub fn is_resource_attached_or_attach(&self) -> bool {
        self.is_resource_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    }

    /// Clear the attached flag. Called from the resource's `destroyed` handler.
    pub fn detach(&self) {
        self.is_resource_attached.store(false, Ordering::Release);
    }
}

impl Default for ColorRepresentationSurfaceData {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_state_default_all_none() {
        let state = ColorRepresentationSurfaceCachedState::default();
        assert!(state.alpha_mode.is_none());
        assert!(state.coefficients.is_none());
        assert!(state.range.is_none());
        assert!(state.chroma_location.is_none());
    }

    #[test]
    fn surface_data_attach_one_shot() {
        let data = ColorRepresentationSurfaceData::new();
        assert!(!data.is_resource_attached_or_attach()); // first attach succeeds
        assert!(data.is_resource_attached_or_attach()); // second sees already-attached
        data.detach();
        assert!(!data.is_resource_attached_or_attach()); // after detach, attach again
    }
}
