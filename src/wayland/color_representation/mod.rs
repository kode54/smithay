//! Implementation of `wp_color_representation_v1` (staging, version 1).
//!
//! Adjacent protocol to [`color_management`](super::color_management). Where
//! `wp_color_management_v1` describes RGB color encoding (primaries + transfer
//! function + luminance), `wp_color_representation_v1` describes the **pixel
//! representation contract** — how the bytes in the buffer map to color values.
//! Specifically:
//!
//! - **Alpha mode** — premultiplied (electrical or optical) vs straight.
//! - **YCbCr coefficients** — BT.709 / BT.2020 / etc., used to convert YUV to RGB.
//! - **Range** — full vs limited (TV) range YUV.
//! - **Chroma location** — sub-sampling position for 4:2:0 / 4:2:2 formats.
//!
//! For RGB pixel formats only the alpha mode is meaningful; coefficients and
//! range are YUV-specific (videos, hardware-accelerated decode buffers).
//!
//! ### Compositor integration
//!
//! ```ignore
//! use smithay::wayland::color_representation::{
//!     ColorRepresentationState, ColorRepresentationCapabilities,
//! };
//! ColorRepresentationState::new::<Self>(&dh, ColorRepresentationCapabilities::conservative());
//! delegate_color_representation!(State);
//! ```
//!
//! Then at render time, read each surface's representation via
//! [`with_surface_color_representation`] to drive blend-equation choice +
//! YUV→RGB conversion + alpha handling.

use std::collections::HashSet;

use wayland_protocols::wp::color_representation::v1::server::{
    wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
    wp_color_representation_surface_v1::{
        AlphaMode, Coefficients, Range, WpColorRepresentationSurfaceV1,
    },
};
use wayland_server::{
    Dispatch, DisplayHandle, GlobalDispatch, Weak as WaylandWeak, backend::GlobalId,
    protocol::wl_surface::WlSurface,
};

pub mod dispatch;
pub mod surface_state;

pub use surface_state::{ColorRepresentationSurfaceCachedState, ColorRepresentationSurfaceData};

pub use dispatch::SurfaceResourceData;

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Builder for the set of `wp_color_representation_v1` features the compositor
/// advertises on bind.
///
/// The manager fans out one `supported_alpha_mode` per alpha mode and one
/// `supported_coefficients_and_ranges` per (coefficients × range) pair.
/// `supported_coefficients_and_ranges` is repeated per range, so a compositor
/// supporting BT.709 in both Full and Limited would send two events for BT.709.
#[derive(Debug, Clone)]
pub struct ColorRepresentationCapabilities {
    alpha_modes: HashSet<AlphaMode>,
    coefficient_range_pairs: HashSet<(Coefficients, Range)>,
}

impl ColorRepresentationCapabilities {
    /// An empty capability set — advertises nothing.
    pub fn empty() -> Self {
        Self {
            alpha_modes: HashSet::new(),
            coefficient_range_pairs: HashSet::new(),
        }
    }

    /// Conservative defaults suitable for cosmic-comp + HDR video:
    /// - Alpha modes: `PremultipliedElectrical` (the canonical Wayland default
    ///   that every existing client already produces) and `Straight` (so
    ///   future apps can opt out of pre-multiplication explicitly).
    /// - YUV coefficients × range:
    ///     - `Bt709` × (Full, Limited) — most consumer video content
    ///     - `Bt2020` × (Full, Limited) — HDR content
    pub fn conservative() -> Self {
        Self::empty()
            .with_alpha_mode(AlphaMode::PremultipliedElectrical)
            .with_alpha_mode(AlphaMode::Straight)
            .with_coefficients_and_range(Coefficients::Bt709, Range::Full)
            .with_coefficients_and_range(Coefficients::Bt709, Range::Limited)
            .with_coefficients_and_range(Coefficients::Bt2020, Range::Full)
            .with_coefficients_and_range(Coefficients::Bt2020, Range::Limited)
    }

    /// Add an alpha mode to the advertised set.
    pub fn with_alpha_mode(mut self, mode: AlphaMode) -> Self {
        self.alpha_modes.insert(mode);
        self
    }

    /// Add a (coefficients, range) pair to the advertised set.
    pub fn with_coefficients_and_range(mut self, coefficients: Coefficients, range: Range) -> Self {
        self.coefficient_range_pairs.insert((coefficients, range));
        self
    }

    /// Returns whether the alpha mode is advertised.
    pub fn supports_alpha_mode(&self, mode: AlphaMode) -> bool {
        self.alpha_modes.contains(&mode)
    }

