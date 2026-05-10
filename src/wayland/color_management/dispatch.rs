//! Dispatch implementations for all `wp_color_management_v1` interfaces.
//!
//! Organized by interface, top-down: manager → output → surface → surface_feedback
//! → params creator → ICC creator → image_description → image_description_info
//! → image_description_reference. Each section starts with a banner comment.

use std::io::{Read, Seek, SeekFrom};
use std::os::fd::OwnedFd;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicU64, Ordering},
};

use wayland_protocols::wp::color_management::v1::server::{
    wp_color_management_output_v1::{self, WpColorManagementOutputV1},
    wp_color_management_surface_feedback_v1::{self, WpColorManagementSurfaceFeedbackV1},
    wp_color_management_surface_v1::{self, WpColorManagementSurfaceV1},
    wp_color_manager_v1::{self, WpColorManagerV1},
    wp_image_description_creator_icc_v1::{self, WpImageDescriptionCreatorIccV1},
    wp_image_description_creator_params_v1::{self, WpImageDescriptionCreatorParamsV1},
    wp_image_description_info_v1::WpImageDescriptionInfoV1,
    wp_image_description_reference_v1::{self, WpImageDescriptionReferenceV1},
    wp_image_description_v1::{self, Cause, WpImageDescriptionV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, Weak as WaylandWeak,
    backend::ClientId,
    protocol::{wl_output::WlOutput, wl_surface::WlSurface},
};

use super::{
    Chromaticities, ColorManagementHandler, ColorManagementState, ColorManagementSurfaceCachedState,
    ColorManagementSurfaceData, IccProfile, ImageDescription, Luminances, MasteringLuminance,
    PrimariesDef, TransferFunctionDef, send_ready,
};
use crate::wayland::compositor;

// ===========================================================================
// User data types attached to each protocol resource
// ===========================================================================

/// User data on a `wp_color_management_output_v1` resource.
#[derive(Debug)]
pub struct OutputResourceData {
    /// The `wl_output` this color-management interface is paired with.
    pub output: WaylandWeak<WlOutput>,
}

/// User data on a `wp_color_management_surface_v1` resource.
#[derive(Debug)]
pub struct SurfaceResourceData {
    /// The `wl_surface` this color-management interface is paired with.
    pub surface: WaylandWeak<WlSurface>,
}

/// User data on a `wp_color_management_surface_feedback_v1` resource.
#[derive(Debug)]
pub struct SurfaceFeedbackResourceData {
    /// The `wl_surface` this feedback interface is paired with.
    pub surface: WaylandWeak<WlSurface>,
    /// Identity of the description we last sent via `preferred_changed[2]` for
    /// this resource. Used to suppress redundant events. `AtomicU64` so the
    /// notify path can read+write without taking a `&mut` reference.
    last_sent_identity: AtomicU64,
}

impl SurfaceFeedbackResourceData {
    pub(crate) fn new(surface: WaylandWeak<WlSurface>) -> Self {
        Self {
            surface,
            last_sent_identity: AtomicU64::new(0),
        }
    }

    /// Read the last-sent identity (0 means we haven't sent yet).
    pub fn last_sent_identity(&self) -> u64 {
        self.last_sent_identity.load(Ordering::Acquire)
    }

    /// Update the last-sent identity.
    pub fn set_last_sent_identity(&self, identity: u64) {
        self.last_sent_identity.store(identity, Ordering::Release);
    }
}

/// Internal state of a `wp_image_description_v1` resource.
#[derive(Debug)]
enum DescriptionResourceState {
    /// Description is ready and the resource has been (or will be) sent
    /// `ready` / `ready2`.
    Ready(Arc<ImageDescription>),
    /// Description failed to construct; resource has been (or will be) sent
    /// `failed` and is now inert.
    Inert,
}

/// User data on a `wp_image_description_v1` resource.
#[derive(Debug)]
pub struct DescriptionResourceData {
    /// Internal state — either a ready Arc<description> or inert. Wrapped in
    /// Mutex so we can flip Ready→Inert if the resource is later invalidated
    /// (e.g. an underlying output going away).
    state: Mutex<DescriptionResourceState>,
    /// `true` if this description's origin permits `get_information`. False for
    /// descriptions created via `wp_color_management_output_v1.get_image_description`
    /// or `wp_color_manager_v1.create_windows_scrgb` — per protocol spec, those
    /// must reject `get_information` with `no_information`.
    pub allow_get_information: bool,
}

impl DescriptionResourceData {
    pub(crate) fn ready(description: Arc<ImageDescription>, allow_get_information: bool) -> Self {
        Self {
            state: Mutex::new(DescriptionResourceState::Ready(description)),
            allow_get_information,
        }
    }

    /// Snapshot the current description (`Some`) or `None` if the resource is
    /// inert.
    pub fn description(&self) -> Option<Arc<ImageDescription>> {
        match &*self.state.lock().unwrap() {
            DescriptionResourceState::Ready(arc) => Some(Arc::clone(arc)),
            DescriptionResourceState::Inert => None,
        }
    }
}

/// User data on a `wp_image_description_reference_v1` resource.
#[derive(Debug)]
pub struct ReferenceResourceData {
    /// The description this reference points at.
    pub description: Arc<ImageDescription>,
    /// Whether descriptions cloned from this reference (via
    /// `wp_color_manager_v1.get_image_description`) should permit
    /// `get_information`.
    pub allow_get_information: bool,
}

// ---------------------------------------------------------------------------
// Builders for the params and ICC creators (one-shot, mutated by setter requests
// before being consumed by `create`).
// ---------------------------------------------------------------------------

/// Mutable in-progress state of a `wp_image_description_creator_params_v1` instance.
/// Wrapped in a [`Mutex`] in resource user-data because dispatch handlers receive
/// `&Self` (the user-data ref), not `&mut`.
#[derive(Debug, Default)]
pub struct ParamsCreatorBuilder(pub(crate) Mutex<ParamsCreatorBuilderInner>);

/// In-progress field set for a parametric image-description creator. Hidden from
/// docs — accessed only by dispatch handlers and the [`Drop`] semantics on the
/// outer wrapper.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct ParamsCreatorBuilderInner {
    pub(crate) tf: Option<TransferFunctionDef>,
    pub(crate) primaries: Option<PrimariesDef>,
    pub(crate) luminances: Option<Luminances>,
    pub(crate) mastering_primaries: Option<Chromaticities>,
    pub(crate) mastering_luminance: Option<MasteringLuminance>,
    pub(crate) max_cll: Option<u32>,
    pub(crate) max_fall: Option<u32>,
    /// Set to true once `create` has been called, so a buggy second `create`
    /// can be detected at the dispatch layer.
    pub(crate) consumed: bool,
}

