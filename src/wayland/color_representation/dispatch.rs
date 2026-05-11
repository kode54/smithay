//! Dispatch implementations for `wp_color_representation_v1`.

use wayland_protocols::wp::color_representation::v1::server::{
    wp_color_representation_manager_v1::{self, WpColorRepresentationManagerV1},
    wp_color_representation_surface_v1::{self, WpColorRepresentationSurfaceV1},
};
use wayland_server::{
    Client, DataInit, Dispatch, DisplayHandle, GlobalDispatch, New, Resource, WEnum,
    Weak as WaylandWeak, backend::ClientId, protocol::wl_surface::WlSurface,
};

use super::{
    ColorRepresentationHandler, ColorRepresentationState, ColorRepresentationSurfaceCachedState,
    ColorRepresentationSurfaceData,
};
use crate::wayland::compositor;

/// User data on a `wp_color_representation_surface_v1` resource.
#[derive(Debug)]
pub struct SurfaceResourceData {
    /// The `wl_surface` this color-representation interface is paired with.
    pub surface: WaylandWeak<WlSurface>,
}

// ===========================================================================
// wp_color_representation_manager_v1 — singleton manager
// ===========================================================================

impl<D> GlobalDispatch<WpColorRepresentationManagerV1, (), D> for ColorRepresentationState
where
    D: GlobalDispatch<WpColorRepresentationManagerV1, ()>
        + Dispatch<WpColorRepresentationManagerV1, ()>
        + ColorRepresentationHandler
        + 'static,
{
    fn bind(
        state: &mut D,
        _: &DisplayHandle,
        _: &Client,
        resource: New<WpColorRepresentationManagerV1>,
        _: &(),
        data_init: &mut DataInit<'_, D>,
    ) {
        let manager = data_init.init(resource, ());
        let caps = state.color_representation_state().capabilities().clone();

        for alpha in caps.alpha_modes() {
            manager.supported_alpha_mode(*alpha);
        }
        for (coefficients, range) in caps.coefficient_range_pairs() {
            manager.supported_coefficients_and_ranges(*coefficients, *range);
        }
        manager.done();
    }
}

