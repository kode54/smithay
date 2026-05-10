//! Image description data types and dedup interner for `wp_color_management_v1`.
//!
//! An [`ImageDescription`] is a finished, immutable color-encoding contract that a
//! client can attach to a surface (or that the compositor advertises on an output).
//! Descriptions can be parametric (named or custom primaries + transfer function +
//! optional luminance / mastering / CLL / FALL metadata) or ICC-profile-based.
//!
//! The protocol's `ready` events carry a 64-bit identity that clients use to compare
//! descriptions cheaply. We assign that identity by **dedup interning**: every newly
//! built description is structurally equality-compared against the existing
//! interned set; on hit we reuse the existing handle, on miss we push a new one and
//! assign `identity = previous_count + 1` (so identities are 1-indexed and monotonic
//! per `ColorManagementState`).
//!
//! Field units follow the protocol XML verbatim — chromaticity coordinates are
//! stored as `i32` scaled by 1,000,000, transfer-function power exponents as `u32`
//! scaled by 10,000, and the `min_lum` channel of luminance ranges as `u32` scaled
//! by 10,000 (max / reference / mastering-max / max_cll / max_fall are unscaled
//! `u32` in cd/m²). No float conversion happens at the data layer; that's the
//! consumer's job (smithay's render path or cosmic-comp). Storing raw integers
//! keeps interner equality exact and side-steps NaN / sub-ULP comparison hazards.

use std::sync::Arc;

use wayland_protocols::wp::color_management::v1::server::{
    wp_color_manager_v1::{Primaries as ProtoPrimaries, TransferFunction as ProtoTransferFunction},
};

/// Chromaticity coordinates of a color volume's three primaries plus its white point.
///
/// Each value is an `i32` representing CIE 1931 xy chromaticity scaled by 1,000,000
/// (so `0.6400` → `640_000`). This matches the wire format for the `set_primaries`,
/// `set_mastering_display_primaries`, `primaries`, and `target_primaries` requests
/// and events.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Chromaticities {
    /// Red primary `(x, y)` × 1,000,000.
    pub r: (i32, i32),
    /// Green primary `(x, y)` × 1,000,000.
    pub g: (i32, i32),
    /// Blue primary `(x, y)` × 1,000,000.
    pub b: (i32, i32),
    /// White point `(x, y)` × 1,000,000.
    pub w: (i32, i32),
}

/// How a description's primary color volume is specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PrimariesDef {
    /// One of the protocol's named primary sets (sRGB, BT.2020, DCI-P3, etc.).
    Named(ProtoPrimaries),
    /// Explicit chromaticities (× 1,000,000).
    Custom(Chromaticities),
}

/// How a description's transfer function is specified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum TransferFunctionDef {
    /// One of the protocol's named transfer functions (sRGB, ST.2084 PQ, HLG, etc.).
    Named(ProtoTransferFunction),
    /// Pure power curve. Stored as the exponent × 10,000 (so γ = 2.4 → `24_000`).
    Power(u32),
}

/// Primary color volume luminance range, in cd/m². `min_lum` is scaled by 10,000;
/// `max_lum` and `reference_lum` are unscaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Luminances {
    /// Minimum luminance × 10,000 (cd/m² × 10,000).
    pub min_lum: u32,
    /// Maximum luminance, cd/m².
    pub max_lum: u32,
    /// Reference white luminance, cd/m².
    pub reference_lum: u32,
}

/// Mastering display luminance range. `min` is scaled by 10,000; `max` is unscaled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct MasteringLuminance {
    /// Minimum mastering luminance × 10,000.
    pub min_lum: u32,
    /// Maximum mastering luminance, cd/m².
    pub max_lum: u32,
}

/// Bytes of an ICC v2 / v4 profile attached to a description.
///
/// Stored as an `Arc<Vec<u8>>` so cloning the description (which the interner does
/// freely) doesn't copy the profile bytes. Equality is byte-exact.
#[derive(Debug, Clone)]
pub struct IccProfile {
    /// Raw ICC bytes, exactly as read from the client's file descriptor.
    pub bytes: Arc<Vec<u8>>,
}

impl PartialEq for IccProfile {
    fn eq(&self, other: &Self) -> bool {
        // Cheap pointer-equality fast path; fall back to byte compare for distinct Arcs.
        Arc::ptr_eq(&self.bytes, &other.bytes) || self.bytes == other.bytes
    }
}
impl Eq for IccProfile {}