/// Mutable in-progress state of a `wp_image_description_creator_icc_v1` instance.
#[derive(Debug, Default)]
pub struct IccCreatorBuilder(pub(crate) Mutex<IccCreatorBuilderInner>);

/// In-progress ICC bytes for an ICC creator. Hidden from docs — internal to
/// dispatch.
#[doc(hidden)]
#[derive(Debug, Default)]
pub struct IccCreatorBuilderInner {
    pub(crate) icc_bytes: Option<Vec<u8>>,
    pub(crate) consumed: bool,
}

// ===========================================================================
// wp_color_manager_v1 — singleton manager
// ===========================================================================

impl<D> GlobalDispatch<WpColorManagerV1, (), D> for ColorManagementState
where
    D: GlobalDispatch<WpColorManagerV1, ()>
        + Dispatch<WpColorManagerV1, ()>
        + ColorManagementHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpColorManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());

        // Advertise capabilities. Order doesn't matter to clients but we go
        // intent → feature → tf_named → primaries_named to match what Hyprland
        // emits, then `done`.
        let caps = state.color_management_state().capabilities().clone();

        for intent in caps.intents() {
            manager.supported_intent(*intent);
        }
        for feature in caps.features() {
            manager.supported_feature(*feature);
        }
        for tf in caps.transfer_functions() {
            manager.supported_tf_named(*tf);
        }
        for primaries in caps.primaries() {
            manager.supported_primaries_named(*primaries);
        }
        manager.done();
    }
}

