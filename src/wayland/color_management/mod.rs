//! Implementation of `wp_color_management_v1` (and `wp_image_description_reference_v1`).
//!
//! ### Overview
//!
//! This module exposes a smithay-side implementation of the `wp_color_management_v1`
//! protocol (staging, version 2). It provides:
//!
//! - [`ColorManagementState`] — the global registration + image-description
//!   interner + advertised capability set, lives on the compositor's main state.
//! - [`ColorManagementHandler`] — trait the compositor implements to answer
//!   "what description does this output advertise?" / "what description should
//!   this surface prefer?" / etc.
//! - [`ColorManagementCapabilities`] — builder for the set of named primaries /
//!   named transfer functions / features / render intents the compositor
//!   advertises on bind.
//! - [`with_surface_image_description`] — public helper compositors / smithay's
//!   own DrmCompositor read at frame time to gate scanout & shader behavior.
//!
//! ### Phase 3 plan reference
//!
//! Cosmic-comp's HDR experiment reaches this module from its `state.rs` init via:
//!
//! ```ignore
//! use smithay::wayland::color_management::{ColorManagementState, ColorManagementCapabilities};
//! ColorManagementState::new::<Self>(&dh, ColorManagementCapabilities::conservative());
//! delegate_color_management!(State);
//! ```
//!
//! The `ColorManagementHandler` impl in cosmic-comp synthesizes per-output and
//! per-surface descriptions from existing `hdr_enabled` / `hdr_colorspace` config.
//! Phase 3.3 (re-enable scanout) reads [`with_surface_image_description`] in
//! smithay's `DrmCompositor` plane-assignment path to gate overlay/primary scanout.

use std::collections::HashSet;
use std::sync::Arc;

use wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::WpColorManagementOutputV1,
    wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
    wp_color_manager_v1::{
        Feature, Primaries as ProtoPrimaries, RenderIntent, TransferFunction as ProtoTransferFunction,
        WpColorManagerV1,
    },
    wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_reference_v1::WpImageDescriptionReferenceV1,
    wp_image_description_v1::{Cause, WpImageDescriptionV1},
};
use dispatch::emit_information_events;
use wayland_server::{
    Dispatch, DisplayHandle, GlobalDispatch, Resource, Weak as WaylandWeak, backend::GlobalId,
    protocol::{wl_output::WlOutput, wl_surface::WlSurface},
};

use crate::output::Output;

pub mod description;
pub mod dispatch;
pub mod surface_state;

pub use description::{
    Chromaticities, IccProfile, ImageDescription, ImageDescriptionInterner, Luminances,
    MasteringLuminance, PrimariesDef, TransferFunctionDef,
};
pub use surface_state::{ColorManagementSurfaceCachedState, ColorManagementSurfaceData};

pub use dispatch::{
    DescriptionResourceData, IccCreatorBuilder, OutputResourceData, ParamsCreatorBuilder,
    ReferenceResourceData, SurfaceFeedbackResourceData, SurfaceResourceData,
};

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// Builder for the set of capabilities the compositor advertises on
/// `wp_color_manager_v1` bind.
///
/// On every bind, the manager fans out one event per advertised intent / feature
/// / named primaries / named TF, then sends `done`. Clients use this set to
/// decide which descriptions are safe to construct.
///
/// Use [`Self::conservative`] for a sane HDR-capable default, or [`Self::empty`]
/// + `with_*` builders for fine-grained control.
#[derive(Debug, Clone)]
pub struct ColorManagementCapabilities {
    intents: HashSet<RenderIntent>,
    features: HashSet<Feature>,
    primaries: HashSet<ProtoPrimaries>,
    transfer_functions: HashSet<ProtoTransferFunction>,
}

impl ColorManagementCapabilities {
    /// An empty capability set — advertises nothing. Useful as a starting point
    /// for tests and full custom builds.
    pub fn empty() -> Self {
        Self {
            intents: HashSet::new(),
            features: HashSet::new(),
            primaries: HashSet::new(),
            transfer_functions: HashSet::new(),
        }
    }