/// A finished, immutable image description.
///
/// All fields are normalized to the protocol's own integer encoding so equality
/// (and therefore interner dedup) is exact. To use a description in rendering,
/// convert these integer fields to floats at the consumer.
///
/// `identity` is assigned by [`ImageDescriptionInterner::intern`] and must NOT be
/// part of structural equality (two descriptions with the same parametric content
/// share an identity). The derived `PartialEq` excludes `identity` via the manual
/// impl below.
#[derive(Debug, Clone)]
pub struct ImageDescription {
    /// Primaries (named or explicit chromaticities). `None` means the description
    /// is pure ICC and primaries come from the profile.
    pub primaries: Option<PrimariesDef>,
    /// Transfer function (named or power). `None` means the description is pure
    /// ICC and the transfer function comes from the profile.
    pub transfer_function: Option<TransferFunctionDef>,
    /// Primary color volume luminances. Optional — `None` means use the named-TF
    /// defaults (e.g. PQ → 0–10,000 cd/m², SDR → 0.2–80 cd/m² @ 203 ref).
    pub luminances: Option<Luminances>,
    /// Mastering display primaries (× 1,000,000). Optional.
    pub mastering_primaries: Option<Chromaticities>,
    /// Mastering display luminance range. Optional.
    pub mastering_luminance: Option<MasteringLuminance>,
    /// Maximum content light level, cd/m². Optional.
    pub max_cll: Option<u32>,
    /// Maximum frame-average light level, cd/m². Optional.
    pub max_fall: Option<u32>,
    /// ICC profile bytes, if this description was built via the ICC creator.
    pub icc: Option<IccProfile>,
    /// `true` if built via `wp_color_manager_v1.create_windows_scrgb`. Carries
    /// implicit primaries (sRGB), transfer (extended-linear), and reference luminance
    /// per the protocol; we don't expand those into the explicit fields above so the
    /// flag survives interner round-trips.
    pub windows_scrgb: bool,
    /// 64-bit identity assigned by the interner. **Excluded from `PartialEq`.**
    pub identity: u64,
}

impl PartialEq for ImageDescription {
    fn eq(&self, other: &Self) -> bool {
        // Identity is intentionally excluded — two structurally-equal descriptions
        // share an identity by construction.
        self.primaries == other.primaries
            && self.transfer_function == other.transfer_function
            && self.luminances == other.luminances
            && self.mastering_primaries == other.mastering_primaries
            && self.mastering_luminance == other.mastering_luminance
            && self.max_cll == other.max_cll
            && self.max_fall == other.max_fall
            && self.icc == other.icc
            && self.windows_scrgb == other.windows_scrgb
    }
}
impl Eq for ImageDescription {}

impl ImageDescription {
    /// Returns `true` if this description carries any ICC profile data.
    #[inline]
    pub fn is_icc(&self) -> bool {
        self.icc.is_some()
    }

    /// Returns `true` if this description has a parametric form the renderer can
    /// consume directly (named or explicit primaries + named or power transfer).
    /// Pure-ICC descriptions return `false`; clients holding such a description
    /// should fall back to ICC rendering or surface a "no parametric" failure on
    /// `wp_color_management_surface_feedback_v1.get_preferred_parametric`.
    #[inline]
    pub fn is_parametric(&self) -> bool {
        self.primaries.is_some() && self.transfer_function.is_some()
    }

    /// The opaque 64-bit identity advertised over the protocol. Split on the wire
    /// as `(identity_hi, identity_lo)`; helper kept here so dispatch code doesn't
    /// duplicate the bit-fiddling.
    #[inline]
    pub fn identity_split(&self) -> (u32, u32) {
        ((self.identity >> 32) as u32, self.identity as u32)
    }
}

/// The default sRGB image description used when a client never sets one (or unsets
/// it on a surface).
///
/// Named sRGB primaries, named sRGB transfer, no luminance / mastering / CLL / FALL
/// metadata, no ICC. Identity is filled in by the interner.
fn srgb_default_template() -> ImageDescription {
    ImageDescription {
        primaries: Some(PrimariesDef::Named(ProtoPrimaries::Srgb)),
        transfer_function: Some(TransferFunctionDef::Named(ProtoTransferFunction::Srgb)),
        luminances: None,
        mastering_primaries: None,
        mastering_luminance: None,
        max_cll: None,
        max_fall: None,
        icc: None,
        windows_scrgb: false,
        identity: 0, // overwritten by intern
    }
}