impl<D> Dispatch<WpColorManagerV1, (), D> for ColorManagementState
where
    D: Dispatch<WpColorManagerV1, ()>
        + Dispatch<WpColorManagementOutputV1, OutputResourceData>
        + Dispatch<WpColorManagementSurfaceV1, SurfaceResourceData>
        + Dispatch<WpColorManagementSurfaceFeedbackV1, SurfaceFeedbackResourceData>
        + Dispatch<WpImageDescriptionCreatorIccV1, IccCreatorBuilder>
        + Dispatch<WpImageDescriptionCreatorParamsV1, ParamsCreatorBuilder>
        + Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        manager: &WpColorManagerV1,
        request: wp_color_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_manager_v1::Request::Destroy => {}

            wp_color_manager_v1::Request::GetOutput { id, output } => {
                let resource = data_init.init(
                    id,
                    OutputResourceData {
                        output: output.downgrade(),
                    },
                );
                state
                    .color_management_state()
                    .register_output_resource(&resource);
            }

            wp_color_manager_v1::Request::GetSurface { id, surface } => {
                // `get_surface` is exclusive: only one wp_color_management_surface_v1
                // may exist per wl_surface at a time. The `surface_exists` error is
                // posted on the manager, not the new resource.
                let already_attached = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(ColorManagementSurfaceData::new);
                    let data = states.data_map.get::<ColorManagementSurfaceData>().unwrap();
                    data.is_resource_attached_or_attach()
                });
                if already_attached {
                    manager.post_error(
                        wp_color_manager_v1::Error::SurfaceExists,
                        "wl_surface already has wp_color_management_surface_v1",
                    );
                    return;
                }
                data_init.init(
                    id,
                    SurfaceResourceData {
                        surface: surface.downgrade(),
                    },
                );
            }

            wp_color_manager_v1::Request::GetSurfaceFeedback { id, surface } => {
                let resource = data_init.init(
                    id,
                    SurfaceFeedbackResourceData::new(surface.downgrade()),
                );
                state
                    .color_management_state()
                    .register_feedback_resource(&resource);
            }

            wp_color_manager_v1::Request::CreateIccCreator { obj } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(wp_color_manager_v1::Feature::IccV2V4)
                {
                    manager.post_error(
                        wp_color_manager_v1::Error::UnsupportedFeature,
                        "ICC v2/v4 image descriptions not supported",
                    );
                    return;
                }
                data_init.init(obj, IccCreatorBuilder::default());
            }

            wp_color_manager_v1::Request::CreateParametricCreator { obj } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(wp_color_manager_v1::Feature::Parametric)
                {
                    manager.post_error(
                        wp_color_manager_v1::Error::UnsupportedFeature,
                        "parametric image descriptions not supported",
                    );
                    return;
                }
                data_init.init(obj, ParamsCreatorBuilder::default());
            }

            wp_color_manager_v1::Request::CreateWindowsScrgb { image_description } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(wp_color_manager_v1::Feature::WindowsScrgb)
                {
                    manager.post_error(
                        wp_color_manager_v1::Error::UnsupportedFeature,
                        "Windows-scRGB image descriptions not supported",
                    );
                    return;
                }
                let arc = state.color_management_state().interner_mut().intern(
                    ImageDescription {
                        primaries: Some(PrimariesDef::Named(
                            wp_color_manager_v1::Primaries::Srgb,
                        )),
                        transfer_function: Some(TransferFunctionDef::Named(
                            wp_color_manager_v1::TransferFunction::ExtLinear,
                        )),
                        luminances: None,
                        mastering_primaries: None,
                        mastering_luminance: None,
                        max_cll: None,
                        max_fall: None,
                        icc: None,
                        windows_scrgb: true,
                        identity: 0,
                    },
                );
                state.new_image_description(&arc);
                let identity = arc.identity;
                let resource = data_init.init(
                    image_description,
                    DescriptionResourceData::ready(arc, /* allow_get_information */ false),
                );
                send_ready(&resource, identity);
            }

            wp_color_manager_v1::Request::GetImageDescription {
                image_description,
                reference,
            } => {
                // Since v2: clone an existing reference into a new wp_image_description_v1.
                let ref_data = match reference.data::<ReferenceResourceData>() {
                    Some(d) => d,
                    None => {
                        // Shouldn't happen — but if the reference resource has no
                        // data, fall back to inert and queue a deferred `failed`.
                        let resource = data_init.init(
                            image_description,
                            DescriptionResourceData {
                                state: Mutex::new(DescriptionResourceState::Inert),
                                allow_get_information: false,
                            },
                        );
                        state.color_management_state().queue_failed_description(
                            resource,
                            Cause::OperatingSystem,
                            "missing reference data".into(),
                        );
                        return;
                    }
                };
                let arc = Arc::clone(&ref_data.description);
                let identity = arc.identity;
                let allow = ref_data.allow_get_information;
                let resource = data_init.init(
                    image_description,
                    DescriptionResourceData::ready(arc, allow),
                );
                send_ready(&resource, identity);
            }

            _ => unreachable!(),
        }
    }
}

// ===========================================================================
// wp_color_management_output_v1
// ===========================================================================

