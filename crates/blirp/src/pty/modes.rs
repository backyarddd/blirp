//! Terminal state that vt100 does not replay in an attach snapshot (§6).
//!
//! vt100 keeps the screen, scrollback, cursor, cursor-key and keypad modes,
//! bracketed paste and mouse tracking. A client attaching mid-session must
//! also inherit everything else the program switched on, or its keys and
//! reports change under it: the kitty keyboard flags Claude Code and Codex
//! push (Shift+Enter, Ctrl+letter), focus reporting and win32-input-mode that
//! ConPTY requests once at startup, cursor style, scroll region, origin and
//! wraparound modes, and character sets. [`Modes`] follows these through the
//! same output vt100 sees, with the semantics xterm.js gives them, and
//! [`Modes::replay`] turns them back into escape sequences, like VS Code's
//! serialize addon does for its reconnects.

/// What [`Modes::advance`] saw that the caller must act on before vt100
/// processes the same byte.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Event {
    None,
    /// The byte ends a switch to the alternate screen (`?1049h`, `?47h`)
    /// while the normal screen is shown.
    EnterAlternate,
    /// The byte ends `CSI 3 J` (erase saved lines).
    ClearScrollback,
    /// The byte ends a kitty keyboard query (`CSI ? u`); the answer is
    /// [`Modes::kitty_reply`].
    KittyQuery,
}

/// Which screen a per-screen setting (kitty flags) belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Screen {
    Normal,
    Alternate,
}

/// Kitty keyboard protocol state: the active flags plus, per screen, the
/// saved flags and the push/pop stack (as xterm.js keeps them).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Kitty {
    flags: u16,
    normal_flags: u16,
    alternate_flags: u16,
    normal_stack: Vec<u16>,
    alternate_stack: Vec<u16>,
}

/// xterm.js drops the oldest entry beyond this many pushes.
const KITTY_STACK_MAX: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
struct State {
    rows: u16,
    alternate: bool,
    application_keypad: bool,
    focus_reporting: bool,
    win32_input: bool,
    color_scheme_updates: bool,
    sgr_pixel_mouse: bool,
    wraparound: bool,
    reverse_wraparound: bool,
    insert: bool,
    newline: bool,
    origin: bool,
    /// DECSCUSR parameter; `None` is the terminal's configured style.
    cursor_style: Option<u16>,
    /// Scroll region (0-based, inclusive) per screen; `None` is the whole screen.
    normal_region: Option<(u16, u16)>,
    alternate_region: Option<(u16, u16)>,
    /// Designated character sets G0-G3 (final byte, `B` is US ASCII) and
    /// which of them is invoked into GL.
    charsets: [u8; 4],
    gl: u8,
    kitty: Kitty,
}

impl State {
    fn new(rows: u16) -> Self {
        Self {
            rows,
            alternate: false,
            application_keypad: false,
            focus_reporting: false,
            win32_input: false,
            color_scheme_updates: false,
            sgr_pixel_mouse: false,
            wraparound: true,
            reverse_wraparound: false,
            insert: false,
            newline: false,
            origin: false,
            cursor_style: None,
            normal_region: None,
            alternate_region: None,
            charsets: [b'B'; 4],
            gl: 0,
            kitty: Kitty::default(),
        }
    }

    fn region_mut(&mut self) -> &mut Option<(u16, u16)> {
        if self.alternate {
            &mut self.alternate_region
        } else {
            &mut self.normal_region
        }
    }

    /// DECSTR: what xterm.js's soft reset clears among the tracked state.
    fn soft_reset(&mut self) {
        self.application_keypad = false;
        self.insert = false;
        self.origin = false;
        self.wraparound = true;
        *self.region_mut() = None;
        self.charsets = [b'B'; 4];
        self.gl = 0;
    }

    fn kitty_stack_mut(&mut self) -> &mut Vec<u16> {
        if self.alternate {
            &mut self.kitty.alternate_stack
        } else {
            &mut self.kitty.normal_stack
        }
    }

    fn set_alternate(&mut self, on: bool) {
        if on == self.alternate {
            return;
        }
        if on {
            self.kitty.normal_flags = self.kitty.flags;
            self.kitty.flags = self.kitty.alternate_flags;
        } else {
            self.kitty.alternate_flags = self.kitty.flags;
            self.kitty.flags = self.kitty.normal_flags;
        }
        self.alternate = on;
    }

