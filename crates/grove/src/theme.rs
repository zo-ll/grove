//! Colour, corners and density — the only place in the TUI that knows what a
//! hex value is.
//!
//! Three rules, and the module exists to keep all three in one place:
//!
//! - **Call sites name a role, never a colour.** `theme.style(Role::Dirty)`,
//!   not `Color::Yellow`. What "dirty" looks like is a theme decision, and a
//!   hex literal in a screen is that decision escaping into a place nobody
//!   will think to re-read when the palette changes.
//! - **Truecolor when the terminal has it, the nearest ANSI colour when it
//!   does not.** SPEC §9 requires the degradation; a 24-bit escape sent to a
//!   16-colour terminal is not ignored, it is rendered as something arbitrary.
//! - **A bad value is never fatal.** SPEC §9: a broken config must not brick
//!   grove. An unparseable colour falls back to that role's default and is
//!   reported, which is a status bar line, not an exit.

use grove_lua::{Corners, Density, Theme as ThemeConfig, TuiConfig};
use ratatui::style::{Color, Style};
use ratatui::widgets::BorderType;

/// What a colour is *for*. Screens ask for these; they never ask for a colour.
///
/// An enum rather than five accessors so that adding a role is a compile error
/// everywhere it must be handled, rather than a silent black.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// Focus rings, selected rows, the active session — where the eye goes.
    Accent,
    /// A worktree with nothing uncommitted.
    Clean,
    /// A worktree with local changes, and anything else that would be lost.
    Dirty,
    /// Failures and destructive confirmations.
    Error,
    /// Secondary text: hints, inactive panes, anything not being acted on.
    Muted,
}

impl Role {
    /// Every role, for iteration in tests and for resolving a whole palette.
    ///
    /// Written as a `match` first so the compiler forces this list to grow with
    /// the enum: a new variant fails E0004 here rather than going unstyled.
    pub const ALL: [Self; 5] = [
        Self::Accent,
        Self::Clean,
        Self::Dirty,
        Self::Error,
        Self::Muted,
    ];

    /// The name used when reporting a bad value, so the message names the
    /// setting the user wrote rather than an index.
    pub fn name(self) -> &'static str {
        match self {
            Self::Accent => "accent",
            Self::Clean => "clean",
            Self::Dirty => "dirty",
            Self::Error => "error",
            Self::Muted => "muted",
        }
    }

    /// This role's configured value.
    ///
    /// A `match` rather than a lookup table: the exhaustiveness check is what
    /// guarantees a new role cannot be added without deciding where it reads
    /// from.
    fn configured(self, theme: &ThemeConfig) -> &str {
        match self {
            Self::Accent => &theme.accent,
            Self::Clean => &theme.clean,
            Self::Dirty => &theme.dirty,
            Self::Error => &theme.error,
            Self::Muted => &theme.muted,
        }
    }
}

/// How many colours the terminal can actually show.
///
/// Detected from the environment rather than probed: a probe means writing an
/// escape and waiting for a reply, which is a visible flicker and a hang on
/// terminals that never answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Depth {
    /// 24-bit colour — the configured value is used exactly.
    True,
    /// The 256-colour cube.
    Ansi256,
    /// The 16 base colours, and the floor everything degrades to.
    Ansi16,
}

impl Depth {
    /// Read the environment.
    pub fn detect() -> Self {
        Self::from_env(
            std::env::var("COLORTERM").ok().as_deref(),
            std::env::var("TERM").ok().as_deref(),
        )
    }

    /// The detection itself, as a function of its inputs so it can be tested
    /// without setting process-wide environment variables — which race, since
    /// tests share a process.
    fn from_env(colorterm: Option<&str>, term: Option<&str>) -> Self {
        // COLORTERM is the only variable that actually promises 24-bit; TERM
        // rarely says so even where it is supported.
        if let Some(ct) = colorterm
            && (ct.contains("truecolor") || ct.contains("24bit"))
        {
            return Self::True;
        }
        match term {
            Some(t) if t.contains("256color") => Self::Ansi256,
            // `dumb` and an absent TERM both mean "assume nothing".
            _ => Self::Ansi16,
        }
    }
}

/// A colour that failed to parse, kept so the UI can say so once.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BadColor {
    /// The role whose value could not be read.
    pub role: Role,
    /// What the config actually said, quoted back so the typo is visible.
    pub value: String,
}