impl<D> Dispatch<WpColorManagementOutputV1, OutputResourceData, D> for ColorManagementState
where
    D: Dispatch<WpColorManagementOutputV1, OutputResourceData>
        + Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &WpColorManagementOutputV1,
        request: wp_color_management_output_v1::Request,
        data: &OutputResourceData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_management_output_v1::Request::Destroy => {}

            wp_color_management_output_v1::Request::GetImageDescription { image_description } => {
                let output = match data.output.upgrade() {
                    Ok(o) => o,
                    Err(_) => {
                        // Output gone — protocol says we should still init the
                        // resource and send `failed(no_output)`. Queue the event
                        // for emission after the current dispatch returns.
                        let resource = data_init.init(
                            image_description,
                            DescriptionResourceData {
                                state: Mutex::new(DescriptionResourceState::Inert),
                                allow_get_information: false,
                            },
                        );
                        state.color_management_state().queue_failed_description(
                            resource,
                            Cause::NoOutput,
                            "wl_output destroyed".into(),
                        );
                        return;
                    }
                };
                let arc = state.output_image_description(&output);
                let identity = arc.identity;
                // Per spec, output-derived descriptions DO allow get_information —
                // wayland-info / clients use this to read the output's primaries +
                // luminance.
                let resource = data_init.init(
                    image_description,
                    DescriptionResourceData::ready(arc, /* allow_get_information */ true),
                );
                send_ready(&resource, identity);
            }

            _ => unreachable!(),
        }
    }
}

// ===========================================================================
// wp_color_management_surface_v1
// ===========================================================================

impl<D> Dispatch<WpColorManagementSurfaceV1, SurfaceResourceData, D> for ColorManagementState
where
    D: Dispatch<WpColorManagementSurfaceV1, SurfaceResourceData>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &WpColorManagementSurfaceV1,
        request: wp_color_management_surface_v1::Request,
        data: &SurfaceResourceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_management_surface_v1::Request::Destroy => {
                // Detach the surface marker so a future get_surface can succeed.
                if let Ok(surface) = data.surface.upgrade() {
                    compositor::with_states(&surface, |states| {
                        if let Some(d) = states.data_map.get::<ColorManagementSurfaceData>() {
                            d.detach();
                        }
                        // Spec: equivalent to unset_image_description — clear pending.
                        let mut guard = states
                            .cached_state
                            .get::<ColorManagementSurfaceCachedState>();
                        let pending = guard.pending();
                        pending.description = None;
                        pending.render_intent = wp_color_manager_v1::RenderIntent::Perceptual;
                    });
                }
            }

            wp_color_management_surface_v1::Request::SetImageDescription {
                image_description,
                render_intent,
            } => {
                let surface = match data.surface.upgrade() {
                    Ok(s) => s,
                    Err(_) => return, // surface gone, nothing to do
                };

                // Resolve the wl_surface's render_intent enum value (it's a WEnum
                // because the client could send any u32).
                let render_intent = match render_intent {
                    wayland_server::WEnum::Value(v) => v,
                    wayland_server::WEnum::Unknown(_) => {
                        resource.post_error(
                            wp_color_management_surface_v1::Error::RenderIntent,
                            "unknown render intent",
                        );
                        return;
                    }
                };

                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_intent(render_intent)
                {
                    resource.post_error(
                        wp_color_management_surface_v1::Error::RenderIntent,
                        "render intent not advertised by compositor",
                    );
                    return;
                }

                // Resolve description from its resource user-data.
                let desc_data = match image_description.data::<DescriptionResourceData>() {
                    Some(d) => d,
                    None => {
                        resource.post_error(
                            wp_color_management_surface_v1::Error::ImageDescription,
                            "missing description user-data",
                        );
                        return;
                    }
                };
                let arc = match desc_data.description() {
                    Some(a) => a,
                    None => {
                        resource.post_error(
                            wp_color_management_surface_v1::Error::ImageDescription,
                            "image description is inert",
                        );
                        return;
                    }
                };

                compositor::with_states(&surface, |states| {
                    let mut guard = states
                        .cached_state
                        .get::<ColorManagementSurfaceCachedState>();
                    let pending = guard.pending();
                    pending.description = Some(arc);
                    pending.render_intent = render_intent;
                });
            }

            wp_color_management_surface_v1::Request::UnsetImageDescription => {
                let surface = match data.surface.upgrade() {
                    Ok(s) => s,
                    Err(_) => return,
                };
                compositor::with_states(&surface, |states| {
                    let mut guard = states
                        .cached_state
                        .get::<ColorManagementSurfaceCachedState>();
                    let pending = guard.pending();
                    pending.description = None;
                    pending.render_intent = wp_color_manager_v1::RenderIntent::Perceptual;
                });
            }

            _ => unreachable!(),
        }
    }

    fn destroyed(
        _state: &mut D,
        _client: ClientId,
        _resource: &WpColorManagementSurfaceV1,
        data: &SurfaceResourceData,
    ) {
        // Re-detach in case the client closed without sending Destroy.
        if let Ok(surface) = data.surface.upgrade() {
            compositor::with_states(&surface, |states| {
                if let Some(d) = states.data_map.get::<ColorManagementSurfaceData>() {
                    d.detach();
                }
            });
        }
    }
}