    /// Returns whether the (coefficients, range) pair is advertised.
    pub fn supports_coefficients_and_range(&self, coefficients: Coefficients, range: Range) -> bool {
        self.coefficient_range_pairs
            .contains(&(coefficients, range))
    }

    /// Iterator of advertised alpha modes (used by bind handler).
    pub fn alpha_modes(&self) -> impl Iterator<Item = &AlphaMode> {
        self.alpha_modes.iter()
    }

    /// Iterator of advertised (coefficients, range) pairs.
    pub fn coefficient_range_pairs(&self) -> impl Iterator<Item = &(Coefficients, Range)> {
        self.coefficient_range_pairs.iter()
    }
}

impl Default for ColorRepresentationCapabilities {
    fn default() -> Self {
        Self::conservative()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Compositor-wide state for `wp_color_representation_v1`.
#[derive(Debug)]
pub struct ColorRepresentationState {
    global: GlobalId,
    capabilities: ColorRepresentationCapabilities,
    /// Live `wp_color_representation_surface_v1` resources. Currently unused
    /// for change-notification (this protocol has no events on the surface),
    /// kept for parity with `color_management` and future use.
    #[allow(dead_code)]
    surface_resources: Vec<WaylandWeak<WpColorRepresentationSurfaceV1>>,
}

impl ColorRepresentationState {
    /// Register the `wp_color_representation_manager_v1` global on `display`.
    pub fn new<D>(display: &DisplayHandle, capabilities: ColorRepresentationCapabilities) -> Self
    where
        D: GlobalDispatch<WpColorRepresentationManagerV1, ()>
            + Dispatch<WpColorRepresentationManagerV1, ()>
            + Dispatch<WpColorRepresentationSurfaceV1, SurfaceResourceData>
            + ColorRepresentationHandler
            + 'static,
    {
        let global = display.create_global::<D, WpColorRepresentationManagerV1, _>(1, ());
        Self {
            global,
            capabilities,
            surface_resources: Vec::new(),
        }
    }

    /// The registered global ID.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Read-only access to the advertised capabilities.
    pub fn capabilities(&self) -> &ColorRepresentationCapabilities {
        &self.capabilities
    }
}

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

/// Trait the compositor implements to plug `wp_color_representation_v1` state
/// into smithay's dispatch.
///
/// Minimal — this protocol has no compositor-policy callbacks (no preferred-X,
/// no per-output advertisement). The handler exists purely for state access.
pub trait ColorRepresentationHandler {
    /// Mutable access to the `ColorRepresentationState` field on the compositor's
    /// main state.
    fn color_representation_state(&mut self) -> &mut ColorRepresentationState;
}

// ---------------------------------------------------------------------------
// Public helper
// ---------------------------------------------------------------------------

/// Read the currently committed color-representation state of a surface.
///
/// Returns `None` for any field the client hasn't set. Renderers should fall
/// back to wayland-spec defaults (premultiplied alpha + BT.709 + Full + Type 0).
///
/// Smithay's `DrmCompositor` calls this during plane assignment + the cosmic-comp
/// render path calls it during blend-equation selection (Phase 3.3+).
pub fn with_surface_color_representation<F, R>(surface: &WlSurface, callback: F) -> R
where
    F: FnOnce(&ColorRepresentationSurfaceCachedState) -> R,
{
    crate::wayland::compositor::with_states(surface, |states| {
        let mut guard = states
            .cached_state
            .get::<ColorRepresentationSurfaceCachedState>();
        let current = guard.current();
        callback(&current.clone())
    })
}

// ---------------------------------------------------------------------------
// Delegate macro
// ---------------------------------------------------------------------------

/// Delegate dispatch of `wp_color_representation_v1` interfaces.
#[macro_export]
macro_rules! delegate_color_representation {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        const _: () = {
            use $crate::reexports::{
                wayland_protocols::wp::color_representation::v1::server::{
                    wp_color_representation_manager_v1::WpColorRepresentationManagerV1,
                    wp_color_representation_surface_v1::WpColorRepresentationSurfaceV1,
                },
                wayland_server::{delegate_dispatch, delegate_global_dispatch},
            };
            use $crate::wayland::color_representation::{
                ColorRepresentationState, SurfaceResourceData,
            };

            delegate_global_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorRepresentationManagerV1: ()] => ColorRepresentationState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorRepresentationManagerV1: ()] => ColorRepresentationState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorRepresentationSurfaceV1: SurfaceResourceData] => ColorRepresentationState
            );
        };
    };
}