impl std::fmt::Display for BadColor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "theme.{} is not a colour: {:?}",
            self.role.name(),
            self.value
        )
    }
}

/// The resolved theme: colours in the terminal's own depth, plus the two
/// non-colour settings that also belong to the look.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Theme {
    accent: Color,
    clean: Color,
    dirty: Color,
    error: Color,
    muted: Color,
    /// Kept so the fixed inks resolve the same way the roles did.
    depth: Depth,
    corners: Corners,
    density: Density,
}

impl Theme {
    /// Resolve a config against a terminal depth.
    ///
    /// Returns the theme *and* whatever could not be read: the theme is always
    /// usable, because a colour grove cannot parse falls back to that role's
    /// default rather than disabling the screen it was for.
    pub fn resolve(config: &TuiConfig, depth: Depth) -> (Self, Vec<BadColor>) {
        let mut bad = Vec::new();
        let rgb = Role::ALL.map(|role| {
            let written = role.configured(&config.theme);
            match parse_hex(written) {
                Some(rgb) => rgb,
                None => {
                    bad.push(BadColor {
                        role,
                        value: written.to_owned(),
                    });
                    // `grove-lua`'s defaults are compile-time constants, so the
                    // fallback cannot fail in turn.
                    parse_hex(role.configured(&ThemeConfig::default()))
                        .expect("grove-lua's defaults are valid hex")
                }
            }
        });

        // At 16 colours the roles are resolved together rather than one at a
        // time, because keeping them distinguishable is a property of the set.
        // Deeper palettes have room for every colour to be itself.
        let colours = match depth {
            Depth::Ansi16 => palette_16(rgb).map(Color::Indexed),
            Depth::Ansi256 => rgb.map(|c| Color::Indexed(nearest_256(c))),
            Depth::True => rgb.map(|(r, g, b)| Color::Rgb(r, g, b)),
        };

        let theme = Self {
            depth,
            accent: colours[0],
            clean: colours[1],
            dirty: colours[2],
            error: colours[3],
            muted: colours[4],
            corners: config.corners,
            density: config.density,
        };
        (theme, bad)
    }

    /// The colour for a role.
    pub fn color(&self, role: Role) -> Color {
        match role {
            Role::Accent => self.accent,
            Role::Clean => self.clean,
            Role::Dirty => self.dirty,
            Role::Error => self.error,
            Role::Muted => self.muted,
        }
    }

    /// The style for a role — what call sites actually want.
    pub fn style(&self, role: Role) -> Style {
        Style::default().fg(self.color(role))
    }

    /// The colour of one of the mock's fixed inks.
    ///
    /// Resolved through the same depth logic as the roles, so a 16-colour
    /// terminal degrades the whole palette together rather than leaving the
    /// frames in truecolour beside downgraded text.
    pub fn ink(&self, ink: Ink) -> Color {
        let rgb = ink.rgb();
        match self.depth {
            Depth::Ansi16 => Color::Indexed(nearest_16(rgb)),
            Depth::Ansi256 => Color::Indexed(nearest_256(rgb)),
            Depth::True => Color::Rgb(rgb.0, rgb.1, rgb.2),
        }
    }

    /// The style for a fixed ink — what call sites actually want.
    pub fn ink_style(&self, ink: Ink) -> Style {
        Style::default().fg(self.ink(ink))
    }

    /// The colour of a pane's border when it does not have focus.
    ///
    /// Dimmer than [`Role::Muted`], which is text: a frame that reads as
    /// loudly as the words inside it competes with them, and in the mock the
    /// unfocused panes recede almost to the background.
    pub fn frame_style(&self) -> Style {
        self.ink_style(Ink::Frame)
    }

    /// The border treatment for panes and overlays.
    pub fn border(&self) -> BorderType {
        match self.corners {
            Corners::Rounded => BorderType::Rounded,
            Corners::Square => BorderType::Plain,
        }
    }

    /// Rows a single list entry occupies.
    ///
    /// Airy spends a blank line per row; compact does not. This is the whole of
    /// density as far as a list is concerned, and it is a number rather than a
    /// bool so the screens do arithmetic instead of branching.
    #[allow(dead_code, reason = "the lists that consume it land in #18-#30")]
    pub fn row_height(&self) -> u16 {
        match self.density {
            Density::Airy => 2,
            Density::Compact => 1,
        }
    }