// ===========================================================================
// wp_color_management_surface_feedback_v1
// ===========================================================================

impl<D> Dispatch<WpColorManagementSurfaceFeedbackV1, SurfaceFeedbackResourceData, D>
    for ColorManagementState
where
    D: Dispatch<WpColorManagementSurfaceFeedbackV1, SurfaceFeedbackResourceData>
        + Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        _resource: &WpColorManagementSurfaceFeedbackV1,
        request: wp_color_management_surface_feedback_v1::Request,
        data: &SurfaceFeedbackResourceData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_management_surface_feedback_v1::Request::Destroy => {}

            wp_color_management_surface_feedback_v1::Request::GetPreferred { image_description } => {
                handle_feedback_get_preferred(state, data, image_description, false, data_init);
            }
            wp_color_management_surface_feedback_v1::Request::GetPreferredParametric {
                image_description,
            } => {
                handle_feedback_get_preferred(state, data, image_description, true, data_init);
            }

            _ => unreachable!(),
        }
    }
}

/// Shared body of `get_preferred` and `get_preferred_parametric`. The latter
/// additionally fails with `unsupported` when the preferred description has no
/// parametric form.
fn handle_feedback_get_preferred<D>(
    state: &mut D,
    data: &SurfaceFeedbackResourceData,
    image_description: New<WpImageDescriptionV1>,
    parametric_only: bool,
    data_init: &mut DataInit<'_, D>,
) where
    D: Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + ColorManagementHandler
        + 'static,
{
    let surface = match data.surface.upgrade() {
        Ok(s) => s,
        Err(_) => {
            let res = data_init.init(
                image_description,
                DescriptionResourceData {
                    state: Mutex::new(DescriptionResourceState::Inert),
                    allow_get_information: false,
                },
            );
            state.color_management_state().queue_failed_description(
                res,
                Cause::OperatingSystem,
                "wl_surface destroyed".into(),
            );
            return;
        }
    };

    let arc = state.preferred_image_description(&surface);

    if parametric_only && !arc.is_parametric() {
        // Per spec for get_preferred_parametric: send failed(unsupported) if no
        // parametric form available (e.g. ICC-only output). Queue for emission
        // outside this dispatch callback.
        let res = data_init.init(
            image_description,
            DescriptionResourceData {
                state: Mutex::new(DescriptionResourceState::Inert),
                allow_get_information: false,
            },
        );
        state.color_management_state().queue_failed_description(
            res,
            Cause::Unsupported,
            "preferred description has no parametric form".into(),
        );
        return;
    }

    let identity = arc.identity;
    data.set_last_sent_identity(identity);
    // Per spec, surface-feedback get_preferred[_parametric] descriptions allow
    // get_information so clients can introspect what the compositor recommends.
    let res = data_init.init(
        image_description,
        DescriptionResourceData::ready(arc, /* allow_get_information */ true),
    );
    send_ready(&res, identity);
    // No preferred_changed[2] here — spec says it's emitted on subsequent
    // changes, not on first get_preferred[_parametric].
}

// ===========================================================================
// wp_image_description_creator_params_v1
// ===========================================================================

impl<D> Dispatch<WpImageDescriptionCreatorParamsV1, ParamsCreatorBuilder, D>
    for ColorManagementState
