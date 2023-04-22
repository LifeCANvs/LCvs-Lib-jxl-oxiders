pub mod illuminant {
    pub const D65: [f32; 2] = [0.3127, 0.329];
    pub const E: [f32; 2] = [1.0 / 3.0, 1.0 / 3.0];
    pub const DCI: [f32; 2] = [0.314, 0.351];

    /// CIE illuminant D50.
    ///
    /// xy-chromaticity value is computed so that the resulting `chad` tag matches that of libjxl.
    pub(crate) const D50: [f32; 2] = [0.345669, 0.358496];
}

#[allow(clippy::excessive_precision)]
pub mod primaries {
    pub const SRGB: [[f32; 2]; 3] = [
        [0.639998686, 0.330010138],
        [0.300003784, 0.600003357],
        [0.150002046, 0.059997204],
    ];

    pub const BT2100: [[f32; 2]; 3] = [
        [0.708, 0.292],
        [0.170, 0.797],
        [0.131, 0.046],
    ];

    pub const P3: [[f32; 2]; 3] = [
        [0.680, 0.320],
        [0.265, 0.690],
        [0.150, 0.060],
    ];
}
