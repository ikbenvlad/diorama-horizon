//! How a surface is lit in a map style: out in the open by the sun, the sky, the shade where blocks
//! meet, the block's own glow, a little brightness with height, and darker sides; in a cave slice by
//! the game's own light.

use crate::style::MapStyle;

/// Cave light up to here is kept as it is; above, it eases toward [`CAVE_CEILING`].
const CAVE_KNEE: f32 = 0.5;
const CAVE_CEILING: f32 = 0.8;

pub fn luminance(r: f32, g: f32, b: f32) -> f32 {
    0.2126 * r + 0.7152 * g + 0.0722 * b
}

/// The light falling on a surface in the open, in linear light: `lambert` is the sun on it
/// relative to a level top face (0 where the sun is behind it), `shadowed` whether something
/// stands between it and the sun, `ao` the corner occlusion, `sky` how much open sky it sees,
/// `emission` its own glow (0..1), `facing` how much its side is darkened (1 for a top) and `y`
/// its height.
#[allow(clippy::too_many_arguments)]
pub fn surface(
    style: &MapStyle,
    lambert: f64,
    shadowed: bool,
    ao: f32,
    sky: f32,
    emission: f32,
    facing: f32,
    y: f64,
) -> [f32; 3] {
    let ao_sun = (1.0 - style.corner_occlusion * (1.0 - ao)).max(0.0);
    let ao_sky = ao.min(ao_sun);
    let direct = if lambert > 0.0 && !shadowed {
        (f64::from(style.sun_strength) * lambert) as f32 * ao_sun
    } else {
        0.0
    };
    let mut ambient = style.ambient * ao_sky;
    if style.sky_occlusion > 0.0 {
        // Less sky overhead, less ambient light: under trees, down in valleys and at the foot of walls.
        ambient *= 1.0 - style.sky_occlusion * (1.0 - (0.15 + 0.85 * sky));
    }
    let (sky_tint, sun_tint) = (style.shadow_tint, style.sun_tint);
    let mut r = ambient * sky_tint[0] + direct * sun_tint[0];
    let mut g = ambient * sky_tint[1] + direct * sun_tint[1];
    let mut b = ambient * sky_tint[2] + direct * sun_tint[2];
    if style.height_shade != 0.0 {
        let above = ((y - 64.0) / 64.0).clamp(-1.0, 1.0) as f32;
        let f = 1.0 + style.height_shade * above;
        r *= f;
        g *= f;
        b *= f;
    }
    if emission > 0.0 {
        let glow = 0.35 + emission * 0.85;
        r = r.max(glow);
        g = g.max(glow);
        b = b.max(glow);
    }
    [r * facing, g * facing, b * facing]
}

/// How bright block and sky light make water in a cave slice.
pub fn cave_water(block_light: f32, sky_light: u8) -> f32 {
    0.42 + (block_light.max(f32::from(sky_light) * 0.8) / 15.0).powf(1.5) * 0.8
}

/// The light on a surface in a cave slice, lit by the game's own light: block and sky light levels
/// (0..15) in the open cell in front of it, its corner occlusion, how much depth below the cut dims
/// it (`fade`, 1 for none), its own glow (0..1) and how much its side is darkened (1 for a top).
pub fn cave(
    block_light: f32,
    sky_light: u8,
    ao: f32,
    fade: f32,
    emission: f32,
    facing: f32,
) -> [f32; 3] {
    let bl = block_light / 15.0;
    let sl = f32::from(sky_light) / 15.0 * 0.8;
    let warm = bl.powf(1.5);
    let cool = sl.powf(1.5);
    // Unlit caves stay readable; torches, lava and daylight make them glow.
    let base = 0.42;
    let mut r = (base * 0.92 + warm * 0.9 + cool * 0.6) * ao * fade;
    let mut g = (base * 0.96 + warm * 0.75 + cool * 0.65) * ao * fade;
    let mut b = (base * 1.08 + warm * 0.5 + cool * 0.75) * ao * fade;
    // Bright light eases toward a ceiling instead of washing blocks out, keeping its colour.
    let peak = r.max(g).max(b);
    if peak > CAVE_KNEE {
        let eased = CAVE_KNEE
            + (CAVE_CEILING - CAVE_KNEE)
                * (1.0 - (-(peak - CAVE_KNEE) / (CAVE_CEILING - CAVE_KNEE)).exp());
        r *= eased / peak;
        g *= eased / peak;
        b *= eased / peak;
    }
    if emission > 0.0 {
        let glow = 0.35 + emission * 0.85;
        r = r.max(glow);
        g = g.max(glow);
        b = b.max(glow);
    }
    [r * facing, g * facing, b * facing]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sun_brightens_and_shadow_darkens() {
        let s = MapStyle::DEFAULT;
        let lit = surface(&s, 1.0, false, 1.0, 1.0, 0.0, 1.0, 64.0);
        let dark = surface(&s, 1.0, true, 1.0, 1.0, 0.0, 1.0, 64.0);
        assert!(luminance(lit[0], lit[1], lit[2]) > 2.0 * luminance(dark[0], dark[1], dark[2]));
        let glow = surface(&s, 1.0, true, 1.0, 1.0, 1.0, 1.0, 64.0);
        assert!(glow.iter().all(|c| *c >= 1.2));
    }

    #[test]
    fn caves_stay_readable_and_bright_light_eases() {
        let dark = cave(0.0, 0, 1.0, 1.0, 0.0, 1.0);
        assert!(dark[0] > 0.3);
        let bright = cave(15.0, 15, 1.0, 1.0, 0.0, 1.0);
        assert!(bright.iter().all(|c| *c <= CAVE_CEILING + 1e-6));
    }
}
