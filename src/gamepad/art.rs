//! Controller illustrations and button glyphs, drawn as SVG at runtime so the
//! pressed buttons, stick positions and trigger pulls can be lit up live.
//! All artwork here is original: simple paths on a 600x400 canvas.
use super::detect::{Btn, Family, Input};
use crate::palette::Color;
use std::fmt::Write;

pub(crate) fn hex(c: Color) -> String {
    let [r, g, b, _] = c.into_rgba8();
    format!("#{r:02x}{g:02x}{b:02x}")
}

fn mix(a: Color, b: Color, t: f32) -> Color {
    let t = t.clamp(0.0, 1.0);
    Color::from_rgb(a.r + (b.r - a.r) * t, a.g + (b.g - a.g) * t, a.b + (b.b - a.b) * t)
}

use crate::palette::rgb;

/// Face-button colours: the Xbox letter colours and the classic PlayStation
/// symbol colours.
pub(crate) fn face_color(family: Family, b: Btn) -> Color {
    if family.is_playstation() {
        match b {
            Btn::South => rgb(0x7FA9EE),
            Btn::East => rgb(0xEE6F6F),
            Btn::West => rgb(0xE68BC8),
            _ => rgb(0x3EC6A4),
        }
    } else if family.is_xbox() {
        match b {
            Btn::South => rgb(0x6CC24A),
            Btn::East => rgb(0xE9493F),
            Btn::West => rgb(0x3D8EEA),
            _ => rgb(0xF2C232),
        }
    } else {
        rgb(0xC9D2DA)
    }
}

/// The glyph drawn on a face button, centred on 0,0 and about 12 units tall.
fn face_glyph(family: Family, b: Btn) -> &'static str {
    if family.is_playstation() {
        return match b {
            Btn::South => r#"<path d="M-5.5 -5.5 L5.5 5.5 M5.5 -5.5 L-5.5 5.5"/>"#,
            Btn::East => r#"<circle r="6.2"/>"#,
            Btn::West => r#"<rect x="-5.6" y="-5.6" width="11.2" height="11.2" rx="0.6"/>"#,
            _ => r#"<path d="M0 -7 L6.6 4.6 L-6.6 4.6 Z"/>"#,
        };
    }
    // Nintendo prints A on the right and B at the bottom.
    let letter = match (family == Family::SwitchPro, b) {
        (false, Btn::South) | (true, Btn::East) => 'A',
        (false, Btn::East) | (true, Btn::South) => 'B',
        (false, Btn::West) | (true, Btn::North) => 'X',
        _ => 'Y',
    };
    match letter {
        'A' => r#"<path d="M-5 6 L0 -6 L5 6 M-3.1 1.8 H3.1"/>"#,
        'B' => r#"<path d="M-3.6 -6 V6 M-3.6 -6 H0.9 A2.9 2.9 0 0 1 0.9 -0.2 H-3.6 M0.9 -0.2 H1.3 A3.1 3.1 0 0 1 1.3 6 H-3.6"/>"#,
        'X' => r#"<path d="M-4.6 -6 L4.6 6 M4.6 -6 L-4.6 6"/>"#,
        _ => r#"<path d="M-4.8 -6 L0 0.2 L4.8 -6 M0 0.2 V6"/>"#,
    }
}

/// A 24x24 glyph for one face button (the bindings list's chips).
pub(crate) fn face_chip(family: Family, b: Btn, light: bool, active: bool) -> String {
    let color = face_color(family, b);
    let base = if active { hex(color) } else if light { "#25303a".into() } else { "#0e161e".into() };
    let ink = if active { "#0d1318".to_string() } else { hex(color) };
    let ring = if light { "#25303a" } else { "#33475a" };
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="-12 -12 24 24"><circle r="11" fill="{base}" stroke="{ring}" stroke-width="1"/><g fill="none" stroke="{ink}" stroke-width="2.1" stroke-linecap="round" stroke-linejoin="round" transform="scale(0.78)">{}</g></svg>"##,
        face_glyph(family, b)
    )
}

const PLUS: &str = "M-4 -11 H4 V-4 H11 V4 H4 V11 H-4 V4 H-11 V-4 H-4 Z";

/// A 24x24 D-pad with the given arms lit (all four for "the whole pad").
pub(crate) fn dpad_chip(lit: &[Btn], ink: Color, accent: Color) -> String {
    let mut s = format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="-12 -12 24 24"><path d="{PLUS}" fill="none" stroke="{}" stroke-width="1.4" stroke-linejoin="round"/>"##,
        hex(ink)
    );
    for b in lit {
        let (x, y, w, h) = match b {
            Btn::DUp => (-4.0, -11.0, 8.0, 7.0),
            Btn::DDown => (-4.0, 4.0, 8.0, 7.0),
            Btn::DLeft => (-11.0, -4.0, 7.0, 8.0),
            _ => (4.0, -4.0, 7.0, 8.0),
        };
        let _ = write!(s, r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="1.2" fill="{}"/>"#, hex(accent));
    }
    s.push_str("</svg>");
    s
}