    fn private_mode(&mut self, mode: u16, on: bool, event: &mut Event) {
        match mode {
            6 => self.origin = on,
            7 => self.wraparound = on,
            45 => self.reverse_wraparound = on,
            66 => self.application_keypad = on,
            1004 => self.focus_reporting = on,
            // Any other mouse encoding replaces SGR-pixels (vt100 keeps those).
            1005 | 1006 | 1015 if on => self.sgr_pixel_mouse = false,
            1016 => self.sgr_pixel_mouse = on,
            47 | 1047 | 1049 => {
                if on && !self.alternate && mode != 1047 {
                    *event = Event::EnterAlternate;
                }
                self.set_alternate(on);
            }
            2031 => self.color_scheme_updates = on,
            9001 => self.win32_input = on,
            _ => {}
        }
    }
}

struct Performer<'a> {
    state: &'a mut State,
    event: Event,
}

fn first(params: &vte::Params, default: u16) -> u16 {
    match params.iter().next().and_then(|p| p.first()) {
        Some(&0) | None => default,
        Some(&n) => n,
    }
}

impl vte::Perform for Performer<'_> {
    fn execute(&mut self, byte: u8) {
        match byte {
            0x0e => self.state.gl = 1, // SO
            0x0f => self.state.gl = 0, // SI
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        let s = &mut *self.state;
        match (intermediates, byte) {
            ([], b'c') => *s = State::new(s.rows), // RIS
            ([], b'=') => s.application_keypad = true,
            ([], b'>') => s.application_keypad = false,
            ([], b'n') => s.gl = 2, // LS2
            ([], b'o') => s.gl = 3, // LS3
            // SCS: `(` `)` `*` `+` designate G0-G3 (consecutive bytes).
            ([g @ b'('..=b'+'], set) => s.charsets[usize::from(g - b'(')] = set,
            _ => {}
        }
    }

    fn csi_dispatch(
        &mut self,
        params: &vte::Params,
        intermediates: &[u8],
        ignore: bool,
        action: char,
    ) {
        if ignore {
            return;
        }
        let s = &mut *self.state;
        match (intermediates, action) {
            ([b'?'], 'h' | 'l') => {
                for p in params.iter() {
                    if let Some(&mode) = p.first() {
                        s.private_mode(mode, action == 'h', &mut self.event);
                    }
                }
            }
            ([], 'h' | 'l') => {
                for p in params.iter() {
                    match p.first() {
                        Some(4) => s.insert = action == 'h',
                        Some(20) => s.newline = action == 'h',
                        _ => {}
                    }
                }
            }
            ([], 'J') if first(params, 0) == 3 => self.event = Event::ClearScrollback,
            ([], 'r') => {
                let rows = s.rows.max(1);
                let top = first(params, 1);
                let bottom = match params.iter().nth(1).and_then(|p| p.first()) {
                    Some(&b) if b != 0 && b <= rows => b,
                    _ => rows,
                };
                // xterm ignores a region that is not at least two lines.
                if top < bottom {
                    *s.region_mut() = if top == 1 && bottom == rows {
                        None
                    } else {
                        Some((top - 1, bottom - 1))
                    };
                }
            }
            ([b' '], 'q') => {
                s.cursor_style = match first(params, 0) {
                    0 => None,
                    n => Some(n),
                };
            }
            ([b'!'], 'p') => s.soft_reset(),
            ([b'?'], 'u') => self.event = Event::KittyQuery,
            ([b'='], 'u') => {
                let flags = first(params, 0);
                let mode = params
                    .iter()
                    .nth(1)
                    .and_then(|p| p.first())
                    .copied()
                    .unwrap_or(1);
                match mode {
                    1 => s.kitty.flags = flags,
                    2 => s.kitty.flags |= flags,
                    3 => s.kitty.flags &= !flags,
                    _ => {}
                }
            }
            ([b'>'], 'u') => {
                let flags = first(params, 0);
                let current = s.kitty.flags;
                let stack = s.kitty_stack_mut();
                if stack.len() >= KITTY_STACK_MAX {
                    stack.remove(0);
                }
                stack.push(current);
                s.kitty.flags = flags;
            }
            ([b'<'], 'u') => {
                let n = first(params, 1).max(1);
                let mut flags = s.kitty.flags;
                let stack = s.kitty_stack_mut();
                for _ in 0..n {
                    match stack.pop() {
                        Some(f) => flags = f,
                        None => break,
                    }
                }
                if stack.is_empty() {
                    flags = 0;
                }
                s.kitty.flags = flags;
            }
            _ => {}
        }
    }
}