/// Dedup interner for image descriptions.
///
/// Holds `Arc<ImageDescription>` so callers can cheaply share handles. New
/// descriptions are interned via [`Self::intern`]; if a structurally-equal
/// description already exists, the existing handle is returned and the input is
/// dropped. Otherwise a fresh identity is assigned and the new description is
/// stored.
///
/// Linear-scan equality is fine here: the population is small (one description
/// per distinct color contract a client cares about), creation rate is low (handful
/// per-session, not per-frame), and structural equality is cheap-ish.
#[derive(Debug)]
pub struct ImageDescriptionInterner {
    descriptions: Vec<Arc<ImageDescription>>,
}

impl ImageDescriptionInterner {
    /// Build a fresh interner pre-populated with the default sRGB description at
    /// `identity = 1`. Use [`Self::srgb_default`] to retrieve that handle.
    pub fn new() -> Self {
        let mut interner = Self {
            descriptions: Vec::with_capacity(8),
        };
        // Pre-populate sRGB at identity=1 so it's stable across sessions and
        // accessible without going through the full intern path.
        let mut srgb = srgb_default_template();
        srgb.identity = 1;
        interner.descriptions.push(Arc::new(srgb));
        interner
    }

    /// Intern a description, assigning it a fresh identity if it's not already
    /// present. The input's `identity` field is ignored — the returned handle's
    /// identity is authoritative.
    pub fn intern(&mut self, mut description: ImageDescription) -> Arc<ImageDescription> {
        if let Some(existing) = self
            .descriptions
            .iter()
            .find(|d| d.as_ref() == &description)
        {
            return Arc::clone(existing);
        }
        description.identity = self.descriptions.len() as u64 + 1;
        let handle = Arc::new(description);
        self.descriptions.push(Arc::clone(&handle));
        handle
    }

    /// Get the default sRGB description (pre-populated at construction, identity = 1).
    pub fn srgb_default(&self) -> Arc<ImageDescription> {
        Arc::clone(&self.descriptions[0])
    }

    /// Look up a description by identity. Returns `None` if no such description
    /// has been interned yet — useful for `wp_image_description_reference_v1`
    /// validation but not strictly required since we always hand out `Arc` handles
    /// directly.
    pub fn lookup(&self, identity: u64) -> Option<Arc<ImageDescription>> {
        self.descriptions
            .iter()
            .find(|d| d.identity == identity)
            .map(Arc::clone)
    }

    /// Total interned description count, for diagnostics / tests.
    #[inline]
    pub fn len(&self) -> usize {
        self.descriptions.len()
    }

    /// Whether the interner is empty. Always `false` after construction (sRGB is
    /// pre-populated), but provided for completeness.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.descriptions.is_empty()
    }
}

impl Default for ImageDescriptionInterner {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pq_bt2020() -> ImageDescription {
        ImageDescription {
            primaries: Some(PrimariesDef::Named(ProtoPrimaries::Bt2020)),
            transfer_function: Some(TransferFunctionDef::Named(ProtoTransferFunction::St2084Pq)),
            luminances: Some(Luminances {
                min_lum: 5,           // 0.0005 cd/m² × 10,000
                max_lum: 10_000,
                reference_lum: 203,
            }),
            mastering_primaries: None,
            mastering_luminance: None,
            max_cll: None,
            max_fall: None,
            icc: None,
            windows_scrgb: false,
            identity: 0,
        }
    }

    #[test]
    fn srgb_pre_populated_at_identity_one() {
        let interner = ImageDescriptionInterner::new();
        let srgb = interner.srgb_default();
        assert_eq!(srgb.identity, 1);
        assert_eq!(
            srgb.primaries,
            Some(PrimariesDef::Named(ProtoPrimaries::Srgb))
        );
        assert!(srgb.is_parametric());
        assert!(!srgb.is_icc());
    }

    #[test]
    fn intern_assigns_monotonic_identity() {
        let mut interner = ImageDescriptionInterner::new();
        let pq = interner.intern(pq_bt2020());
        assert_eq!(pq.identity, 2); // sRGB took 1
        assert_eq!(interner.len(), 2);
    }

    #[test]
    fn intern_dedups_structurally_equal_descriptions() {
        let mut interner = ImageDescriptionInterner::new();
        let pq1 = interner.intern(pq_bt2020());
        let pq2 = interner.intern(pq_bt2020());
        assert_eq!(pq1.identity, pq2.identity);
        assert!(Arc::ptr_eq(&pq1, &pq2));
        assert_eq!(interner.len(), 2); // sRGB + PQ, not three
    }

    #[test]
    fn intern_distinguishes_descriptions_differing_in_one_field() {
        let mut interner = ImageDescriptionInterner::new();
        let pq = interner.intern(pq_bt2020());
        let mut pq_with_cll = pq_bt2020();
        pq_with_cll.max_cll = Some(1000);
        let pq2 = interner.intern(pq_with_cll);
        assert_ne!(pq.identity, pq2.identity);
        assert_eq!(interner.len(), 3);
    }