/// A 24x24 thumbstick glyph.
pub(crate) fn stick_chip(ink: Color, accent: Color) -> String {
    format!(
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="24" height="24" viewBox="-12 -12 24 24"><circle r="10.5" fill="none" stroke="{}" stroke-width="1.4"/><circle r="6" fill="{}" opacity="0.9"/><circle r="3" fill="none" stroke="{}" stroke-width="1.2" opacity="0.6"/></svg>"##,
        hex(ink),
        hex(accent),
        hex(ink)
    )
}

#[derive(Clone, Copy)]
enum Shape {
    Xbox,
    Handheld,
    PlayStation,
    Generic,
}

struct Palette {
    body_top: String,
    body_bottom: String,
    edge: String,
    core_top: String,
    core_bottom: String,
    well: String,
    button: Color,
    cap_top: String,
    cap_bottom: String,
    cap_ring: String,
    symbol: Color,
    shadow: f32,
}

fn palette(shape: Shape, light: bool) -> Palette {
    let s = |v: u32| hex(rgb(v));
    match (shape, light) {
        (Shape::Xbox | Shape::Handheld, false) => Palette {
            body_top: s(0x3B444E),
            body_bottom: s(0x1E252C),
            edge: s(0x56626E),
            core_top: s(0x2E363F),
            core_bottom: s(0x232A31),
            well: s(0x12171C),
            button: rgb(0x161B21),
            cap_top: s(0x414B55),
            cap_bottom: s(0x20262C),
            cap_ring: s(0x59646F),
            symbol: rgb(0xC8D0D8),
            shadow: 0.45,
        },
        (Shape::Xbox | Shape::Handheld, true) => Palette {
            body_top: s(0xFDFDFE),
            body_bottom: s(0xDCE2E7),
            edge: s(0xAFBAC4),
            core_top: s(0xF0F3F5),
            core_bottom: s(0xE3E8EC),
            well: s(0xC4CDD5),
            button: rgb(0x2A3138),
            cap_top: s(0xFAFBFC),
            cap_bottom: s(0xD2D9DF),
            cap_ring: s(0xA9B4BE),
            symbol: rgb(0x55606B),
            shadow: 0.16,
        },
        (Shape::PlayStation, light) => Palette {
            body_top: s(0xFCFDFE),
            body_bottom: s(0xD9DFE6),
            edge: if light { s(0xA7B2BD) } else { s(0x8996A3) },
            core_top: s(0x23282E),
            core_bottom: s(0x111417),
            well: s(0x0B0D10),
            button: rgb(0x2B3139),
            cap_top: s(0x353C44),
            cap_bottom: s(0x181C20),
            cap_ring: s(0x4C555F),
            symbol: rgb(0xC3CAD2),
            shadow: if light { 0.16 } else { 0.45 },
        },
        (Shape::Generic, false) => Palette {
            body_top: s(0x46505A),
            body_bottom: s(0x272E35),
            edge: s(0x5E6A76),
            core_top: s(0x39424B),
            core_bottom: s(0x2D343B),
            well: s(0x161B20),
            button: rgb(0x1D2329),
            cap_top: s(0x4A545E),
            cap_bottom: s(0x262C32),
            cap_ring: s(0x606B76),
            symbol: rgb(0xB9C3CC),
            shadow: 0.45,
        },
        (Shape::Generic, true) => Palette {
            body_top: s(0xF1F4F6),
            body_bottom: s(0xCCD4DB),
            edge: s(0xA4AFBA),
            core_top: s(0xE6EBEF),
            core_bottom: s(0xD9E0E5),
            well: s(0xB9C3CC),
            button: rgb(0x39424B),
            cap_top: s(0x56606A),
            cap_bottom: s(0x323A42),
            cap_ring: s(0x6B7681),
            symbol: rgb(0xDCE2E7),
            shadow: 0.16,
        },
    }
}