    /// A conservative HDR-capable default suitable for cosmic-comp Phase 3.1.
    ///
    /// - Intents: `Perceptual` only
    /// - Features: `Parametric`, `SetPrimaries`, `SetLuminances`,
    ///   `SetMasteringDisplayPrimaries`, `ExtendedTargetVolume`
    /// - Primaries: `Srgb`, `Bt2020`, `DciP3`, `DisplayP3`
    /// - Transfer functions: `Srgb` (deprecated but real clients still use it),
    ///   `St2084Pq`, `Hlg`, `ExtLinear`, `Bt1886`, `Gamma22`
    ///
    /// Excludes ICC profile support, custom power-curve TFs, and non-perceptual
    /// intents — all things the renderer would have to plumb explicitly.
    pub fn conservative() -> Self {
        Self::empty()
            .with_intent(RenderIntent::Perceptual)
            .with_feature(Feature::Parametric)
            .with_feature(Feature::SetPrimaries)
            .with_feature(Feature::SetLuminances)
            .with_feature(Feature::SetMasteringDisplayPrimaries)
            .with_feature(Feature::ExtendedTargetVolume)
            .with_primaries(ProtoPrimaries::Srgb)
            .with_primaries(ProtoPrimaries::Bt2020)
            .with_primaries(ProtoPrimaries::DciP3)
            .with_primaries(ProtoPrimaries::DisplayP3)
            .with_transfer_function(ProtoTransferFunction::Srgb)
            .with_transfer_function(ProtoTransferFunction::St2084Pq)
            .with_transfer_function(ProtoTransferFunction::Hlg)
            .with_transfer_function(ProtoTransferFunction::ExtLinear)
            .with_transfer_function(ProtoTransferFunction::Bt1886)
            .with_transfer_function(ProtoTransferFunction::Gamma22)
    }

    /// Add a render intent to the advertised set.
    pub fn with_intent(mut self, intent: RenderIntent) -> Self {
        self.intents.insert(intent);
        self
    }

    /// Add a feature to the advertised set.
    pub fn with_feature(mut self, feature: Feature) -> Self {
        self.features.insert(feature);
        self
    }

    /// Add a named primaries set to the advertised list.
    pub fn with_primaries(mut self, primaries: ProtoPrimaries) -> Self {
        self.primaries.insert(primaries);
        self
    }

    /// Add a named transfer function to the advertised list.
    pub fn with_transfer_function(mut self, tf: ProtoTransferFunction) -> Self {
        self.transfer_functions.insert(tf);
        self
    }

    /// Returns whether the given intent is advertised.
    pub fn supports_intent(&self, intent: RenderIntent) -> bool {
        self.intents.contains(&intent)
    }

    /// Returns whether the given feature is advertised.
    pub fn supports_feature(&self, feature: Feature) -> bool {
        self.features.contains(&feature)
    }

    /// Returns whether the given named primaries set is advertised.
    pub fn supports_primaries(&self, primaries: ProtoPrimaries) -> bool {
        self.primaries.contains(&primaries)
    }

    /// Returns whether the given named transfer function is advertised.
    pub fn supports_transfer_function(&self, tf: ProtoTransferFunction) -> bool {
        self.transfer_functions.contains(&tf)
    }

    /// Iterator of advertised render intents (used by the bind handler).
    pub fn intents(&self) -> impl Iterator<Item = &RenderIntent> {
        self.intents.iter()
    }

    /// Iterator of advertised features.
    pub fn features(&self) -> impl Iterator<Item = &Feature> {
        self.features.iter()
    }

    /// Iterator of advertised named primaries.
    pub fn primaries(&self) -> impl Iterator<Item = &ProtoPrimaries> {
        self.primaries.iter()
    }

    /// Iterator of advertised named transfer functions.
    pub fn transfer_functions(&self) -> impl Iterator<Item = &ProtoTransferFunction> {
        self.transfer_functions.iter()
    }
}