pub(super) struct Modes {
    parser: vte::Parser,
    state: State,
}

impl Modes {
    pub(super) fn new(rows: u16) -> Self {
        Self {
            parser: vte::Parser::new(),
            state: State::new(rows),
        }
    }

    /// Follow one byte of program output.
    pub(super) fn advance(&mut self, byte: u8) -> Event {
        let mut p = Performer {
            state: &mut self.state,
            event: Event::None,
        };
        self.parser.advance(&mut p, &[byte]);
        p.event
    }

    /// Like xterm.js, a resize resets the scroll regions.
    pub(super) fn resize(&mut self, rows: u16) {
        self.state.rows = rows;
        self.state.normal_region = None;
        self.state.alternate_region = None;
    }

    /// The answer to a kitty keyboard query, as xterm.js gives it.
    pub(super) fn kitty_reply(&self) -> Vec<u8> {
        format!("\x1b[?{}u", self.state.kitty.flags).into_bytes()
    }

    /// Kitty keyboard flags and stack of `screen`, to replay while that
    /// screen is active on a terminal that starts with none.
    pub(super) fn kitty(&self, screen: Screen) -> Vec<u8> {
        let k = &self.state.kitty;
        let (flags, stack) = match (screen, self.state.alternate) {
            (Screen::Normal, false) => (k.flags, &k.normal_stack),
            (Screen::Normal, true) => (k.normal_flags, &k.normal_stack),
            (Screen::Alternate, true) => (k.flags, &k.alternate_stack),
            (Screen::Alternate, false) => (k.alternate_flags, &k.alternate_stack),
        };
        let mut out = Vec::new();
        // Each push saves the current flags, so set the bottom entry first
        // and push the rest; the last push sets the active flags.
        match stack.split_first() {
            None if flags == 0 => {}
            None => out.extend(format!("\x1b[={flags};1u").as_bytes()),
            Some((bottom, rest)) => {
                if *bottom != 0 {
                    out.extend(format!("\x1b[={bottom};1u").as_bytes());
                }
                for f in rest.iter().chain([&flags]) {
                    out.extend(format!("\x1b[>{f}u").as_bytes());
                }
            }
        }
        out
    }