const XBOX_BODY: &str = "M300 78 C360 78 400 74 430 72 C480 68 515 85 530 115 C555 165 580 260 585 315 C590 360 560 385 525 378 C495 372 475 345 455 315 C440 292 420 281 395 281 C360 281 332 289 300 289 C268 289 240 281 205 281 C180 281 160 292 145 315 C125 345 105 372 75 378 C40 385 10 360 15 315 C20 260 45 165 70 115 C85 85 120 68 170 72 C200 74 240 78 300 78 Z";
const PS_BODY: &str = "M300 72 C360 72 410 66 446 66 C492 66 520 80 536 110 C562 160 590 268 590 322 C590 362 562 386 528 380 C498 374 480 350 462 324 C446 302 430 292 405 292 L195 292 C170 292 154 302 138 324 C120 350 102 374 72 380 C38 386 10 362 10 322 C10 268 38 160 64 110 C80 80 108 66 154 66 C190 66 240 72 300 72 Z";
const PS_CORE: &str = "M196 96 L404 96 C420 96 426 110 428 128 C434 170 452 198 472 220 C490 242 488 272 464 285 C452 291 440 292 425 292 L175 292 C160 292 148 291 136 285 C112 272 110 242 128 220 C148 198 166 170 172 128 C174 110 180 96 196 96 Z";
const GENERIC_BODY: &str = "M150 92 H450 C520 92 568 140 576 220 C584 300 548 352 494 352 C452 352 430 322 402 302 H198 C170 322 148 352 106 352 C52 352 16 300 24 220 C32 140 80 92 150 92 Z";
const HANDHELD_BODY: &str = "M143 96 H457 C478 81 516 78 544 94 C570 109 580 139 580 178 L584 263 C587 305 572 338 548 346 C524 354 509 336 493 307 L470 277 H130 L107 307 C91 336 76 354 52 346 C28 338 13 305 16 263 L20 178 C20 139 30 109 56 94 C84 78 122 81 143 96 Z";

struct Spots {
    ls: (f32, f32),
    rs: (f32, f32),
    dpad: (f32, f32),
    face: (f32, f32),
    back: (f32, f32),
    start: (f32, f32),
    guide: (f32, f32),
}

fn spots(shape: Shape) -> Spots {
    match shape {
        Shape::Xbox => Spots {
            ls: (176.0, 160.0),
            rs: (368.0, 236.0),
            dpad: (232.0, 236.0),
            face: (425.0, 160.0),
            back: (262.0, 160.0),
            start: (338.0, 160.0),
            guide: (300.0, 112.0),
        },
        Shape::Handheld => Spots {
            ls: (72.0, 155.0),
            rs: (514.0, 244.0),
            dpad: (83.0, 244.0),
            face: (520.0, 153.0),
            back: (122.0, 111.0),
            start: (478.0, 111.0),
            guide: (122.0, 143.0),
        },
        Shape::PlayStation => Spots {
            ls: (232.0, 240.0),
            rs: (368.0, 240.0),
            dpad: (150.0, 168.0),
            face: (450.0, 168.0),
            back: (182.0, 86.0),
            start: (418.0, 86.0),
            guide: (300.0, 236.0),
        },
        Shape::Generic => Spots {
            ls: (168.0, 170.0),
            rs: (368.0, 252.0),
            dpad: (232.0, 252.0),
            face: (432.0, 170.0),
            back: (262.0, 150.0),
            start: (338.0, 150.0),
            guide: (300.0, 196.0),
        },
    }
}

struct Ctx<'a> {
    s: String,
    p: Palette,
    input: &'a Input,
    accent: Color,
    family: Family,
    light: bool,
}