where
    D: Dispatch<WpImageDescriptionCreatorParamsV1, ParamsCreatorBuilder>
        + Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionCreatorParamsV1,
        request: wp_image_description_creator_params_v1::Request,
        data: &ParamsCreatorBuilder,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_image_description_creator_params_v1::Request::Create { image_description } => {
                // Drain the builder.
                let inner = {
                    let mut guard = data.0.lock().unwrap();
                    if guard.consumed {
                        // Defensive — should be unreachable since `create` is a
                        // destructor request.
                        resource.post_error(
                            wp_image_description_creator_params_v1::Error::AlreadySet,
                            "create called twice",
                        );
                        return;
                    }
                    guard.consumed = true;
                    std::mem::take(&mut *guard)
                };

                if inner.tf.is_none() || inner.primaries.is_none() {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::IncompleteSet,
                        "tf and primaries are required",
                    );
                    return;
                }

                let description = ImageDescription {
                    primaries: inner.primaries,
                    transfer_function: inner.tf,
                    luminances: inner.luminances,
                    mastering_primaries: inner.mastering_primaries,
                    mastering_luminance: inner.mastering_luminance,
                    max_cll: inner.max_cll,
                    max_fall: inner.max_fall,
                    icc: None,
                    windows_scrgb: false,
                    identity: 0,
                };
                let arc = state
                    .color_management_state()
                    .interner_mut()
                    .intern(description);
                state.new_image_description(&arc);
                let identity = arc.identity;
                // Per spec, params-creator descriptions do NOT allow get_information
                // — the client built the description themselves and already knows
                // its parameters.
                let res = data_init.init(
                    image_description,
                    DescriptionResourceData::ready(arc, /* allow_get_information */ false),
                );
                send_ready(&res, identity);
            }

            wp_image_description_creator_params_v1::Request::SetTfNamed { tf } => {
                let mut guard = data.0.lock().unwrap();
                if guard.tf.is_some() {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::AlreadySet,
                        "transfer function already set",
                    );
                    return;
                }
                let tf = match tf {
                    wayland_server::WEnum::Value(v) => v,
                    _ => {
                        resource.post_error(
                            wp_image_description_creator_params_v1::Error::InvalidTf,
                            "unknown transfer function",
                        );
                        return;
                    }
                };
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_transfer_function(tf)
                {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::InvalidTf,
                        "transfer function not advertised",
                    );
                    return;
                }
                guard.tf = Some(TransferFunctionDef::Named(tf));
            }

            wp_image_description_creator_params_v1::Request::SetTfPower { eexp } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(wp_color_manager_v1::Feature::SetTfPower)
                {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::UnsupportedFeature,
                        "set_tf_power not advertised",
                    );
                    return;
                }
                let mut guard = data.0.lock().unwrap();
                if guard.tf.is_some() {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::AlreadySet,
                        "transfer function already set",
                    );
                    return;
                }
                guard.tf = Some(TransferFunctionDef::Power(eexp));
            }

            wp_image_description_creator_params_v1::Request::SetPrimariesNamed { primaries } => {
                let mut guard = data.0.lock().unwrap();
                if guard.primaries.is_some() {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::AlreadySet,
                        "primaries already set",
                    );
                    return;
                }
                let primaries = match primaries {
                    wayland_server::WEnum::Value(v) => v,
                    _ => {
                        resource.post_error(
                            wp_image_description_creator_params_v1::Error::InvalidPrimariesNamed,
                            "unknown primaries",
                        );
                        return;
                    }
                };
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_primaries(primaries)
                {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::InvalidPrimariesNamed,
                        "primaries not advertised",
                    );
                    return;
                }
                guard.primaries = Some(PrimariesDef::Named(primaries));
            }

            wp_image_description_creator_params_v1::Request::SetPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(wp_color_manager_v1::Feature::SetPrimaries)
                {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::UnsupportedFeature,
                        "set_primaries not advertised",
                    );
                    return;
                }
                let mut guard = data.0.lock().unwrap();
                if guard.primaries.is_some() {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::AlreadySet,
                        "primaries already set",
                    );
                    return;
                }
                guard.primaries = Some(PrimariesDef::Custom(Chromaticities {
                    r: (r_x, r_y),
                    g: (g_x, g_y),
                    b: (b_x, b_y),
                    w: (w_x, w_y),
                }));
            }

            wp_image_description_creator_params_v1::Request::SetLuminances {
                min_lum,
                max_lum,
                reference_lum,
            } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(wp_color_manager_v1::Feature::SetLuminances)
                {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::UnsupportedFeature,
                        "set_luminances not advertised",
                    );
                    return;
                }
                let mut guard = data.0.lock().unwrap();
                if guard.luminances.is_some() {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::AlreadySet,
                        "luminances already set",
                    );
                    return;
                }
                if max_lum < reference_lum || reference_lum == 0 {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::InvalidLuminance,
                        "invalid luminance range",
                    );
                    return;
                }
                guard.luminances = Some(Luminances {
                    min_lum,
                    max_lum,
                    reference_lum,
                });
            }

            wp_image_description_creator_params_v1::Request::SetMasteringDisplayPrimaries {
                r_x,
                r_y,
                g_x,
                g_y,
                b_x,
                b_y,
                w_x,
                w_y,
            } => {
                if !state
                    .color_management_state()
                    .capabilities()
                    .supports_feature(
                        wp_color_manager_v1::Feature::SetMasteringDisplayPrimaries,
                    )
                {
                    resource.post_error(
                        wp_image_description_creator_params_v1::Error::UnsupportedFeature,
                        "set_mastering_display_primaries not advertised",
                    );
                    return;
                }
                let mut guard = data.0.lock().unwrap();
                guard.mastering_primaries = Some(Chromaticities {
                    r: (r_x, r_y),
                    g: (g_x, g_y),
                    b: (b_x, b_y),
                    w: (w_x, w_y),
                });
            }

            wp_image_description_creator_params_v1::Request::SetMasteringLuminance {
                min_lum,
                max_lum,
            } => {
                let mut guard = data.0.lock().unwrap();
                guard.mastering_luminance = Some(MasteringLuminance { min_lum, max_lum });
            }

            wp_image_description_creator_params_v1::Request::SetMaxCll { max_cll } => {
                let mut guard = data.0.lock().unwrap();
                guard.max_cll = Some(max_cll);
            }

            wp_image_description_creator_params_v1::Request::SetMaxFall { max_fall } => {
                let mut guard = data.0.lock().unwrap();
                guard.max_fall = Some(max_fall);
            }

            _ => unreachable!(),
        }
    }
}