impl Default for ColorManagementCapabilities {
    fn default() -> Self {
        Self::conservative()
    }
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// Compositor-wide state for the color-management protocol.
///
/// Stored as a field on the compositor's main state. Holds:
/// - the registered `wp_color_manager_v1` global,
/// - the [`ImageDescriptionInterner`] (dedup-by-content + identity assignment),
/// - the [`ColorManagementCapabilities`] advertised on bind,
/// - lightweight registries of per-output / per-surface-feedback resources used
///   to broadcast `image_description_changed` and `preferred_changed2` events.
#[derive(Debug)]
pub struct ColorManagementState {
    global: GlobalId,
    interner: ImageDescriptionInterner,
    capabilities: ColorManagementCapabilities,

    /// All live `wp_color_management_output_v1` resources, used to fan out
    /// `image_description_changed` when an output's description shifts. We store
    /// `Weak` and reap dead entries lazily at notify-time.
    output_resources: Vec<WaylandWeak<WpColorManagementOutputV1>>,
    /// All live `wp_color_management_surface_feedback_v1` resources, similarly
    /// used to fan out `preferred_changed2`.
    feedback_resources: Vec<WaylandWeak<WpColorManagementSurfaceFeedbackV1>>,

    // ---- Deferred destructor-event queues ----------------------------------
    //
    // wayland-backend's `Result::unwrap()` at common_poll.rs:284 panics if a
    // freshly-created (via `data_init.init`) resource has a destructor event
    // sent on it from inside the same dispatch callback — sending the
    // destructor event removes the object from the client's map, so the
    // post-callback "store user_data" hook can't find it.
    //
    // We work around this by queuing destructor emissions and flushing them
    // outside the dispatch callback. The compositor must call
    // [`ColorManagementState::flush_pending`] after every dispatch round
    // (cosmic-comp does this in its main loop alongside `display.flush_clients`).
    pending_info_emissions: Vec<(WpImageDescriptionInfoV1, Arc<ImageDescription>)>,
    pending_failed_descriptions: Vec<(WpImageDescriptionV1, Cause, String)>,
}

impl ColorManagementState {
    /// Register the `wp_color_manager_v1` global on `display`.
    ///
    /// `D` is the compositor state type; it must implement [`ColorManagementHandler`]
    /// and dispatch all the color-management interfaces. The
    /// [`delegate_color_management!`] macro emits the dispatch impls.
    pub fn new<D>(display: &DisplayHandle, capabilities: ColorManagementCapabilities) -> Self
    where
        D: GlobalDispatch<WpColorManagerV1, ()>
            + Dispatch<WpColorManagerV1, ()>
            + Dispatch<WpColorManagementOutputV1, OutputResourceData>
            + Dispatch<WpColorManagementSurfaceV1, SurfaceResourceData>
            + Dispatch<WpColorManagementSurfaceFeedbackV1, SurfaceFeedbackResourceData>
            + Dispatch<WpImageDescriptionCreatorIccV1, IccCreatorBuilder>
            + Dispatch<WpImageDescriptionCreatorParamsV1, ParamsCreatorBuilder>
            + Dispatch<WpImageDescriptionV1, DescriptionResourceData>
            + Dispatch<WpImageDescriptionInfoV1, ()>
            + Dispatch<WpImageDescriptionReferenceV1, ReferenceResourceData>
            + ColorManagementHandler
            + 'static,
    {
        // Advertise version 2 of wp_color_manager_v1 (matches the staging XML
        // shipped with wayland-protocols 0.32.x). v1 clients still bind cleanly;
        // v2-only events (preferred_changed2, ready2) require the bound version
        // to be checked at event-emit time.
        let global = display.create_global::<D, WpColorManagerV1, _>(2, ());
        Self {
            global,
            interner: ImageDescriptionInterner::new(),
            capabilities,
            output_resources: Vec::new(),
            feedback_resources: Vec::new(),
            pending_info_emissions: Vec::new(),
            pending_failed_descriptions: Vec::new(),
        }
    }