impl Ctx<'_> {
    fn on(&self, b: Btn) -> bool {
        self.input.pressed(b)
    }
    fn halo(&mut self, x: f32, y: f32, r: f32, c: Color) {
        let c = hex(c);
        let _ = write!(
            self.s,
            r#"<circle cx="{x}" cy="{y}" r="{}" fill="{c}" opacity="0.22"/><circle cx="{x}" cy="{y}" r="{}" fill="{c}" opacity="0.30"/>"#,
            r + 9.0,
            r + 4.0
        );
    }
    fn stick(&mut self, (cx, cy): (f32, f32), x: f32, y: f32, pressed: bool) {
        let (px, py) = (cx + x * 11.0, cy - y * 11.0);
        let moved = x.abs() > 0.08 || y.abs() > 0.08;
        let _ = write!(
            self.s,
            r#"<circle cx="{cx}" cy="{cy}" r="35" fill="{}" stroke="{}" stroke-width="1.5"/>"#,
            self.p.well, self.p.edge
        );
        if pressed {
            self.halo(px, py, 25.0, self.accent);
        }
        let _ = write!(
            self.s,
            r#"<circle cx="{px}" cy="{}" r="26" fill="black" opacity="0.28"/><circle cx="{px}" cy="{py}" r="25" fill="url(#cap)" stroke="{}" stroke-width="1.5"/><circle cx="{px}" cy="{py}" r="18" fill="none" stroke="{}" stroke-width="1.2" opacity="0.7"/>"#,
            py + 3.0,
            if pressed { hex(self.accent) } else { self.p.cap_ring.clone() },
            self.p.cap_ring
        );
        if moved || pressed {
            let _ = write!(
                self.s,
                r#"<circle cx="{px}" cy="{py}" r="25" fill="none" stroke="{}" stroke-width="3"/>"#,
                hex(self.accent)
            );
        }
    }
    fn face(&mut self, b: Btn, x: f32, y: f32, r: f32) {
        let pressed = self.on(b);
        let color = face_color(self.family, b);
        let lit = if self.family == Family::Generic { self.accent } else { color };
        if pressed {
            self.halo(x, y, r, lit);
        }
        let fill = if pressed { hex(lit) } else { hex(self.p.button) };
        let _ = write!(
            self.s,
            r#"<circle cx="{x}" cy="{}" r="{r}" fill="black" opacity="0.35"/><circle cx="{x}" cy="{y}" r="{r}" fill="{fill}" stroke="{}" stroke-width="1.2"/><circle cx="{x}" cy="{}" r="{}" fill="white" opacity="{}"/>"#,
            y + 2.5,
            if pressed { hex(lit) } else { "#000000".into() },
            y - r * 0.35,
            r * 0.55,
            if pressed { 0.18 } else { 0.06 }
        );
        if self.family == Family::Generic {
            return;
        }
        let ink = if pressed {
            "#0d1318".to_string()
        } else if self.family.is_playstation() {
            hex(self.p.symbol)
        } else {
            hex(color)
        };
        let _ = write!(
            self.s,
            r#"<g transform="translate({x} {y}) scale({})" fill="none" stroke="{ink}" stroke-width="2.3" stroke-linecap="round" stroke-linejoin="round">{}</g>"#,
            r / 15.0,
            face_glyph(self.family, b)
        );
    }
    fn faces(&mut self, (cx, cy): (f32, f32), gap: f32, r: f32) {
        self.face(Btn::North, cx, cy - gap, r);
        self.face(Btn::West, cx - gap, cy, r);
        self.face(Btn::East, cx + gap, cy, r);
        self.face(Btn::South, cx, cy + gap, r);
    }
    fn arm_fill(&self, b: Btn) -> String {
        if self.on(b) {
            hex(self.accent)
        } else {
            hex(self.p.button)
        }
    }
    /// One-piece cross D-pad (Xbox and generic).
    fn cross_dpad(&mut self, (cx, cy): (f32, f32)) {
        let _ = write!(
            self.s,
            r##"<circle cx="{cx}" cy="{cy}" r="31" fill="{}" stroke="{}" stroke-width="1.5"/><g transform="translate({cx} {cy})"><path d="M-8.5 -25 H8.5 V-8.5 H25 V8.5 H8.5 V25 H-8.5 V8.5 H-25 V-8.5 H-8.5 Z" transform="translate(0 2.5)" fill="black" opacity="0.35"/><path d="M-8.5 -25 H8.5 V-8.5 H25 V8.5 H8.5 V25 H-8.5 V8.5 H-25 V-8.5 H-8.5 Z" fill="{}" stroke="#000000" stroke-width="1.2" stroke-linejoin="round"/>"##,
            self.p.well,
            self.p.edge,
            hex(self.p.button),
        );
        for (b, x, y, w, h) in [
            (Btn::DUp, -8.5, -25.0, 17.0, 16.0),
            (Btn::DDown, -8.5, 9.0, 17.0, 16.0),
            (Btn::DLeft, -25.0, -8.5, 16.0, 17.0),
            (Btn::DRight, 9.0, -8.5, 16.0, 17.0),
        ] {
            if self.on(b) {
                let _ = write!(self.s, r#"<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="2" fill="{}"/>"#, hex(self.accent));
            }
        }
        let _ = write!(
            self.s,
            r#"<circle r="4.5" fill="none" stroke="{}" stroke-width="1" opacity="0.5"/></g>"#,
            self.p.cap_ring
        );
    }
    /// Four separate arrow buttons pointing inward (PlayStation).
    fn arrow_dpad(&mut self, (cx, cy): (f32, f32)) {
        let _ = write!(self.s, r#"<g transform="translate({cx} {cy})">"#);
        for (b, angle) in [(Btn::DUp, 0), (Btn::DRight, 90), (Btn::DDown, 180), (Btn::DLeft, 270)] {
            if self.on(b) {
                let _ = write!(
                    self.s,
                    r#"<g transform="rotate({angle})"><path d="M0 -45 m-19 0 h38 v24 l-19 15 l-19 -15 Z" fill="{}" opacity="0.28"/></g>"#,
                    hex(self.accent)
                );
            }
            let _ = write!(
                self.s,
                r#"<g transform="rotate({angle})"><path d="M-12 -41 H12 Q15 -41 15 -38 V-22 L0 -10 L-15 -22 V-38 Q-15 -41 -12 -41 Z" transform="translate(0 2.5)" fill="black" opacity="0.4"/><path d="M-12 -41 H12 Q15 -41 15 -38 V-22 L0 -10 L-15 -22 V-38 Q-15 -41 -12 -41 Z" fill="{}" stroke="black" stroke-width="1"/><path d="M-4.5 -27 L0 -32 L4.5 -27" fill="none" stroke="{}" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" opacity="0.55"/></g>"#,
                self.arm_fill(b),
                hex(self.p.symbol)
            );
        }
        self.s.push_str("</g>");
    }
    fn pill(&mut self, b: Btn, (x, y): (f32, f32), w: f32, h: f32, glyph: &str) {
        let fill = self.arm_fill(b);
        if self.on(b) {
            self.halo(x, y, w.max(h) / 2.0, self.accent);
        }
        let _ = write!(
            self.s,
            r#"<rect x="{}" y="{}" width="{w}" height="{h}" rx="{}" fill="{fill}" stroke="black" stroke-width="1"/>"#,
            x - w / 2.0,
            y - h / 2.0,
            w.min(h) / 2.0
        );
        if !glyph.is_empty() {
            let _ = write!(
                self.s,
                r#"<g transform="translate({x} {y})" fill="none" stroke="{}" stroke-width="1.3" stroke-linecap="round" stroke-linejoin="round">{glyph}</g>"#,
                if self.on(b) { "#0d1318".to_string() } else { hex(self.p.symbol) }
            );
        }
    }
    /// A shoulder bumper (left side; `mirror` draws the right one).
    fn bumper(&mut self, b: Btn, d: &str, mirror: bool) {
        let t = if mirror { r#" transform="translate(600 0) scale(-1 1)""# } else { "" };
        let fill = if self.on(b) { hex(self.accent) } else { self.p.core_bottom.clone() };
        let _ = write!(
            self.s,
            r#"<path d="{d}"{t} fill="{fill}" stroke="{}" stroke-width="1.3" stroke-linejoin="round"/>"#,
            self.p.edge
        );
    }
    /// A trigger peeking above the body, tinted by how far it is pulled.
    fn trigger(&mut self, value: f32, d: &str, mirror: bool) {
        let t = if mirror { r#" transform="translate(600 0) scale(-1 1)""# } else { "" };
        let base = if self.light && !self.family.is_playstation() {
            rgb(0xC3CCD4)
        } else {
            rgb(0x262C33)
        };
        let fill = mix(base, self.accent, value);
        if value > 0.02 {
            let _ = write!(
                self.s,
                r#"<path d="{d}"{t} fill="{}" opacity="{}" stroke="{}" stroke-width="10" stroke-linejoin="round"/>"#,
                hex(self.accent),
                0.25 * value,
                hex(self.accent)
            );
        }
        let _ = write!(
            self.s,
            r#"<path d="{d}"{t} fill="{}" stroke="{}" stroke-width="1.3" stroke-linejoin="round"/>"#,
            hex(fill),
            self.p.edge
        );
    }
}

fn shape_of(family: Family) -> Shape {
    match family {
        Family::Xbox => Shape::Xbox,
        Family::RogXboxAlly | Family::RogXboxAllyX => Shape::Handheld,
        f if f.is_playstation() => Shape::PlayStation,
        _ => Shape::Generic,
    }
}

/// The full controller illustration (600x400) with `input` lit up.
pub(crate) fn controller(family: Family, input: &Input, light: bool, accent: Color) -> String {
    let shape = shape_of(family);
    // The Ally's white shell and Ally X's black shell identify the hardware;
    // the surrounding UI theme does not change the device's case colour.
    let mut p = palette(shape, if family.is_handheld() { family == Family::RogXboxAlly } else { light });
    if family.is_handheld() {
        p.well = hex(rgb(0x14191F));
        p.cap_top = hex(rgb(0x424B54));
        p.cap_bottom = hex(rgb(0x20262C));
        p.cap_ring = hex(rgb(0x606B75));
        p.button = rgb(0x242B32);
        p.symbol = rgb(0xD6DEE5);
        p.shadow = if light { 0.16 } else { 0.45 };
    }
    let mut c = Ctx {
        s: String::with_capacity(16 * 1024),
        p,
        input,
        accent,
        family,
        light,
    };
    let _ = write!(
        c.s,
        r##"<svg xmlns="http://www.w3.org/2000/svg" width="600" height="400" viewBox="0 0 600 400"><defs><linearGradient id="body" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient><linearGradient id="core" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></linearGradient><radialGradient id="cap" cx="0.42" cy="0.35" r="0.75"><stop offset="0" stop-color="{}"/><stop offset="1" stop-color="{}"/></radialGradient><linearGradient id="pad" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#f6f8fa"/><stop offset="1" stop-color="#dfe4e9"/></linearGradient></defs><ellipse cx="300" cy="388" rx="250" ry="9" fill="black" opacity="{}"/>"##,
        c.p.body_top, c.p.body_bottom, c.p.core_top, c.p.core_bottom, c.p.cap_top, c.p.cap_bottom, c.p.shadow
    );
    let sp = spots(shape);
    match shape {
        Shape::Handheld => {
            let trigger = "M24 120 C23 98 32 83 53 77 L110 74 C127 74 137 83 137 96 L133 106 L34 131 Z";
            c.trigger(input.lt, trigger, false);
            c.trigger(input.rt, trigger, true);
            let _ = write!(
                c.s,
                r##"<g data-part="handheld"><title>{}</title><path d="{HANDHELD_BODY}" fill="url(#body)" stroke="{}" stroke-width="1.6"/><path d="M31 180 L27 262 C25 294 33 317 49 323 M569 180 L573 262 C575 294 567 317 551 323" fill="none" stroke="{}" stroke-width="1.4" opacity="0.65"/><rect x="140" y="96" width="320" height="182" rx="12" fill="{}" stroke="{}" stroke-width="1.5"/><rect data-part="handheld-screen" x="151" y="106" width="298" height="162" rx="4" fill="#0e171e"/>"##,
                family.label(), c.p.edge, c.p.edge, c.p.well, c.p.edge
            );
            // Original geometric screen artwork, with no game or vendor assets.
            let _ = write!(
                c.s,
                r#"<g fill="{}"><path d="M151 246 L209 195 L260 235 L328 169 L397 228 L449 195 V268 H151 Z" opacity="0.08"/><path d="M300 143 L327 175 L300 221 L273 175 Z" opacity="0.22"/><path d="M300 143 V221 M273 175 H327 M300 143 L288 175 L300 221 L312 175 Z" fill="none" stroke="{}" stroke-width="1" opacity="0.6"/></g><path d="M160 259 H440" stroke="{}" stroke-width="1" opacity="0.25"/>"#,
                hex(accent), hex(accent), hex(accent)
            );
            let bumper = "M24 115 C29 99 42 91 59 89 L124 87 C132 87 136 92 133 98 L60 101 C46 102 38 108 34 121 Z";
            c.bumper(Btn::LB, bumper, false);
            c.bumper(Btn::RB, bumper, true);
            c.pill(Btn::Back, sp.back, 16.0, 16.0, r#"<rect x="-3.6" y="-2.6" width="5" height="4" rx="0.6"/><rect x="-1.4" y="-0.6" width="5" height="4" rx="0.6"/>"#);
            c.pill(Btn::Start, sp.start, 16.0, 16.0, r#"<path d="M-3.4 -2.4 H3.4 M-3.4 0 H3.4 M-3.4 2.4 H3.4"/>"#);
            c.pill(Btn::Guide, sp.guide, 17.0, 17.0, r#"<circle r="5.6"/><path d="M-3.8 -3.8 L3.8 3.8 M3.8 -3.8 L-3.8 3.8"/>"#);
            // Command Center and Library are device shortcuts, not generic
            // gamepad buttons. Show their positions without claiming input.
            let _ = write!(c.s, r#"<g fill="{}" stroke="{}" stroke-width="1"><rect x="114" y="168" width="16" height="14" rx="5"/><rect x="470" y="168" width="16" height="14" rx="5"/></g>"#, hex(c.p.button), c.p.edge);
            let _ = write!(c.s, r#"<g stroke="{}" stroke-width="1" fill="none" opacity="0.7"><path d="M119 172 H125 M119 175 H125 M119 178 H125 M475 172 H481 V178 H475 Z"/></g>"#, hex(c.p.symbol));
            for (x, y) in [(72, 155), (514, 244)] {
                let _ = write!(c.s, r#"<circle cx="{x}" cy="{y}" r="36.5" fill="none" stroke="{}" stroke-width="2.5" opacity="0.65"/>"#, hex(accent));
            }
            c.stick(sp.ls, input.lx, input.ly, c.on(Btn::LS));
            c.cross_dpad(sp.dpad);
            c.stick(sp.rs, input.rx, input.ry, c.on(Btn::RS));
            c.faces(sp.face, 24.0, 12.5);
            // Speaker perforations are part of the front shell.
            for x in [111, 472] {
                for row in 0..3 {
                    for column in 0..4 {
                        let _ = write!(c.s, r#"<circle cx="{}" cy="{}" r="1.2" fill="{}" opacity="0.6"/>"#, x + column * 4, 259 + row * 4, c.p.edge);
                    }
                }
            }
            c.s.push_str("</g>");
        }
        Shape::Xbox => {
            c.trigger(input.lt, "M98 96 C100 64 122 42 154 40 C182 38 200 50 204 72 L206 80 L98 104 Z", false);
            c.trigger(input.rt, "M98 96 C100 64 122 42 154 40 C182 38 200 50 204 72 L206 80 L98 104 Z", true);
            let _ = write!(
                c.s,
                r#"<path d="{XBOX_BODY}" fill="url(#body)" stroke="{}" stroke-width="1.6"/><path d="M300 92 C350 92 395 88 428 86 C470 83 502 98 515 122" fill="none" stroke="white" stroke-width="1.2" opacity="0.10"/><path d="M300 92 C250 92 205 88 172 86 C130 83 98 98 85 122" fill="none" stroke="white" stroke-width="1.2" opacity="0.10"/>"#,
                c.p.edge
            );
            let lb = "M72 112 C86 82 122 64 172 66 L212 68 C217 72 217 79 211 81 L172 80 C130 79 101 92 85 116 C80 121 72 119 72 112 Z";
            c.bumper(Btn::LB, lb, false);
            c.bumper(Btn::RB, lb, true);
            // The guide button and its ring.
            let gp = c.on(Btn::Guide);
            if gp {
                c.halo(sp.guide.0, sp.guide.1, 17.0, accent);
            }
            let _ = write!(
                c.s,
                r#"<circle cx="300" cy="112" r="18" fill="{}" stroke="{}" stroke-width="1.4"/><circle cx="300" cy="112" r="11" fill="none" stroke="{}" stroke-width="2" opacity="0.85"/><path d="M294 106 L306 118 M306 106 L294 118" stroke="{}" stroke-width="2" stroke-linecap="round" opacity="0.85"/>"#,
                if gp { hex(accent) } else { hex(c.p.button) },
                c.p.edge,
                hex(c.p.symbol),
                hex(c.p.symbol)
            );
            c.pill(Btn::Back, sp.back, 15.0, 15.0, r#"<rect x="-3.6" y="-2.6" width="5" height="4" rx="0.6"/><rect x="-1.4" y="-0.6" width="5" height="4" rx="0.6"/>"#);
            c.pill(Btn::Start, sp.start, 15.0, 15.0, r#"<path d="M-3.4 -2.4 H3.4 M-3.4 0 H3.4 M-3.4 2.4 H3.4"/>"#);
            c.pill(Btn::Mute, (300.0, 184.0), 14.0, 7.0, "");
            c.stick(sp.ls, input.lx, input.ly, c.on(Btn::LS));
            c.cross_dpad(sp.dpad);
            c.stick(sp.rs, input.rx, input.ry, c.on(Btn::RS));
            c.faces(sp.face, 34.0, 16.0);
        }
        Shape::PlayStation => {
            c.trigger(input.lt, "M84 100 C86 66 110 42 146 40 C176 38 196 52 200 74 L202 84 L84 108 Z", false);
            c.trigger(input.rt, "M84 100 C86 66 110 42 146 40 C176 38 196 52 200 74 L202 84 L84 108 Z", true);
            let _ = write!(
                c.s,
                r#"<path d="{PS_BODY}" fill="url(#body)" stroke="{}" stroke-width="1.6"/><path d="{PS_CORE}" fill="url(#core)" stroke="black" stroke-opacity="0.5" stroke-width="1"/>"#,
                c.p.edge
            );
            let l1 = "M64 112 C78 84 104 70 146 68 L200 70 C205 74 205 80 199 82 L148 81 C112 82 92 94 78 118 C73 123 63 120 64 112 Z";
            c.bumper(Btn::LB, l1, false);
            c.bumper(Btn::RB, l1, true);
            // Touchpad with the light bar along its sides.
            let tp = c.on(Btn::Touchpad);
            let _ = write!(
                c.s,
                r##"<path d="M207 82 L214 170" stroke="#4ea7ff" stroke-width="8" opacity="0.22" stroke-linecap="round"/><path d="M393 82 L386 170" stroke="#4ea7ff" stroke-width="8" opacity="0.22" stroke-linecap="round"/><path d="M216 72 L384 72 Q396 72 395 84 L387 170 Q386 180 375 180 L225 180 Q214 180 213 170 L205 84 Q204 72 216 72 Z" fill="{}" stroke="{}" stroke-width="1.3"/><path d="M207.5 84 L214.5 168" stroke="#5db0ff" stroke-width="2.6" stroke-linecap="round"/><path d="M392.5 84 L385.5 168" stroke="#5db0ff" stroke-width="2.6" stroke-linecap="round"/>"##,
                if tp { hex(mix(Color::WHITE, accent, 0.55)) } else { "url(#pad)".into() },
                c.p.edge
            );
            c.pill(Btn::Back, sp.back, 8.0, 19.0, "");
            c.pill(Btn::Start, sp.start, 8.0, 19.0, "");
            c.arrow_dpad(sp.dpad);
            c.faces(sp.face, 37.0, 17.0);
            c.stick(sp.ls, input.lx, input.ly, c.on(Btn::LS));
            c.stick(sp.rs, input.rx, input.ry, c.on(Btn::RS));
            let gp = c.on(Btn::Guide);
            if gp {
                c.halo(sp.guide.0, sp.guide.1, 12.0, accent);
            }
            let _ = write!(
                c.s,
                r#"<circle cx="300" cy="236" r="12" fill="{}" stroke="black" stroke-width="1"/><circle cx="300" cy="236" r="7" fill="none" stroke="{}" stroke-width="1.4" opacity="0.6"/>"#,
                if gp { hex(accent) } else { hex(c.p.button) },
                hex(c.p.symbol)
            );
            if matches!(family, Family::DualSense | Family::DualSenseEdge) {
                c.pill(Btn::Mute, (300.0, 266.0), 20.0, 7.0, "");
            }
        }
        Shape::Generic => {
            c.trigger(input.lt, "M110 100 C112 70 132 52 160 50 C186 48 202 60 206 84 L206 92 L110 108 Z", false);
            c.trigger(input.rt, "M110 100 C112 70 132 52 160 50 C186 48 202 60 206 84 L206 92 L110 108 Z", true);
            let _ = write!(
                c.s,
                r#"<path d="{GENERIC_BODY}" fill="url(#body)" stroke="{}" stroke-width="1.6"/>"#,
                c.p.edge
            );
            let lb = "M86 112 C100 96 122 90 150 90 L208 90 C213 93 213 100 208 102 L150 102 C126 102 108 108 96 120 C90 125 82 120 86 112 Z";
            c.bumper(Btn::LB, lb, false);
            c.bumper(Btn::RB, lb, true);
            c.pill(Btn::Back, sp.back, 20.0, 9.0, "");
            c.pill(Btn::Start, sp.start, 20.0, 9.0, "");
            c.pill(Btn::Guide, sp.guide, 16.0, 16.0, "");
            c.stick(sp.ls, input.lx, input.ly, c.on(Btn::LS));
            c.cross_dpad(sp.dpad);
            c.stick(sp.rs, input.rx, input.ry, c.on(Btn::RS));
            c.faces(sp.face, 33.0, 15.5);
        }
    }
    c.s.push_str("</svg>");
    c.s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gamepad_art_parses_for_every_family() {
        let mut input = Input::default();
        for b in Btn::ALL {
            input.buttons |= b.bit();
        }
        input.lt = 0.5;
        input.lx = 1.0;
        for family in [Family::Xbox, Family::RogXboxAlly, Family::RogXboxAllyX, Family::DualSense, Family::DualSenseEdge, Family::DualShock4, Family::SwitchPro, Family::Generic] {
            for light in [false, true] {
                for inp in [Input::default(), input] {
                    let svg = controller(family, &inp, light, Color::from_rgb8(157, 213, 234));
                    assert!(svg.starts_with("<svg") && svg.ends_with("</svg>"));
                    assert_eq!(svg.matches("<svg").count(), 1);
                    assert!(!svg.contains("NaN"));
                    let mut reader = quick_xml::Reader::from_str(&svg);
                    loop {
                        if reader.read_event().expect("valid controller SVG") == quick_xml::events::Event::Eof {
                            break;
                        }
                    }
                }
            }
            for b in [Btn::South, Btn::East, Btn::West, Btn::North] {
                assert!(face_chip(family, b, false, false).ends_with("</svg>"));
            }
        }
    }

    #[test]
    fn gamepad_face_letters_follow_each_family() {
        // Xbox: A bottom, B right; Nintendo: B bottom, A right.
        assert!(face_glyph(Family::Xbox, Btn::South).contains("H3.1"));
        assert!(face_glyph(Family::SwitchPro, Btn::East).contains("H3.1"));
        assert!(face_glyph(Family::SwitchPro, Btn::South).contains("A2.9"));
        assert!(face_glyph(Family::Xbox, Btn::East).contains("A2.9"));
        assert!(face_glyph(Family::Xbox, Btn::North).contains("L0 0.2"));
        assert!(face_glyph(Family::SwitchPro, Btn::West).contains("L0 0.2"));
        assert!(face_glyph(Family::DualSense, Btn::South).contains("M-5.5"));
        for family in [Family::RogXboxAlly, Family::RogXboxAllyX] {
            for b in [Btn::South, Btn::East, Btn::West, Btn::North] {
                assert_eq!(face_glyph(family, b), face_glyph(Family::Xbox, b));
                assert_eq!(face_color(family, b), face_color(Family::Xbox, b));
            }
        }
    }

    #[test]
    fn gamepad_handheld_illustration_has_a_screen_and_distinct_case_colours() {
        let render = |family| controller(family, &Input::default(), false, Color::from_rgb8(157, 213, 234));
        let ally = render(Family::RogXboxAlly);
        let ally_x = render(Family::RogXboxAllyX);
        for svg in [&ally, &ally_x] {
            assert!(svg.contains("data-part=\"handheld-screen\""));
            assert!(svg.contains(HANDHELD_BODY));
        }
        assert_ne!(ally, ally_x);
        assert!(!render(Family::Xbox).contains("data-part=\"handheld-screen\""));
    }
}