    /// Everything else, for a terminal that already shows the content with
    /// the cursor at `cursor` (0-based row, column; absolute).
    pub(super) fn replay(&self, cursor: (u16, u16)) -> Vec<u8> {
        let s = &self.state;
        let mut out: Vec<u8> = Vec::new();
        out.extend(if s.application_keypad {
            &b"\x1b="[..]
        } else {
            b"\x1b>"
        });
        for (on, mode) in [
            (s.focus_reporting, 1004),
            (s.win32_input, 9001),
            (s.color_scheme_updates, 2031),
            (s.sgr_pixel_mouse, 1016),
            (s.reverse_wraparound, 45),
        ] {
            if on {
                out.extend(format!("\x1b[?{mode}h").as_bytes());
            }
        }
        if !s.wraparound {
            out.extend(b"\x1b[?7l");
        }
        if s.insert {
            out.extend(b"\x1b[4h");
        }
        if s.newline {
            out.extend(b"\x1b[20h");
        }
        if let Some(n) = s.cursor_style {
            out.extend(format!("\x1b[{n} q").as_bytes());
        }
        for (i, g) in [b'(', b')', b'*', b'+'].iter().enumerate() {
            if s.charsets[i] != b'B' {
                out.extend([0x1b, *g, s.charsets[i]]);
            }
        }
        match s.gl {
            1 => out.push(0x0e),
            2 => out.extend(b"\x1bn"),
            3 => out.extend(b"\x1bo"),
            _ => {}
        }
        let region = if s.alternate {
            s.alternate_region
        } else {
            s.normal_region
        };
        let region = region.filter(|&(_, bottom)| bottom < s.rows);
        if region.is_some() || s.origin {
            // DECSTBM and DECOM home the cursor: put it back, relative to
            // the region when origin mode is on.
            let top = region.map_or(0, |(top, _)| top);
            if let Some((top, bottom)) = region {
                out.extend(format!("\x1b[{};{}r", top + 1, bottom + 1).as_bytes());
            }
            let (row, col) = cursor;
            let row = if s.origin {
                out.extend(b"\x1b[?6h");
                row.saturating_sub(top)
            } else {
                row
            };
            out.extend(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Emulator, SCROLLBACK, snapshot};
    use super::*;

    /// A fresh client that applied `p`'s snapshot.
    fn reattach(p: &mut Emulator) -> Emulator {
        let snap = snapshot(p);
        let mut c = Emulator::new(snap.rows, snap.cols, SCROLLBACK);
        c.feed(snap.data.as_bytes());
        c
    }

    fn assert_same(p: &mut Emulator) {
        let c = reattach(p);
        assert_eq!(c.modes.state, p.modes.state);
        let (cs, ps) = (c.parser.screen(), p.parser.screen());
        // Keypad mode is compared through `state` (vt100 misses `?66h`).
        assert_eq!(cs.application_cursor(), ps.application_cursor());
        assert_eq!(cs.bracketed_paste(), ps.bracketed_paste());
        assert_eq!(cs.mouse_protocol_mode(), ps.mouse_protocol_mode());
        assert_eq!(cs.mouse_protocol_encoding(), ps.mouse_protocol_encoding());
        assert_eq!(cs.alternate_screen(), ps.alternate_screen());
        assert_eq!(cs.contents(), ps.contents());
        assert_eq!(cs.cursor_position(), ps.cursor_position());
        assert_eq!(cs.hide_cursor(), ps.hide_cursor());
    }

    // ConPTY asks for focus reports and win32-input-mode once, before any
    // client attached; every later client must still get them.
    #[test]
    fn conpty_startup_modes_survive_a_reattach() {
        let mut p = Emulator::new(24, 80, SCROLLBACK);
        p.feed(b"\x1b[6n\x1b[c\x1b[?1004h\x1b[?9001h\x1b[1;1HPS C:\\> ");
        let snap = String::from_utf8(snapshot(&mut p).data.into_bytes()).unwrap();
        assert!(
            snap.contains("\x1b[?1004h") && snap.contains("\x1b[?9001h"),
            "{snap:?}"
        );
        assert_same(&mut p);
    }

    #[test]
    fn kitty_keyboard_flags_survive_a_reattach() {
        let mut p = Emulator::new(24, 80, SCROLLBACK);
        // Claude Code / Codex: query, then push "disambiguate escape codes".
        p.feed(b"\x1b[?u\x1b[c\x1b[>1u> ");
        assert_eq!(p.modes.state.kitty.flags, 1);
        let c = reattach(&mut p);
        assert_eq!(c.modes.state.kitty.flags, 1);
        assert_same(&mut p);
        // Nested pushes, a set and a pop.
        p.feed(b"\x1b[>5u\x1b[=8;2u\x1b[>31u\x1b[<u");
        assert_eq!(p.modes.state.kitty.flags, 13);
        assert_eq!(p.modes.state.kitty.normal_stack, [0, 1]);
        assert_same(&mut p);
        p.feed(b"\x1b[<9u");
        assert_eq!(p.modes.state.kitty.flags, 0);
        assert_same(&mut p);
    }

    // Each screen keeps its own kitty flags; the snapshot sets the normal
    // screen's before switching.
    #[test]
    fn kitty_flags_per_screen_survive_a_reattach() {
        let mut p = Emulator::new(24, 80, SCROLLBACK);
        p.feed(b"\x1b[>1u$ vim\r\n\x1b[?1049h\x1b[>3u\x1b[>11utext");
        assert_same(&mut p);
        let mut c = reattach(&mut p);
        c.feed(b"\x1b[?1049l");
        p.feed(b"\x1b[?1049l");
        assert_eq!(c.modes.state, p.modes.state);
        assert_eq!(p.modes.state.kitty.flags, 1);
    }

    #[test]
    fn kitty_queries_are_answered_in_order() {
        let mut p = Emulator::new(24, 80, 0);
        p.feed(b"\x1b[?u\x1b[c");
        assert_eq!(p.parser.callbacks().replies, b"\x1b[?0u\x1b[?1;2c");
        p.parser.callbacks_mut().replies.clear();
        p.feed(b"\x1b[>1u\x1b[?");
        p.feed(b"u");
        assert_eq!(p.parser.callbacks().replies, b"\x1b[?1u");
    }

    #[test]
    fn terminal_modes_survive_a_reattach() {
        let mut p = Emulator::new(10, 40, SCROLLBACK);
        p.feed(
            b"\x1b[?1h\x1b[?66h\x1b[?2004h\x1b[?1002h\x1b[?1016h\x1b[?2031h\x1b[?45h\
              \x1b[4h\x1b[5 q\x1b(0\x1b)A\x0e\x1b[?25lhello",
        );
        assert_same(&mut p);
        let snap = snapshot(&mut p).data;
        for want in [
            "\x1b=",
            "\x1b[?1016h",
            "\x1b[5 q",
            "\x1b(0",
            "\x1b)A",
            "\x0e",
            "\x1b[4h",
        ] {
            assert!(snap.contains(want), "{want:?} missing from {snap:?}");
        }
        // Wraparound off, then a soft reset puts it and more back.
        p.feed(b"\x1b[?7l");
        assert_same(&mut p);
        p.feed(b"\x1b[!p");
        assert!(p.modes.state.wraparound && !p.modes.state.insert);
        assert_same(&mut p);
    }

    // Scroll region and origin mode: the cursor ends where it was, relative
    // to the region when origin mode is on.
    #[test]
    fn scroll_region_and_origin_survive_a_reattach() {
        let mut p = Emulator::new(10, 40, SCROLLBACK);
        p.feed(b"status line\x1b[2;9r\x1b[5;7Hx");
        assert_eq!(p.modes.state.normal_region, Some((1, 8)));
        assert_same(&mut p);
        p.feed(b"\x1b[?6h\x1b[3;4Hy");
        assert_same(&mut p);
        // Scrolling inside the region keeps the status line in both.
        let mut c = reattach(&mut p);
        let scroll = b"\x1b[8;1H\r\n\r\n\r\nz";
        p.feed(scroll);
        c.feed(scroll);
        assert_eq!(c.parser.screen().contents(), p.parser.screen().contents());
        // A resize resets the region, as in xterm.js.
        p.resize(12, 40);
        assert_eq!(p.modes.state.normal_region, None);
    }

    // The normal screen behind a full-screen program comes back when it
    // exits, with its cursor.
    #[test]
    fn normal_screen_behind_the_alternate_one_survives_a_reattach() {
        let mut p = Emulator::new(4, 20, SCROLLBACK);
        for i in 0..6 {
            p.feed(format!("line {i}\r\n").as_bytes());
        }
        p.feed(b"$ vim");
        let normal = p.parser.screen().contents();
        let cursor = p.parser.screen().cursor_position();
        p.feed(b"\x1b[?1049h\x1b[H\x1b[2Jediting");
        assert!(p.normal.is_some());
        assert_same(&mut p);
        let mut c = reattach(&mut p);
        c.feed(b"\x1b[?1049l");
        assert_eq!(c.parser.screen().contents(), normal);
        assert_eq!(c.parser.screen().cursor_position(), cursor);
        p.feed(b"\x1b[?1049l");
        assert!(p.normal.is_none());
    }

    // Sequences split across PTY reads are still followed.
    #[test]
    fn split_sequences_are_followed() {
        let mut p = Emulator::new(4, 20, SCROLLBACK);
        p.feed(b"\x1b[?10");
        p.feed(b"04h\x1b[>");
        p.feed(b"1u");
        assert!(p.modes.state.focus_reporting);
        assert_eq!(p.modes.state.kitty.flags, 1);
        for i in 0..10 {
            p.feed(format!("old {i}\r\n").as_bytes());
        }
        p.feed(b"\x1b[3");
        p.feed(b"J");
        p.parser.screen_mut().set_scrollback(usize::MAX);
        assert_eq!(p.parser.screen().scrollback(), 0);
    }

    // RIS resets everything tracked.
    #[test]
    fn full_reset_clears_the_modes() {
        let mut p = Emulator::new(4, 20, SCROLLBACK);
        p.feed(b"\x1b[?1004h\x1b[>1u\x1b[3 q\x1b[2;3r\x1bc");
        assert_eq!(p.modes.state, State::new(4));
    }
}
