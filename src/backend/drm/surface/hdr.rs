// SPDX-License-Identifier: MIT OR Apache-2.0
//
// Per-connector HDR signaling state for atomic DRM surfaces.
//
// HDR signaling on KMS-atomic drivers requires two connector properties to be
// committed together as part of the same atomic request:
//
//   * `Colorspace` (enum) — set to `BT2020_RGB` or `DCI-P3_RGB_D65` to switch
//     the panel into a wide-gamut interpretation of the framebuffer.
//   * `HDR_OUTPUT_METADATA` (blob) — references a property blob containing a
//     `struct hdr_output_metadata` (CTA-861.3 / BT.2100) describing the source
//     content's EOTF (e.g. SMPTE ST 2084 / PQ) and mastering luminance.
//
// Writing these via the legacy `set_property` ioctl puts them in pending atomic
// state but does not actually transmit the InfoFrame to the panel firmware until
// an atomic commit affecting the connector fires. Compositors that drive their
// render loop through smithay's atomic surface (e.g. cosmic-comp via
// `GbmDrmCompositor`) need the values included in *every* atomic commit so the
// panel-side HDR state survives across every flip — otherwise blob refs get
// reset to 0 on the next render commit and the panel falls back to SDR.
//
// This module gives compositors a way to attach per-connector HDR state to an
// `AtomicDrmSurface`; smithay then includes the props in every atomic commit
// it builds for that surface.

/// Per-connector HDR signaling values to include in every atomic commit on a
/// surface. Values are raw (u64) at this level — callers (typically a
/// compositor) resolve symbolic enum names like `"BT2020_RGB"` to the underlying
/// `u64` via the connector's `property::ValueType::Enum` info, and create the
/// metadata blob via `Device::create_property_blob`.
///
/// `colorspace_value = 0` and `metadata_blob_id = 0` together mean "back to
/// SDR" — same as never having set HDR on the connector. Clearing one without
/// the other can leave the panel in a wedged half-HDR state on some firmwares.
///
/// The CRTC color pipeline fields (`degamma_lut_blob_id`, `ctm_blob_id`,
/// `gamma_lut_blob_id`) are an OPTIONAL hardware-accelerated path: when
/// supplied, smithay stages the corresponding CRTC properties (`DEGAMMA_LUT`,
/// `CTM`, `GAMMA_LUT`) in every atomic commit alongside the connector
/// Colorspace + HDR_OUTPUT_METADATA. The kernel runs the operations in
/// fixed-function display-engine hardware after composition / before scanout,
/// freeing the GPU shader from doing per-pixel sRGB-decode + matrix +
/// PQ-encode every frame. When all three blob IDs are `None`, smithay leaves
/// the CRTC color pipeline alone (whatever the kernel last set), and the
/// caller is expected to do the encode in a postprocess shader instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HdrState {
    /// Raw value for the `Colorspace` enum property on the connector.
    /// Look up via `PropMapping::conn_prop_handle(conn, "Colorspace")`,
    /// then resolve to a `u64` from the connector's `EnumValues` list.
    /// Pass `0` (`Default`) to signal SDR / Rec.709.
    pub colorspace_value: u64,

    /// Blob ID of the `HDR_OUTPUT_METADATA` property. The caller owns the
    /// blob's lifetime — create with `Device::create_property_blob(&bytes)`,
    /// destroy with `Device::destroy_property_blob(id)` when no longer used.
    /// Pass `0` to clear (kernel treats blob 0 as "no override").
    pub metadata_blob_id: u64,

    /// Optional. Blob ID of a CRTC `DEGAMMA_LUT` blob (array of
    /// `struct drm_color_lut`, RGB+reserved interleaved, kernel applies
    /// per-channel before the CTM). Pass `Some(0)` to clear an existing
    /// LUT, `None` to leave the property untouched.
    pub degamma_lut_blob_id: Option<u64>,

    /// Optional. Blob ID of a CRTC `CTM` blob (`struct drm_color_ctm` —
    /// 9-entry sign-magnitude S31.32 matrix, applied to linear RGB).
    pub ctm_blob_id: Option<u64>,

    /// Optional. Blob ID of a CRTC `GAMMA_LUT` blob — applied per-channel
    /// after the CTM, output goes to scanout.
    pub gamma_lut_blob_id: Option<u64>,
}

impl HdrState {
    /// Convenience: HDR state representing "no HDR" — Colorspace=0 (Default)
    /// and metadata blob cleared, plus all CRTC color pipeline blobs cleared
    /// to 0 (so any previously-staged hardware encode pipeline is torn down
    /// alongside the SDR signaling fallback).
    pub const fn sdr() -> Self {
        Self {
            colorspace_value: 0,
            metadata_blob_id: 0,
            degamma_lut_blob_id: Some(0),
            ctm_blob_id: Some(0),
            gamma_lut_blob_id: Some(0),
        }
    }

    /// Convenience: shader-only HDR (no hardware color pipeline). Same as
    /// the previous two-field `HdrState` — connector signaling only, no
    /// CRTC LUT/CTM staging. Use this when the kernel's color pipeline
    /// props aren't exposed (older drivers) and the caller will run the
    /// encode in a postprocess shader.
    pub const fn signaling_only(colorspace_value: u64, metadata_blob_id: u64) -> Self {
        Self {
            colorspace_value,
            metadata_blob_id,
            degamma_lut_blob_id: None,
            ctm_blob_id: None,
            gamma_lut_blob_id: None,
        }
    }
}