    /// Blank columns between a pane's border and its content.
    pub fn padding(&self) -> u16 {
        match self.density {
            Density::Airy => 1,
            Density::Compact => 0,
        }
    }
}

/// The mock's fixed colours, as opposed to the five a config may set.
///
/// These are structure rather than meaning: a frame, a rule, the weight of a
/// line of text against the line above it. [`Role`] is about what a thing *is*
/// — clean, dirty, wrong — and none of these are any of those, which is why
/// they are not configurable and not in that enum. Every value is Catppuccin
/// Mocha's, because the mock is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(
    dead_code,
    reason = "Text, Faint and Other are the overlays' — the palette, picker, \
              diff and end-session screens land next, and splitting one \
              palette across those commits would mean choosing its colours \
              four times"
)]
pub enum Ink {
    /// `crust`. What an overlay washes the screen behind it with.
    Backdrop,
    /// `surface0`. An unfocused pane's border, and the status bar's ground.
    Frame,
    /// `surface1`. A rule between two things on one line.
    Divider,
    /// `text`. A row the cursor is on, or a name being acted on.
    Text,
    /// `subtext0`. Ordinary rows, and the status bar's labels.
    Subtext,
    /// `overlay0`. Present but not the point: a row's owner, a hint's number.
    Faint,
    /// `blue`. Another session's claim on a worktree — the one ownership
    /// state that is neither yours nor free, and so has a colour of its own.
    Other,
}

impl Ink {
    const fn rgb(self) -> (u8, u8, u8) {
        match self {
            Self::Backdrop => (0x11, 0x11, 0x1b),
            Self::Frame => (0x31, 0x32, 0x44),
            Self::Divider => (0x45, 0x47, 0x5a),
            Self::Text => (0xcd, 0xd6, 0xf4),
            Self::Subtext => (0xa6, 0xad, 0xc8),
            Self::Faint => (0x6c, 0x70, 0x86),
            Self::Other => (0x89, 0xb4, 0xfa),
        }
    }
}

/// `#rrggbb` or `rrggbb`, case-insensitive.
///
/// Deliberately narrow: named colours and `#rgb` shorthand are not accepted,
/// because silently accepting half a format invites a config that renders
/// differently from the one the user thinks they wrote.
fn parse_hex(value: &str) -> Option<(u8, u8, u8)> {
    let digits = value.strip_prefix('#').unwrap_or(value);
    if digits.len() != 6 || !digits.bytes().all(|b| b.is_ascii_hexdigit()) {
        return None;
    }
    let byte = |at: usize| u8::from_str_radix(&digits[at..at + 2], 16).ok();
    Some((byte(0)?, byte(2)?, byte(4)?))
}

/// Distance between two colours, weighted so the result matches what the eye
/// would call "nearest".
///
/// Plain Euclidean distance in RGB gets this wrong in exactly the case that
/// matters here: it will happily call a saturated green the nearest match for a
/// mid grey, because the channels are treated as interchangeable. This is the
/// "redmean" approximation, which weights by where in the red axis the pair
/// sits and is the standard cheap fix.
fn distance(a: (u8, u8, u8), b: (u8, u8, u8)) -> i64 {
    let rmean = (i64::from(a.0) + i64::from(b.0)) / 2;
    let dr = i64::from(a.0) - i64::from(b.0);
    let dg = i64::from(a.1) - i64::from(b.1);
    let db = i64::from(a.2) - i64::from(b.2);
    (((512 + rmean) * dr * dr) >> 8) + 4 * dg * dg + (((767 - rmean) * db * db) >> 8)
}

