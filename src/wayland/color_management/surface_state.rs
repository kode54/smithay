//! Per-`WlSurface` color-management state.
//!
//! Two kinds of state live on each surface that has a `wp_color_management_surface_v1`
//! attached:
//!
//! 1. [`ColorManagementSurfaceCachedState`] — the double-buffered "what description
//!    has the client requested" record. Goes through smithay's
//!    [`compositor::Cacheable`] machinery so `set_image_description` writes into
//!    `pending` and `wl_surface.commit` snapshots `pending` into `current`. This
//!    is the spec-correct double-buffer Hyprland's implementation skips.
//! 2. [`ColorManagementSurfaceData`] — non-double-buffered surface-management state.
//!    Right now just an atomic flag so the manager's `surface_exists` error can
//!    fire when a client tries to bind a second `wp_color_management_surface_v1`
//!    to the same surface.
//!
//! Render-intent is double-buffered alongside the description. A surface with no
//! description yet has [`ColorManagementSurfaceCachedState::description`] = `None`;
//! consumers should fall back to the default sRGB description from
//! [`crate::wayland::color_management::ImageDescriptionInterner::srgb_default`].

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

use wayland_protocols::wp::color_management::v1::server::wp_color_manager_v1::RenderIntent;
use wayland_server::DisplayHandle;

use super::ImageDescription;
use crate::wayland::compositor::Cacheable;

/// Per-surface double-buffered color-management state.
///
/// Written into `pending` by `wp_color_management_surface_v1.set_image_description`
/// (and `unset_image_description`); snapshotted into `current` by
/// `wl_surface.commit` via the [`Cacheable`] impl below.
///
/// `description = None` means the client has no description set on this surface;
/// the compositor should treat it as the default sRGB description.
#[derive(Debug, Clone)]
pub struct ColorManagementSurfaceCachedState {
    /// The description the client most recently set, or `None` if it has been
    /// unset (or was never set).
    pub description: Option<Arc<ImageDescription>>,
    /// Render intent paired with `description`. Meaningless when `description` is
    /// `None`. Defaults to [`RenderIntent::Perceptual`] which is the only intent
    /// every implementation is required to support.
    pub render_intent: RenderIntent,
}

impl ColorManagementSurfaceCachedState {
    /// Create state with no description (the default).
    pub fn new() -> Self {
        Self {
            description: None,
            render_intent: RenderIntent::Perceptual,
        }
    }
}

impl Default for ColorManagementSurfaceCachedState {
    fn default() -> Self {
        Self::new()
    }
}

impl Cacheable for ColorManagementSurfaceCachedState {
    fn commit(&mut self, _dh: &DisplayHandle) -> Self {
        // Snapshot pending → current. We clone the `Arc` (cheap) rather than move
        // the description out, so a future `set_image_description` followed by a
        // *second* commit before any other change still sees the correct value
        // in pending.
        self.clone()
    }

    fn merge_into(self, into: &mut Self, _dh: &DisplayHandle) {
        *into = self;
    }
}

/// Per-surface state for color-management resource lifetimes.
///
/// Currently just a boolean tracking whether a `wp_color_management_surface_v1` is
/// bound to this surface. `wp_color_manager_v1.get_surface` posts the
/// `surface_exists` protocol error if one already is.
///
/// Created lazily when a client first queries it via
/// [`Self::is_resource_attached_or_attach`].
#[derive(Debug)]
pub struct ColorManagementSurfaceData {
    is_resource_attached: AtomicBool,
}

impl ColorManagementSurfaceData {
    /// Create a fresh marker with the resource flag clear.
    pub fn new() -> Self {
        Self {
            is_resource_attached: AtomicBool::new(false),
        }
    }

    /// Atomically check-and-set the attached flag.
    ///
    /// Returns `true` if a resource was already attached (caller must post the
    /// `surface_exists` error and reject the request); returns `false` if this
    /// call took the slot (caller can proceed to construct the resource).
    pub fn is_resource_attached_or_attach(&self) -> bool {
        // compare_exchange returns Err(prev) if the prev value didn't match the
        // expected; that means it was already true → already attached.
        self.is_resource_attached
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
    }

    /// Clear the attached flag. Called from the resource's `destroyed` handler so
    /// the surface can be re-attached after the previous resource is gone.
    pub fn detach(&self) {
        self.is_resource_attached.store(false, Ordering::Release);
    }
}

impl Default for ColorManagementSurfaceData {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_state_default_has_no_description() {
        let state = ColorManagementSurfaceCachedState::new();
        assert!(state.description.is_none());
        assert_eq!(state.render_intent, RenderIntent::Perceptual);
    }

    #[test]
    fn surface_data_attach_is_one_shot_until_detach() {
        let data = ColorManagementSurfaceData::new();

        // First attach succeeds (returns false = "was not attached before").
        assert!(!data.is_resource_attached_or_attach());

        // Second attempt sees it already attached.
        assert!(data.is_resource_attached_or_attach());

        // After detach, attach succeeds again.
        data.detach();
        assert!(!data.is_resource_attached_or_attach());
    }
}