    /// Drain any queued destructor-event emissions (info events + `done()` and
    /// per-resource `failed(...)` calls).
    ///
    /// **MUST be called from outside any dispatch callback** — typically once
    /// per main-loop iteration after `Display::dispatch_clients`. Sending these
    /// events from inside a dispatch callback would panic wayland-backend (the
    /// destructor event removes the object from the client map before the
    /// post-callback hook tries to set its user-data; see
    /// `wayland-backend-0.3.15/src/rs/server_impl/common_poll.rs:284`).
    pub fn flush_pending(&mut self) {
        for (info, description) in self.pending_info_emissions.drain(..) {
            emit_information_events(&info, &description);
            // `done()` is a destructor event — safe to send here because we're
            // outside the dispatch callback that init'd `info`.
            info.done();
        }
        for (resource, cause, msg) in self.pending_failed_descriptions.drain(..) {
            resource.failed(cause, msg);
        }
    }

    /// Queue info events + `done()` for emission outside the current dispatch
    /// callback. Called by dispatch handlers.
    pub(crate) fn queue_info_emission(
        &mut self,
        info: WpImageDescriptionInfoV1,
        description: Arc<ImageDescription>,
    ) {
        self.pending_info_emissions.push((info, description));
    }

    /// Queue a `failed(cause, msg)` event for emission outside the current
    /// dispatch callback. Called by dispatch handlers.
    pub(crate) fn queue_failed_description(
        &mut self,
        resource: WpImageDescriptionV1,
        cause: Cause,
        msg: String,
    ) {
        self.pending_failed_descriptions
            .push((resource, cause, msg));
    }

    /// The registered `wp_color_manager_v1` global ID.
    pub fn global(&self) -> GlobalId {
        self.global.clone()
    }

    /// Read-only access to the advertised capability set.
    pub fn capabilities(&self) -> &ColorManagementCapabilities {
        &self.capabilities
    }

    /// Mutable access to the description interner.
    ///
    /// Dispatch handlers use this to intern newly built descriptions (from the
    /// params / ICC creators or `wp_color_management_output_v1.get_image_description`).
    pub fn interner_mut(&mut self) -> &mut ImageDescriptionInterner {
        &mut self.interner
    }

    /// Read-only access to the description interner.
    pub fn interner(&self) -> &ImageDescriptionInterner {
        &self.interner
    }

    /// Register a newly bound `wp_color_management_output_v1` resource so we can
    /// later fire `image_description_changed` on it. Called from dispatch.
    pub(crate) fn register_output_resource(&mut self, resource: &WpColorManagementOutputV1) {
        self.output_resources.push(resource.downgrade());
    }

    /// Register a newly bound `wp_color_management_surface_feedback_v1` resource.
    pub(crate) fn register_feedback_resource(
        &mut self,
        resource: &WpColorManagementSurfaceFeedbackV1,
    ) {
        self.feedback_resources.push(resource.downgrade());
    }

    /// Reap dead output resource refs and return the live ones. Called by the
    /// notify path; mutating + returning means callers don't pay for dead entries
    /// repeatedly.
    fn live_output_resources(&mut self) -> Vec<WpColorManagementOutputV1> {
        let mut alive = Vec::new();
        self.output_resources.retain(|w| {
            if let Ok(r) = w.upgrade() {
                alive.push(r);
                true
            } else {
                false
            }
        });
        alive
    }

    /// Reap dead feedback resource refs and return the live ones.
    fn live_feedback_resources(&mut self) -> Vec<WpColorManagementSurfaceFeedbackV1> {
        let mut alive = Vec::new();
        self.feedback_resources.retain(|w| {
            if let Ok(r) = w.upgrade() {
                alive.push(r);
                true
            } else {
                false
            }
        });
        alive
    }
}

// ---------------------------------------------------------------------------
// Handler trait
// ---------------------------------------------------------------------------

/// Trait the compositor implements to expose color-management policy decisions
/// to smithay.
///
/// All methods take `&mut self` so implementations can lazily intern descriptions
/// they synthesize from internal compositor state (e.g. cosmic-comp building a
/// PQ-BT2020 description from `hdr_enabled` + `hdr_colorspace`).
pub trait ColorManagementHandler {
    /// Mutable access to the `ColorManagementState` field on the compositor's
    /// main state. Smithay calls this from inside dispatch handlers.
    fn color_management_state(&mut self) -> &mut ColorManagementState;