/// The ANSI index a human would name for this colour.
///
/// **Not** the nearest by distance, and the difference is the whole point. The
/// default palette is Catppuccin Mocha, whose colours are pastels: `#a6e3a1`
/// is a green, but it is a *light* green, and the nearest of the 16 by any
/// distance metric is plain white — as is the nearest for the pink and the
/// peach. Distance is the right question for "which of these 16 looks most
/// like that" and the wrong one for "which of these 16 means what that meant".
///
/// So: hue decides the colour, lightness decides whether it is the bright
/// variant, and only a colour with too little chroma to *have* a hue falls
/// back to the greys.
fn nearest_16(rgb: (u8, u8, u8)) -> u8 {
    let (r, g, b) = (i32::from(rgb.0), i32::from(rgb.1), i32::from(rgb.2));
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let chroma = max - min;
    let lightness = (max + min) / 2;
    // Bright variants sit 8 apart from their base in the ANSI table.
    let bright = if lightness >= 128 { 8 } else { 0 };

    // Below this there is no hue worth preserving — the value is a grey, and
    // calling it "slightly blue" would send `muted` to blue.
    if chroma < GREY_CHROMA {
        return match lightness {
            // Four achromatic slots, so the choice is which grey rather than
            // whether to have one.
            l if l < 64 => 0,
            l if l < 160 => 8,
            l if l < 224 => 7,
            _ => 15,
        };
    }

    // Standard hue derivation, in degrees, scaled by 60 per sector.
    let hue = if max == r {
        (((g - b) * 60) / chroma + 360) % 360
    } else if max == g {
        ((b - r) * 60) / chroma + 120
    } else {
        ((r - g) * 60) / chroma + 240
    };

    let base = match hue {
        0..=29 | 330..=359 => 1, // red
        30..=89 => 3,            // yellow
        90..=149 => 2,           // green
        150..=209 => 6,          // cyan
        210..=269 => 4,          // blue
        _ => 5,                  // magenta
    };
    base + bright
}

/// Chroma below which a colour is treated as grey rather than as a very
/// desaturated hue.
const GREY_CHROMA: i32 = 32;

/// The 16-colour palette, with every role kept distinguishable.
///
/// Hue mapping alone is not enough: `accent` is a peach and `error` is a pink,
/// and at 16 colours both are honestly "red". Rendering them identically would
/// satisfy every property except the one that matters — a user glancing at the
/// screen has to be able to tell a highlight from a failure.
///
/// So roles claim their colour in order of how much the colour is carrying.
/// The three status colours go first, because "clean", "dirty" and "error" are
/// read as meaning; `accent` and `muted` take what is left. A displaced role
/// tries its other brightness before it tries another hue, so it stays
/// recognisably the colour it was.
fn palette_16(colours: [(u8, u8, u8); 5]) -> [u8; 5] {
    // Index into `Role::ALL`, in claim order rather than declaration order.
    const CLAIM_ORDER: [usize; 5] = [3, 1, 2, 4, 0];

    let mut assigned = [None; 5];
    let mut taken = [false; 16];
    for role in CLAIM_ORDER {
        let wanted = nearest_16(colours[role]);
        let index = alternatives(wanted)
            .into_iter()
            .find(|candidate| !taken[usize::from(*candidate)])
            // Cannot happen with five roles and sixteen colours, but a panic
            // in a render path is not worth the certainty.
            .unwrap_or(wanted);
        taken[usize::from(index)] = true;
        assigned[role] = Some(index);
    }
    assigned.map(|index| index.expect("every role was assigned"))
}

/// What a displaced colour tries next, nearest in meaning first: the other
/// brightness of the same hue, then the neighbouring hues, then anything.
fn alternatives(wanted: u8) -> Vec<u8> {
    let mut out = vec![wanted, wanted ^ 8];
    let base = wanted % 8;
    let bright = wanted & 8;
    if (1..=6).contains(&base) {
        for step in [1, 6] {
            let neighbour = ((base - 1 + step) % 6) + 1;
            out.push(neighbour + bright);
            out.push(neighbour + (bright ^ 8));
        }
    }
    out.extend(0..16);
    out
}

/// The 6×6×6 cube's levels. Not evenly spaced — the first step is large, which
/// is why a naive `value / 51` mapping produces visible banding in dark tones.
const CUBE_LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];