// ===========================================================================
// wp_image_description_creator_icc_v1
// ===========================================================================

impl<D> Dispatch<WpImageDescriptionCreatorIccV1, IccCreatorBuilder, D> for ColorManagementState
where
    D: Dispatch<WpImageDescriptionCreatorIccV1, IccCreatorBuilder>
        + Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionCreatorIccV1,
        request: wp_image_description_creator_icc_v1::Request,
        data: &IccCreatorBuilder,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_image_description_creator_icc_v1::Request::Create { image_description } => {
                let inner = {
                    let mut guard = data.0.lock().unwrap();
                    if guard.consumed {
                        resource.post_error(
                            wp_image_description_creator_icc_v1::Error::AlreadySet,
                            "create called twice",
                        );
                        return;
                    }
                    guard.consumed = true;
                    std::mem::take(&mut *guard)
                };

                let bytes = match inner.icc_bytes {
                    Some(b) => b,
                    None => {
                        resource.post_error(
                            wp_image_description_creator_icc_v1::Error::IncompleteSet,
                            "set_icc_file required",
                        );
                        return;
                    }
                };

                let description = ImageDescription {
                    primaries: None,
                    transfer_function: None,
                    luminances: None,
                    mastering_primaries: None,
                    mastering_luminance: None,
                    max_cll: None,
                    max_fall: None,
                    icc: Some(IccProfile {
                        bytes: Arc::new(bytes),
                    }),
                    windows_scrgb: false,
                    identity: 0,
                };
                let arc = state
                    .color_management_state()
                    .interner_mut()
                    .intern(description);
                state.new_image_description(&arc);
                let identity = arc.identity;
                // Per spec, ICC-creator descriptions do NOT allow get_information
                // (the client uploaded the ICC profile, so they already have it).
                let res = data_init.init(
                    image_description,
                    DescriptionResourceData::ready(arc, /* allow_get_information */ false),
                );
                send_ready(&res, identity);
            }

            wp_image_description_creator_icc_v1::Request::SetIccFile {
                icc_profile,
                offset,
                length,
            } => {
                let mut guard = data.0.lock().unwrap();
                if guard.icc_bytes.is_some() {
                    resource.post_error(
                        wp_image_description_creator_icc_v1::Error::AlreadySet,
                        "ICC file already set",
                    );
                    return;
                }

                match read_icc_fd(icc_profile, offset, length) {
                    Ok(bytes) => guard.icc_bytes = Some(bytes),
                    Err(IccReadError::BadFd) => {
                        resource.post_error(
                            wp_image_description_creator_icc_v1::Error::BadFd,
                            "ICC fd not seekable/readable",
                        );
                    }
                    Err(IccReadError::BadSize) => {
                        resource.post_error(
                            wp_image_description_creator_icc_v1::Error::BadSize,
                            "ICC length out of range",
                        );
                    }
                    Err(IccReadError::OutOfFile) => {
                        resource.post_error(
                            wp_image_description_creator_icc_v1::Error::OutOfFile,
                            "offset+length exceeds file size",
                        );
                    }
                }
            }

            _ => unreachable!(),
        }
    }
}

#[derive(Debug)]
enum IccReadError {
    BadFd,
    BadSize,
    OutOfFile,
}

/// Read `length` bytes starting at `offset` from `fd`, validating against the
/// ICC profile size limits the protocol enforces.
///
/// Per spec: bad_size if length is 0 or > 4 MiB; out_of_file if offset+length
/// exceeds the file's size; bad_fd if the fd isn't readable + seekable.
fn read_icc_fd(fd: OwnedFd, offset: u32, length: u32) -> Result<Vec<u8>, IccReadError> {
    const MAX_ICC: u32 = 4 * 1024 * 1024;
    if length == 0 || length > MAX_ICC {
        return Err(IccReadError::BadSize);
    }

    let mut file = std::fs::File::from(fd);
    let file_size = file.metadata().map_err(|_| IccReadError::BadFd)?.len();
    if (offset as u64) + (length as u64) > file_size {
        return Err(IccReadError::OutOfFile);
    }
    file.seek(SeekFrom::Start(offset as u64))
        .map_err(|_| IccReadError::BadFd)?;
    let mut buf = vec![0u8; length as usize];
    file.read_exact(&mut buf).map_err(|_| IccReadError::BadFd)?;
    Ok(buf)
}