impl<D> Dispatch<WpColorRepresentationManagerV1, (), D> for ColorRepresentationState
where
    D: Dispatch<WpColorRepresentationManagerV1, ()>
        + Dispatch<WpColorRepresentationSurfaceV1, SurfaceResourceData>
        + ColorRepresentationHandler
        + 'static,
{
    fn request(
        _state: &mut D,
        _client: &Client,
        manager: &WpColorRepresentationManagerV1,
        request: wp_color_representation_manager_v1::Request,
        _: &(),
        _dh: &DisplayHandle,
        data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_representation_manager_v1::Request::Destroy => {}

            wp_color_representation_manager_v1::Request::GetSurface { id, surface } => {
                let already_attached = compositor::with_states(&surface, |states| {
                    states
                        .data_map
                        .insert_if_missing_threadsafe(ColorRepresentationSurfaceData::new);
                    let data = states
                        .data_map
                        .get::<ColorRepresentationSurfaceData>()
                        .unwrap();
                    data.is_resource_attached_or_attach()
                });
                if already_attached {
                    manager.post_error(
                        wp_color_representation_manager_v1::Error::SurfaceExists,
                        "wl_surface already has wp_color_representation_surface_v1",
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

            _ => unreachable!(),
        }
    }
}

// ===========================================================================
// wp_color_representation_surface_v1 — per-surface state setters
// ===========================================================================

impl<D> Dispatch<WpColorRepresentationSurfaceV1, SurfaceResourceData, D>
    for ColorRepresentationState
where
    D: Dispatch<WpColorRepresentationSurfaceV1, SurfaceResourceData>
        + ColorRepresentationHandler
        + 'static,
{
    fn request(
        state: &mut D,
        _client: &Client,
        resource: &WpColorRepresentationSurfaceV1,
        request: wp_color_representation_surface_v1::Request,
        data: &SurfaceResourceData,
        _dh: &DisplayHandle,
        _data_init: &mut DataInit<'_, D>,
    ) {
        match request {
            wp_color_representation_surface_v1::Request::Destroy => {
                if let Ok(surface) = data.surface.upgrade() {
                    compositor::with_states(&surface, |states| {
                        if let Some(d) = states.data_map.get::<ColorRepresentationSurfaceData>() {
                            d.detach();
                        }
                        // Per spec: destroy reverts surface state to compositor
                        // defaults. Clear pending so the next commit reflects that.
                        let mut guard = states
                            .cached_state
                            .get::<ColorRepresentationSurfaceCachedState>();
                        let pending = guard.pending();
                        pending.alpha_mode = None;
                        pending.coefficients = None;
                        pending.range = None;
                        pending.chroma_location = None;
                    });
                }
            }

            wp_color_representation_surface_v1::Request::SetAlphaMode { alpha_mode } => {
                let alpha_mode = match alpha_mode {
                    WEnum::Value(v) => v,
                    WEnum::Unknown(_) => {
                        resource.post_error(
                            wp_color_representation_surface_v1::Error::AlphaMode,
                            "unknown alpha_mode enum value",
                        );
                        return;
                    }
                };
                if !state
                    .color_representation_state()
                    .capabilities()
                    .supports_alpha_mode(alpha_mode)
                {
                    resource.post_error(
                        wp_color_representation_surface_v1::Error::AlphaMode,
                        "alpha_mode not advertised by compositor",
                    );
                    return;
                }
                if let Ok(surface) = data.surface.upgrade() {
                    compositor::with_states(&surface, |states| {
                        states
                            .cached_state
                            .get::<ColorRepresentationSurfaceCachedState>()
                            .pending()
                            .alpha_mode = Some(alpha_mode);
                    });
                }
            }

            wp_color_representation_surface_v1::Request::SetCoefficientsAndRange {
                coefficients,
                range,
            } => {
                let coefficients = match coefficients {
                    WEnum::Value(v) => v,
                    WEnum::Unknown(_) => {
                        resource.post_error(
                            wp_color_representation_surface_v1::Error::Coefficients,
                            "unknown coefficients enum value",
                        );
                        return;
                    }
                };
                let range = match range {
                    WEnum::Value(v) => v,
                    WEnum::Unknown(_) => {
                        resource.post_error(
                            wp_color_representation_surface_v1::Error::Coefficients,
                            "unknown range enum value",
                        );
                        return;
                    }
                };
                if !state
                    .color_representation_state()
                    .capabilities()
                    .supports_coefficients_and_range(coefficients, range)
                {
                    resource.post_error(
                        wp_color_representation_surface_v1::Error::Coefficients,
                        "(coefficients, range) pair not advertised by compositor",
                    );
                    return;
                }
                if let Ok(surface) = data.surface.upgrade() {
                    compositor::with_states(&surface, |states| {
                        let mut guard = states
                            .cached_state
                            .get::<ColorRepresentationSurfaceCachedState>();
                        let pending = guard.pending();
                        pending.coefficients = Some(coefficients);
                        pending.range = Some(range);
                    });
                }
            }

            wp_color_representation_surface_v1::Request::SetChromaLocation { chroma_location } => {
                let chroma_location = match chroma_location {
                    WEnum::Value(v) => v,
                    WEnum::Unknown(_) => {
                        resource.post_error(
                            wp_color_representation_surface_v1::Error::ChromaLocation,
                            "unknown chroma_location enum value",
                        );
                        return;
                    }
                };
                if let Ok(surface) = data.surface.upgrade() {
                    compositor::with_states(&surface, |states| {
                        states
                            .cached_state
                            .get::<ColorRepresentationSurfaceCachedState>()
                            .pending()
                            .chroma_location = Some(chroma_location);
                    });
                }
            }

            _ => unreachable!(),
        }
    }

    fn destroyed(
        _state: &mut D,
        _client: ClientId,
        _resource: &WpColorRepresentationSurfaceV1,
        data: &SurfaceResourceData,
    ) {
        // Re-detach in case the client closed without sending Destroy.
        if let Ok(surface) = data.surface.upgrade() {
            compositor::with_states(&surface, |states| {
                if let Some(d) = states.data_map.get::<ColorRepresentationSurfaceData>() {
                    d.detach();
                }
            });
        }
    }
}