fn nearest_256(rgb: (u8, u8, u8)) -> u8 {
    let level = |v: u8| {
        let mut best = 0usize;
        let mut best_distance = i64::MAX;
        for (index, candidate) in CUBE_LEVELS.iter().enumerate() {
            let d = (i64::from(v) - i64::from(*candidate)).abs();
            if d < best_distance {
                best_distance = d;
                best = index;
            }
        }
        best
    };
    let (ri, gi, bi) = (level(rgb.0), level(rgb.1), level(rgb.2));
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let cube_rgb = (CUBE_LEVELS[ri], CUBE_LEVELS[gi], CUBE_LEVELS[bi]);

    // The 24 greys are finer than the cube's grey diagonal, so a near-grey —
    // `muted` is one — lands closer on the ramp than in the cube. Try both.
    let grey_step = |v: u8| {
        let step = (i64::from(v) - 8).clamp(0, 238) / 10;
        u8::try_from(step).expect("clamped to 0..=23")
    };
    let average = ((u16::from(rgb.0) + u16::from(rgb.1) + u16::from(rgb.2)) / 3) as u8;
    let grey = grey_step(average);
    let grey_value = 8 + grey * 10;
    let grey_rgb = (grey_value, grey_value, grey_value);

    if distance(rgb, grey_rgb) < distance(rgb, cube_rgb) {
        232 + grey
    } else {
        u8::try_from(cube_index).expect("the cube tops out at 231")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The 16 base colours with the RGB a default xterm palette paints them,
    /// used to measure how far a degradation drifted. Approximate by nature —
    /// a terminal may theme these — which is why nothing outside the tests
    /// depends on the values.
    const ANSI16: [(u8, u8, u8); 16] = [
        (0x00, 0x00, 0x00), // black
        (0x80, 0x00, 0x00), // red
        (0x00, 0x80, 0x00), // green
        (0x80, 0x80, 0x00), // yellow
        (0x00, 0x00, 0x80), // blue
        (0x80, 0x00, 0x80), // magenta
        (0x00, 0x80, 0x80), // cyan
        (0xc0, 0xc0, 0xc0), // white
        (0x80, 0x80, 0x80), // bright black
        (0xff, 0x00, 0x00), // bright red
        (0x00, 0xff, 0x00), // bright green
        (0xff, 0xff, 0x00), // bright yellow
        (0x00, 0x00, 0xff), // bright blue
        (0xff, 0x00, 0xff), // bright magenta
        (0x00, 0xff, 0xff), // bright cyan
        (0xff, 0xff, 0xff), // bright white
    ];

    fn config(theme: ThemeConfig) -> TuiConfig {
        TuiConfig {
            theme,
            ..TuiConfig::default()
        }
    }

    #[test]
    fn truecolor_uses_the_configured_value_exactly() {
        let (theme, bad) = Theme::resolve(&TuiConfig::default(), Depth::True);
        assert!(bad.is_empty());
        // Catppuccin Mocha's peach, which is what grove-lua defaults `accent`
        // to. Asserting the exact triple is the point: at this depth nothing
        // is allowed to approximate.
        assert_eq!(theme.color(Role::Accent), Color::Rgb(0xfa, 0xb3, 0x87));
    }

    #[test]
    fn distinct_colours_stay_distinct_after_degrading() {
        // SPEC §9's invariant, and not only for the palette that motivated it:
        // a set-resolution tuned to Catppuccin would be over-fitted. These are
        // five colours that crowd the same corner of the 16 — four reds and a
        // near-grey — which is the case the claim has to survive.
        let crowded = ThemeConfig {
            accent: "#ff0000".into(),
            clean: "#e01010".into(),
            dirty: "#c02020".into(),
            error: "#a03030".into(),
            muted: "#606060".into(),
        };
        let (theme, bad) = Theme::resolve(&config(crowded), Depth::Ansi16);
        assert!(bad.is_empty());
        let mut seen = Vec::new();
        for role in Role::ALL {
            let colour = theme.color(role);
            assert!(
                !seen.contains(&colour),
                "{} collapsed onto a colour another role already uses: {colour:?}",
                role.name()
            );
            seen.push(colour);
        }
    }

    #[test]
    fn sixteen_colours_keep_the_roles_apart() {
        // The acceptance criterion is legibility in a 16-colour terminal, and
        // legibility here means a user can tell clean from dirty from error at
        // a glance. Collapsing two roles onto one index would still "work" and
        // would be unusable.
        let (theme, _) = Theme::resolve(&TuiConfig::default(), Depth::Ansi16);
        let mut seen = Vec::new();
        for role in Role::ALL {
            let colour = theme.color(role);
            assert!(
                !seen.contains(&colour),
                "{} collapsed onto a colour another role already uses: {colour:?}",
                role.name()
            );
            seen.push(colour);
        }
    }

    #[test]
    fn degrading_picks_the_colour_a_human_would_name() {
        // Distinctness alone is satisfiable by any injective mapping, including
        // a nonsense one. These assert the mapping means something: the green
        // lands on a green, the red on a red, the grey on a grey.
        let (theme, _) = Theme::resolve(&TuiConfig::default(), Depth::Ansi16);
        assert_eq!(
            theme.color(Role::Clean),
            Color::Indexed(10),
            "#a6e3a1 is a green"
        );
        assert_eq!(
            theme.color(Role::Error),
            Color::Indexed(9),
            "#f38ba8 is a red"
        );
        assert_eq!(
            theme.color(Role::Muted),
            Color::Indexed(8),
            "#7f849c is a grey"
        );
    }

    #[test]
    fn a_desaturated_colour_stays_grey_instead_of_acquiring_a_hue() {
        // Guards the chroma threshold rather than an outcome. `muted` is a
        // blue-ish grey; derive a hue from it and it becomes blue, which reads
        // as a state rather than as absence. Lower GREY_CHROMA and this fails.
        assert_eq!(nearest_16((0x7f, 0x84, 0x9c)), 8, "a grey must stay grey");
        assert_eq!(nearest_16((0x80, 0x80, 0x80)), 8);
        assert_eq!(nearest_16((0x00, 0x00, 0x00)), 0);
        assert_eq!(nearest_16((0xff, 0xff, 0xff)), 15);
    }

    #[test]
    fn a_pastel_keeps_its_hue_rather_than_collapsing_to_white() {
        // The reason this module does not use distance at 16 colours: every
        // one of these is nearer to white than to its own hue by any metric,
        // and mapping them that way makes three roles identical.
        assert_eq!(nearest_16((0xa6, 0xe3, 0xa1)), 10, "pastel green");
        assert_eq!(nearest_16((0xf3, 0x8b, 0xa8)), 9, "pastel pink");
        assert_eq!(nearest_16((0xf9, 0xe2, 0xaf)), 11, "pastel yellow");
    }

    #[test]
    fn more_colours_means_a_closer_match() {
        // The property that makes the 256 path worth having at all: for every
        // role, the 256-colour choice is at least as close to what the user
        // configured as the 16-colour one. Asserting a particular index would
        // pin an implementation detail of the cube instead.
        let config = TuiConfig::default();
        let deep = Theme::resolve(&config, Depth::Ansi256).0;
        let shallow = Theme::resolve(&config, Depth::Ansi16).0;
        for role in Role::ALL {
            let wanted = parse_hex(role.configured(&config.theme)).expect("a default");
            let near = distance(wanted, rgb_of(deep.color(role)));
            let far = distance(wanted, rgb_of(shallow.color(role)));
            assert!(
                near <= far,
                "{}: 256 colours landed further away than 16 ({near} vs {far})",
                role.name()
            );
        }
    }

    /// What a terminal actually paints for an indexed colour, so a test can
    /// measure how far a degradation drifted.
    fn rgb_of(colour: Color) -> (u8, u8, u8) {
        match colour {
            Color::Rgb(r, g, b) => (r, g, b),
            Color::Indexed(i @ 0..=15) => ANSI16[usize::from(i)],
            Color::Indexed(i @ 16..=231) => {
                let i = usize::from(i) - 16;
                (
                    CUBE_LEVELS[i / 36],
                    CUBE_LEVELS[(i / 6) % 6],
                    CUBE_LEVELS[i % 6],
                )
            }
            Color::Indexed(i) => {
                let v = 8 + (i - 232) * 10;
                (v, v, v)
            }
            other => panic!("the theme emits no other colours: {other:?}"),
        }
    }

    #[test]
    fn an_unparseable_colour_falls_back_and_is_reported() {
        // SPEC §9: a broken config must never brick grove. The screen still
        // renders, in the default, and the user is told which setting was
        // wrong rather than left wondering why nothing changed.
        let (theme, bad) = Theme::resolve(
            &config(ThemeConfig {
                accent: "not a colour".into(),
                ..ThemeConfig::default()
            }),
            Depth::True,
        );
        assert_eq!(
            theme.color(Role::Accent),
            Color::Rgb(0xfa, 0xb3, 0x87),
            "must fall back to the default for that role"
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(bad[0].role, Role::Accent);
        assert!(
            bad[0].to_string().contains("not a colour"),
            "the message must quote what was written: {}",
            bad[0]
        );
    }

    #[test]
    fn one_bad_colour_does_not_take_the_others_with_it() {
        // Falling back *entirely* on one typo would be the other failure: the
        // user loses four settings they got right.
        let (theme, bad) = Theme::resolve(
            &config(ThemeConfig {
                dirty: "#gggggg".into(),
                clean: "#00ff00".into(),
                ..ThemeConfig::default()
            }),
            Depth::True,
        );
        assert_eq!(bad.len(), 1);
        assert_eq!(theme.color(Role::Clean), Color::Rgb(0, 0xff, 0));
    }

    #[test]
    fn hex_parsing_accepts_what_the_spec_shows_and_nothing_sloppy() {
        assert_eq!(parse_hex("#fab387"), Some((0xfa, 0xb3, 0x87)));
        assert_eq!(parse_hex("FAB387"), Some((0xfa, 0xb3, 0x87)));
        // Shorthand and names are rejected rather than guessed at: a config
        // that renders differently from the one the user believes they wrote
        // is worse than one that says it is wrong.
        assert_eq!(parse_hex("#fab"), None);
        assert_eq!(parse_hex("peach"), None);
        assert_eq!(parse_hex(""), None);
        assert_eq!(parse_hex("#fab38g"), None);
    }

    #[test]
    fn depth_comes_from_the_environment_not_a_probe() {
        assert_eq!(
            Depth::from_env(Some("truecolor"), Some("xterm")),
            Depth::True
        );
        assert_eq!(Depth::from_env(Some("24bit"), None), Depth::True);
        assert_eq!(
            Depth::from_env(None, Some("xterm-256color")),
            Depth::Ansi256
        );
        assert_eq!(Depth::from_env(None, Some("xterm")), Depth::Ansi16);
        // No TERM and `dumb` both mean assume the floor rather than hope.
        assert_eq!(Depth::from_env(None, None), Depth::Ansi16);
        assert_eq!(Depth::from_env(None, Some("dumb")), Depth::Ansi16);
    }

    #[test]
    fn corners_and_density_both_settings_render() {
        let rounded = Theme::resolve(&TuiConfig::default(), Depth::True).0;
        assert_eq!(rounded.border(), BorderType::Rounded);
        assert_eq!(rounded.row_height(), 2, "airy spends a line per row");
        assert_eq!(rounded.padding(), 1);

        let square = Theme::resolve(
            &TuiConfig {
                corners: Corners::Square,
                density: Density::Compact,
                ..TuiConfig::default()
            },
            Depth::True,
        )
        .0;
        assert_eq!(square.border(), BorderType::Plain);
        assert_eq!(square.row_height(), 1);
        assert_eq!(square.padding(), 0);
    }

    #[test]
    fn the_two_corner_settings_draw_different_glyphs() {
        // The setting exists to change what is on screen; asserting the enum
        // alone would pass even if both mapped to the same border set.
        let rounded = BorderType::border_symbols(BorderType::Rounded);
        let square = BorderType::border_symbols(BorderType::Plain);
        assert_eq!(rounded.top_left, "╭");
        assert_eq!(square.top_left, "┌");
    }

    /// Every source in the crate except this one, baked in at compile time.
    ///
    /// `include_str!` rather than reading the tree at run time, for two
    /// reasons. It cannot flake: the guard used to walk `CARGO_MANIFEST_DIR`,
    /// which is fixed when the test is compiled, so a binary built in a
    /// throwaway worktree — a reviewer exporting the branch, say — panicked
    /// with a bare `NotFound` once that directory was cleaned up, and the
    /// failure looked like a colour problem. And it cannot silently pass: a
    /// missing file is a compile error rather than one fewer file walked.
    ///
    /// The cost is a hand-kept list, so `the_guard_covers_every_source` holds
    /// it to the tree.
    const SCANNED: &[(&str, &str)] = &[
        ("columns.rs", include_str!("columns.rs")),
        ("dash.rs", include_str!("dash.rs")),
        ("diff.rs", include_str!("diff.rs")),
        ("empty.rs", include_str!("empty.rs")),
        ("endsession.rs", include_str!("endsession.rs")),
        ("events.rs", include_str!("events.rs")),
        ("help.rs", include_str!("help.rs")),
        ("keymap.rs", include_str!("keymap.rs")),
        ("main.rs", include_str!("main.rs")),
        ("mouse.rs", include_str!("mouse.rs")),
        ("overlay.rs", include_str!("overlay.rs")),
        ("palette.rs", include_str!("palette.rs")),
        ("prune.rs", include_str!("prune.rs")),
        ("repos.rs", include_str!("repos.rs")),
        ("select.rs", include_str!("select.rs")),
        ("sessions.rs", include_str!("sessions.rs")),
        ("statusbar.rs", include_str!("statusbar.rs")),
        ("terminal.rs", include_str!("terminal.rs")),
        ("terminals.rs", include_str!("terminals.rs")),
        ("text.rs", include_str!("text.rs")),
        ("userkeys.rs", include_str!("userkeys.rs")),
        ("worktrees.rs", include_str!("worktrees.rs")),
    ];

    #[test]
    fn no_hex_literal_outside_this_module() {
        // The rule call sites have to follow, asserted rather than trusted:
        // once a screen writes its own `#rrggbb`, changing the palette stops
        // changing the screen, and nobody finds out until it looks wrong.
        let mut offenders = Vec::new();
        for (name, text) in SCANNED {
            for (number, line) in text.lines().enumerate() {
                if looks_like_a_hex_colour(line) {
                    offenders.push(format!("{name}:{}: {}", number + 1, line.trim()));
                }
            }
        }
        assert!(
            offenders.is_empty(),
            "colours belong to the theme module, not to screens:\n{}",
            offenders.join("\n")
        );
    }

    #[test]
    fn the_guard_covers_every_source() {
        // The list above is hand-kept, so this walks the tree and fails if a
        // file escaped it — the screens in #18-#30 will add files and
        // subdirectories, and a guard that quietly stops covering the code is
        // worse than no guard.
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let Ok(root) = std::fs::read_dir(&src) else {
            // The only way this happens is the tree the test was compiled in
            // having been removed since — a reviewer's exported worktree, or a
            // checkout that no longer exists. There is nothing to compare
            // against, and the guard itself is unaffected because its sources
            // are baked in.
            //
            // Which is fine locally and not fine in CI: there the tree is the
            // runner's own checkout, so a missing one means the runner is
            // broken, and passing quietly would be the one path where this
            // check approves without checking. Panicking here everywhere would
            // just resurrect the flake for stale local binaries, so the
            // environment decides.
            assert!(
                std::env::var_os("CI").is_none(),
                "{} is gone, so the coverage check cannot run — in CI that means a broken checkout",
                src.display()
            );
            eprintln!(
                "skipping coverage check: {} is gone (stale test binary)",
                src.display()
            );
            return;
        };

        let mut found = Vec::new();
        let mut pending = vec![(src.clone(), root)];
        while let Some((dir, entries)) = pending.pop() {
            for entry in entries {
                let path = match entry {
                    Ok(entry) => entry.path(),
                    // Name the directory rather than panicking bare: a failure
                    // here is about the filesystem, not about colours, and the
                    // message is the only thing the next reader will have.
                    Err(error) => panic!("reading {}: {error}", dir.display()),
                };
                if path.is_dir() {
                    match std::fs::read_dir(&path) {
                        Ok(entries) => pending.push((path, entries)),
                        Err(error) => panic!("reading {}: {error}", path.display()),
                    }
                    continue;
                }
                if path.extension().is_none_or(|e| e != "rs") {
                    continue;
                }
                let name = path
                    .strip_prefix(&src)
                    .expect("walked from src")
                    .to_string_lossy()
                    .into_owned();
                if name != "theme.rs" {
                    found.push(name);
                }
            }
        }

        let listed: Vec<&str> = SCANNED.iter().map(|(name, _)| *name).collect();
        let missed: Vec<&String> = found
            .iter()
            .filter(|f| !listed.contains(&f.as_str()))
            .collect();
        assert!(
            missed.is_empty(),
            "these sources are not covered by the hex guard — add them to SCANNED: {missed:?}"
        );
    }

    /// `#` followed by exactly six hex digits, which is the shape this module
    /// parses and therefore the shape worth banning elsewhere.
    fn looks_like_a_hex_colour(line: &str) -> bool {
        let bytes = line.as_bytes();
        bytes.iter().enumerate().any(|(at, b)| {
            *b == b'#'
                && bytes
                    .get(at + 1..at + 7)
                    .is_some_and(|d| d.iter().all(u8::is_ascii_hexdigit))
                && bytes.get(at + 7).is_none_or(|n| !n.is_ascii_hexdigit())
        })
    }
}