// ===========================================================================
// wp_image_description_v1
// ===========================================================================

impl<D> Dispatch<WpImageDescriptionV1, DescriptionResourceData, D> for ColorManagementState
where
    D: Dispatch<WpImageDescriptionV1, DescriptionResourceData>
        + Dispatch<WpImageDescriptionInfoV1, ()>
        + ColorManagementHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &WpImageDescriptionV1,
        request: wp_image_description_v1::Request,
        data: &DescriptionResourceData,
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_image_description_v1::Request::Destroy => {}

            wp_image_description_v1::Request::GetInformation { information } => {
                if !data.allow_get_information {
                    resource.post_error(
                        wp_image_description_v1::Error::NoInformation,
                        "get_information not allowed for this description",
                    );
                    return;
                }
                let arc = match data.description() {
                    Some(a) => a,
                    None => {
                        resource.post_error(
                            wp_image_description_v1::Error::NotReady,
                            "description not ready",
                        );
                        return;
                    }
                };
                let info = data_init.init(information, ());
                // Queue the information events + `done()` for emission outside
                // this dispatch callback. Sending them inline would panic
                // wayland-backend (the destructor `done()` event removes the
                // freshly-init'd `info` from the client map before the
                // post-callback hook can finish setting its user_data).
                state.color_management_state().queue_info_emission(info, arc);
            }

            _ => unreachable!(),
        }
    }
}

/// Emit every applicable information event followed by `done()`.
///
/// Called from [`super::ColorManagementState::flush_pending`] outside the
/// dispatch callback that init'd `info`. Calling this from inside dispatch
/// would panic wayland-backend (`done()` is a destructor event).
pub(super) fn emit_information_events(info: &WpImageDescriptionInfoV1, desc: &ImageDescription) {
    match desc.primaries {
        Some(PrimariesDef::Named(p)) => info.primaries_named(p),
        Some(PrimariesDef::Custom(c)) => {
            info.primaries(c.r.0, c.r.1, c.g.0, c.g.1, c.b.0, c.b.1, c.w.0, c.w.1)
        }
        None => {}
    }
    match desc.transfer_function {
        Some(TransferFunctionDef::Named(tf)) => info.tf_named(tf),
        Some(TransferFunctionDef::Power(eexp)) => info.tf_power(eexp),
        None => {}
    }
    if let Some(l) = desc.luminances {
        info.luminances(l.min_lum, l.max_lum, l.reference_lum);
    }
    if let Some(c) = desc.mastering_primaries {
        info.target_primaries(c.r.0, c.r.1, c.g.0, c.g.1, c.b.0, c.b.1, c.w.0, c.w.1);
    }
    if let Some(l) = desc.mastering_luminance {
        info.target_luminance(l.min_lum, l.max_lum);
    }
    if let Some(c) = desc.max_cll {
        info.target_max_cll(c);
    }
    if let Some(f) = desc.max_fall {
        info.target_max_fall(f);
    }
    // ICC fd echo intentionally not implemented in Phase 3.1 — would require
    // memfd_create + writing bytes + sending the fd. Matches Hyprland's TODO
    // here. SDR/HDR clients don't depend on it.
}

// ===========================================================================
// wp_image_description_info_v1 — fire-and-forget
// ===========================================================================

impl<D> Dispatch<WpImageDescriptionInfoV1, (), D> for ColorManagementState
where
    D: Dispatch<WpImageDescriptionInfoV1, ()> + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &WpImageDescriptionInfoV1,
        _request: <WpImageDescriptionInfoV1 as Resource>::Request,
        _data: &(),
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        // wp_image_description_info_v1 has no requests in v2 — clients only
        // consume events. Nothing to do.
    }
}

// ===========================================================================
// wp_image_description_reference_v1
// ===========================================================================

impl<D> Dispatch<WpImageDescriptionReferenceV1, ReferenceResourceData, D> for ColorManagementState
where
    D: Dispatch<WpImageDescriptionReferenceV1, ReferenceResourceData> + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        _resource: &WpImageDescriptionReferenceV1,
        request: wp_image_description_reference_v1::Request,
        _data: &ReferenceResourceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_image_description_reference_v1::Request::Destroy => {}
            _ => unreachable!(),
        }
    }
}