    #[test]
    fn intern_ignores_identity_field_on_input() {
        let mut interner = ImageDescriptionInterner::new();
        let mut pq = pq_bt2020();
        pq.identity = 999; // should be overwritten
        let handle = interner.intern(pq);
        assert_eq!(handle.identity, 2); // 2, not 999
    }

    #[test]
    fn lookup_by_identity_returns_handle() {
        let mut interner = ImageDescriptionInterner::new();
        let pq = interner.intern(pq_bt2020());
        let looked_up = interner.lookup(pq.identity).expect("handle by identity");
        assert!(Arc::ptr_eq(&pq, &looked_up));
    }

    #[test]
    fn lookup_missing_identity_returns_none() {
        let interner = ImageDescriptionInterner::new();
        assert!(interner.lookup(42).is_none());
    }

    #[test]
    fn identity_split_is_high_then_low() {
        let mut desc = pq_bt2020();
        desc.identity = 0x0000_0001_0000_0002;
        let (hi, lo) = desc.identity_split();
        assert_eq!(hi, 1);
        assert_eq!(lo, 2);
    }

    #[test]
    fn icc_descriptions_compare_byte_exact() {
        let icc_a = IccProfile {
            bytes: Arc::new(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        };
        let icc_b = IccProfile {
            bytes: Arc::new(vec![0xDE, 0xAD, 0xBE, 0xEF]),
        };
        let icc_c = IccProfile {
            bytes: Arc::new(vec![0xDE, 0xAD, 0xBE, 0xF0]),
        };
        assert_eq!(icc_a, icc_b);
        assert_ne!(icc_a, icc_c);

        // Two ICC descriptions with byte-equal profiles dedup; differing bytes
        // produce distinct identities.
        let mut interner = ImageDescriptionInterner::new();
        let mut base = ImageDescription {
            primaries: None,
            transfer_function: None,
            luminances: None,
            mastering_primaries: None,
            mastering_luminance: None,
            max_cll: None,
            max_fall: None,
            icc: Some(icc_a),
            windows_scrgb: false,
            identity: 0,
        };
        let h1 = interner.intern(base.clone());
        let h2 = interner.intern(ImageDescription {
            icc: Some(icc_b),
            ..base.clone()
        });
        assert!(Arc::ptr_eq(&h1, &h2));

        base.icc = Some(icc_c);
        let h3 = interner.intern(base);
        assert_ne!(h1.identity, h3.identity);
    }

    #[test]
    fn is_parametric_false_for_pure_icc() {
        let icc_only = ImageDescription {
            primaries: None,
            transfer_function: None,
            luminances: None,
            mastering_primaries: None,
            mastering_luminance: None,
            max_cll: None,
            max_fall: None,
            icc: Some(IccProfile {
                bytes: Arc::new(vec![0; 128]),
            }),
            windows_scrgb: false,
            identity: 0,
        };
        assert!(!icc_only.is_parametric());
        assert!(icc_only.is_icc());
    }

    #[test]
    fn windows_scrgb_distinguishes_from_plain_srgb() {
        // windows_scrgb=true is its own description even with same other fields,
        // because the protocol carries semantics (extended-linear TF) that we
        // don't materialize into the explicit fields.
        let mut interner = ImageDescriptionInterner::new();
        let plain = interner.srgb_default();
        let scrgb = interner.intern(ImageDescription {
            primaries: Some(PrimariesDef::Named(ProtoPrimaries::Srgb)),
            transfer_function: Some(TransferFunctionDef::Named(ProtoTransferFunction::Srgb)),
            luminances: None,
            mastering_primaries: None,
            mastering_luminance: None,
            max_cll: None,
            max_fall: None,
            icc: None,
            windows_scrgb: true,
            identity: 0,
        });
        assert_ne!(plain.identity, scrgb.identity);
    }

    #[test]
    fn custom_primaries_distinguish_from_named() {
        let mut interner = ImageDescriptionInterner::new();
        let named = interner.intern(pq_bt2020());

        let mut custom_pq = pq_bt2020();
        custom_pq.primaries = Some(PrimariesDef::Custom(Chromaticities {
            r: (708_000, 292_000),
            g: (170_000, 797_000),
            b: (131_000, 46_000),
            w: (312_700, 329_000),
        }));
        let custom = interner.intern(custom_pq);
        assert_ne!(named.identity, custom.identity);
    }
}
