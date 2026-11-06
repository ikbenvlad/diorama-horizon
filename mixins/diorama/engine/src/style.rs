//! How the map looks: camera angle, colour grade, lighting, outlines and water.

/// A map style. Switching to a style whose tiles look different re-bakes every tile.
#[derive(Clone, Debug, PartialEq)]
pub struct MapStyle {
    pub name: &'static str,
    pub pitch_degrees: f64,
    pub saturation: f32,
    pub exposure: f32,
    /// Colour of direct sunlight, multiplied per channel.
    pub sun_tint: [f32; 3],
    /// Colour of the ambient sky light that fills shadows.
    pub shadow_tint: [f32; 3],
    /// How much to darken pixels next to a much nearer surface, 0 for none.
    pub outline: f32,
    /// Brightness spread between neighbouring blocks of the same kind, 0 for none.
    pub block_variation: f32,
    /// Round the fog edge off instead of following the loaded chunks.
    pub rounded_fog: bool,
    /// Water absorbs each colour by depth, so shallows look turquoise and deep water blue.
    pub clear_water: bool,
    /// How much a spot that sees little sky loses of its ambient light, 0 for none.
    pub sky_occlusion: f32,
    /// The sun's height above the horizon in degrees; lower casts longer shadows.
    pub sun_elevation: f64,
    /// Sky light that fills shadows; lower makes shadows deeper.
    pub ambient: f32,
    /// Direct sunlight on a top face.
    pub sun_strength: f32,
    /// How much brighter high ground is than low ground, 0 for none.
    pub height_shade: f32,
    /// How much corner shade dims the sunlight too: 0 only the sky light, 1 as much.
    pub corner_occlusion: f32,
}

impl MapStyle {
    pub const MIN_SUN_ELEVATION: f64 = 25.0;
    pub const MAX_SUN_ELEVATION: f64 = 60.0;

    pub const BASELINE: MapStyle = MapStyle {
        name: "Baseline",
        pitch_degrees: 60.0,
        saturation: 1.18,
        exposure: 1.06,
        sun_tint: [1.06, 1.0, 0.9],
        shadow_tint: [0.9, 0.95, 1.08],
        outline: 0.0,
        block_variation: 0.0,
        rounded_fog: false,
        clear_water: false,
        sky_occlusion: 0.0,
        sun_elevation: 51.3,
        ambient: 0.5,
        sun_strength: 0.484,
        height_shade: 0.0,
        corner_occlusion: 0.0,
    };

    /// The look before depth: outlines for crisp silhouettes and a rounded fog edge.
    pub const FLAT: MapStyle = MapStyle {
        name: "Flat",
        outline: 0.38,
        rounded_fog: true,
        ..MapStyle::BASELINE
    };

    /// What players get: [`Self::FLAT`] made to read in depth.
    pub const DEFAULT: MapStyle = MapStyle {
        name: "Default",
        pitch_degrees: 50.0,
        saturation: 1.35,
        exposure: 1.16,
        sun_tint: [1.14, 1.0, 0.8],
        shadow_tint: [0.74, 0.86, 1.18],
        sky_occlusion: 0.85,
        sun_elevation: 38.0,
        ambient: 0.34,
        sun_strength: 0.62,
        height_shade: 0.22,
        corner_occlusion: 1.5,
        clear_water: true,
        ..MapStyle::FLAT
    };

    /// True when tiles baked under one style look the same under the other.
    pub fn looks_like(&self, other: &MapStyle) -> bool {
        let a = MapStyle {
            name: "",
            rounded_fog: false,
            ..self.clone()
        };
        let b = MapStyle {
            name: "",
            rounded_fog: false,
            ..other.clone()
        };
        a == b
    }

    /// A short fingerprint of everything that changes baked pixels, for the store's folder name.
    pub fn fingerprint(&self) -> u32 {
        let mut h: u32 = 0x811C_9DC5;
        let mut eat = |v: u64| {
            for b in v.to_le_bytes() {
                h ^= u32::from(b);
                h = h.wrapping_mul(0x0100_0193);
            }
        };
        eat(self.pitch_degrees.to_bits());
        for f in [
            self.saturation,
            self.exposure,
            self.outline,
            self.block_variation,
            self.sky_occlusion,
            self.ambient,
            self.sun_strength,
            self.height_shade,
            self.corner_occlusion,
        ]
        .into_iter()
        .chain(self.sun_tint)
        .chain(self.shadow_tint)
        {
            eat(u64::from(f.to_bits()));
        }
        eat(self.sun_elevation.to_bits());
        eat(u64::from(self.clear_water));
        h
    }
}

impl Default for MapStyle {
    fn default() -> Self {
        MapStyle::DEFAULT
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_differs_from_flat_and_ignores_names() {
        assert!(!MapStyle::DEFAULT.looks_like(&MapStyle::FLAT));
        let renamed = MapStyle {
            name: "x",
            rounded_fog: false,
            ..MapStyle::DEFAULT
        };
        assert!(renamed.looks_like(&MapStyle::DEFAULT));
        assert_eq!(renamed.fingerprint(), MapStyle::DEFAULT.fingerprint());
        assert_ne!(
            MapStyle::FLAT.fingerprint(),
            MapStyle::DEFAULT.fingerprint()
        );
    }
}
