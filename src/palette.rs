//! The palette colours Rust itself uses (the DWM border, the controller art's SVGs); the Slint
//! markup has its own Theme (ui/theme.slint) with the same values.

/// An sRGB colour with straight alpha, each channel 0..=1.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Color {
    pub r: f32,
    pub g: f32,
    pub b: f32,
    pub a: f32,
}

impl Color {
    pub(crate) const WHITE: Color = Color::from_rgb(1.0, 1.0, 1.0);

    pub(crate) const fn from_rgb(r: f32, g: f32, b: f32) -> Color {
        Color { r, g, b, a: 1.0 }
    }

    pub(crate) const fn from_rgb8(r: u8, g: u8, b: u8) -> Color {
        Color::from_rgb(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0)
    }

    pub(crate) fn into_rgba8(self) -> [u8; 4] {
        [self.r, self.g, self.b, self.a].map(|c| (c.clamp(0.0, 1.0) * 255.0).round() as u8)
    }
}

pub(crate) fn rgb(value: u32) -> Color {
    Color::from_rgb8((value >> 16) as u8, (value >> 8) as u8, value as u8)
}

/// The colours Rust needs from the approved "Crystal refined" palette (2026-10-22).
#[derive(Clone, Copy)]
pub(crate) struct Colors {
    /// Input borders, and the window's DWM border.
    pub(crate) line: Color,
    pub(crate) ink: Color,
    pub(crate) sub: Color,
    pub(crate) accent: Color,
}

pub(crate) fn colors(light: bool) -> Colors {
    if light {
        Colors {
            line: rgb(0xC8D3DD),
            ink: rgb(0x142130),
            sub: rgb(0x52657A),
            accent: rgb(0x1F6F9C),
        }
    } else {
        Colors {
            line: rgb(0x2A3A49),
            ink: rgb(0xE9EFF5),
            sub: rgb(0x9DADBD),
            accent: rgb(0x8FCDEB),
        }
    }
}