    /// What image description should the compositor advertise on this output?
    ///
    /// Called by `wp_color_management_output_v1.get_image_description` and used
    /// to drive `image_description_changed` events.
    ///
    /// Default returns the interner's pre-populated sRGB description, suitable
    /// for stub / pre-Phase-3.2 builds.
    fn output_image_description(&mut self, _output: &WlOutput) -> Arc<ImageDescription> {
        self.color_management_state().interner().srgb_default()
    }

    /// What image description should we recommend a surface use? Hyprland
    /// implements this as "the surface's main output's description, with HDR
    /// override for known-HDR clients."
    ///
    /// Default returns sRGB so a minimal handler still satisfies the protocol.
    fn preferred_image_description(
        &mut self,
        _surface: &WlSurface,
    ) -> Arc<ImageDescription> {
        self.color_management_state().interner().srgb_default()
    }

    /// Called once after a fresh description has been interned (post creator-side
    /// `create`). Lets the compositor log / instrument new descriptions. Default
    /// is no-op.
    fn new_image_description(&mut self, _description: &Arc<ImageDescription>) {}

    /// Called when a description we just constructed cannot actually be honored
    /// (e.g. unsupported feature combo or ICC parse fail). We send `failed` on
    /// the resource regardless of what this method does; this is purely a
    /// notification hook for the compositor to log / surface diagnostics.
    fn image_description_failed(&mut self, _resource: &WpImageDescriptionV1, _cause: Cause) {}
}

// ---------------------------------------------------------------------------
// Public helpers
// ---------------------------------------------------------------------------

/// Read the currently committed color-management state of a surface.
///
/// Returns the description (`Some`) and render intent the client most recently
/// committed via `wp_color_management_surface_v1.set_image_description`, or
/// `None` for the description if the client has never set one (or has unset it).
///
/// **This is the accessor smithay's `DrmCompositor` calls during plane assignment
/// (Phase 3.3 — re-enable scanout).** Reading the surface's description is how
/// we gate overlay-plane scanout: only assign a surface to an overlay if its
/// description matches the output's description, otherwise the kernel composites
/// pre-DEGAMMA across mixed encodings and we get region-flicker.
pub fn with_surface_image_description<F, R>(surface: &WlSurface, callback: F) -> R
where
    F: FnOnce(Option<&Arc<ImageDescription>>, RenderIntent) -> R,
{
    crate::wayland::compositor::with_states(surface, |states| {
        let mut guard = states
            .cached_state
            .get::<ColorManagementSurfaceCachedState>();
        let current = guard.current();
        callback(current.description.as_ref(), current.render_intent)
    })
}

/// Notify the compositor that the given smithay [`Output`]'s image description
/// has changed.
///
/// Call this when `output_image_description(output)` would now return a
/// different `Arc<ImageDescription>` than it did before — typically as a result
/// of toggling `hdr_enabled`, switching colorspace, or hardware mode changes.
///
/// We fan out `image_description_changed` to every live `wp_color_management_output_v1`
/// whose underlying `wl_output` resolves to this smithay `Output` (across all
/// clients), and re-evaluate every live surface-feedback resource (firing
/// `preferred_changed2` if its preferred description's identity has shifted).
pub fn notify_output_image_description_changed<D>(state: &mut D, output: &Output)
where
    D: ColorManagementHandler,
{
    // Output resources first — fire image_description_changed for any whose
    // bound wl_output resolves back to the changed smithay Output.
    let cm_state = state.color_management_state();
    let alive_outputs = cm_state.live_output_resources();
    for resource in alive_outputs {
        let data = match resource.data::<OutputResourceData>() {
            Some(d) => d,
            None => continue,
        };
        if let Ok(bound_wl_output) = data.output.upgrade() {
            if Output::from_resource(&bound_wl_output).as_ref() == Some(output) {
                resource.image_description_changed();
            }
        }
    }

    // Then sweep all feedback resources — re-evaluate preferred description for
    // each. We can't cheaply know which surfaces are on the changed output, so
    // we re-evaluate everything and let the compositor's
    // `preferred_image_description` method decide. The identity comparison
    // suppresses redundant `preferred_changed` events for surfaces whose
    // preferred description didn't actually shift.
    let cm_state = state.color_management_state();
    let alive_feedback = cm_state.live_feedback_resources();
    for resource in alive_feedback {
        let data = match resource.data::<SurfaceFeedbackResourceData>() {
            Some(d) => d,
            None => continue,
        };
        let surface = match data.surface.upgrade() {
            Ok(s) => s,
            Err(_) => continue,
        };
        let new_desc = state.preferred_image_description(&surface);
        let new_id = new_desc.identity;
        let last_id = data.last_sent_identity();
        if last_id != new_id {
            data.set_last_sent_identity(new_id);
            send_preferred_changed(&resource, new_id);
        }
    }
}

/// Send `preferred_changed2` (v2) or `preferred_changed` (v1) based on the
/// resource's bound version.
pub(crate) fn send_preferred_changed(
    resource: &WpColorManagementSurfaceFeedbackV1,
    identity: u64,
) {
    let (hi, lo) = ((identity >> 32) as u32, identity as u32);
    if resource.version() >= 2 {
        resource.preferred_changed2(hi, lo);
    } else {
        // v1 only had a 32-bit identity; truncate. Safe for the first
        // 4 billion descriptions, which we will never reach.
        resource.preferred_changed(lo);
    }
}

/// Send `ready2` (v2) or `ready` (v1) on a description resource based on its
/// bound version.
pub(crate) fn send_ready(resource: &WpImageDescriptionV1, identity: u64) {
    let (hi, lo) = ((identity >> 32) as u32, identity as u32);
    if resource.version() >= 2 {
        resource.ready2(hi, lo);
    } else {
        resource.ready(lo);
    }
}

// ---------------------------------------------------------------------------
// Delegate macro
// ---------------------------------------------------------------------------

/// Delegate dispatch of all `wp_color_management_v1` interfaces to
/// [`ColorManagementState`].
///
/// Use on the compositor's main state once the state implements
/// [`ColorManagementHandler`]:
///
/// ```ignore
/// delegate_color_management!(State);
/// ```
#[macro_export]
macro_rules! delegate_color_management {
    ($(@<$( $lt:tt $( : $clt:tt $(+ $dlt:tt )* )? ),+>)? $ty: ty) => {
        const _: () = {
            use $crate::reexports::{
                wayland_protocols::wp::color_management::v1::server::{
                    wp_color_management_output_v1::WpColorManagementOutputV1,
                    wp_color_management_surface_feedback_v1::WpColorManagementSurfaceFeedbackV1,
                    wp_color_management_surface_v1::WpColorManagementSurfaceV1,
                    wp_color_manager_v1::WpColorManagerV1,
                    wp_image_description_creator_icc_v1::WpImageDescriptionCreatorIccV1,
                    wp_image_description_creator_params_v1::WpImageDescriptionCreatorParamsV1,
                    wp_image_description_info_v1::WpImageDescriptionInfoV1,
                    wp_image_description_reference_v1::WpImageDescriptionReferenceV1,
                    wp_image_description_v1::WpImageDescriptionV1,
                },
                wayland_server::{delegate_dispatch, delegate_global_dispatch},
            };
            use $crate::wayland::color_management::{
                ColorManagementState, DescriptionResourceData, IccCreatorBuilder,
                OutputResourceData, ParamsCreatorBuilder, ReferenceResourceData,
                SurfaceFeedbackResourceData, SurfaceResourceData,
            };

            delegate_global_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorManagerV1: ()] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorManagerV1: ()] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorManagementOutputV1: OutputResourceData] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorManagementSurfaceV1: SurfaceResourceData] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpColorManagementSurfaceFeedbackV1: SurfaceFeedbackResourceData] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpImageDescriptionCreatorIccV1: IccCreatorBuilder] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpImageDescriptionCreatorParamsV1: ParamsCreatorBuilder] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpImageDescriptionV1: DescriptionResourceData] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpImageDescriptionInfoV1: ()] => ColorManagementState
            );
            delegate_dispatch!(
                $(@< $( $lt $( : $clt $(+ $dlt )* )? ),+ >)?
                $ty: [WpImageDescriptionReferenceV1: ReferenceResourceData] => ColorManagementState
            );
        };
    };
}
