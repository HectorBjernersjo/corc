//! Emitting images with the kitty graphics protocol, through tmux, into a
//! pane that is usually not the active one.
//!
//! Two things make this harder than `kitten icat`. First, tmux knows nothing
//! about the pixels: escape sequences reach the terminal via the passthrough
//! wrapper, so tmux cannot clip them to the pane or repaint them after a
//! redraw. Second — and this is what rules out the direct placement icat uses
//! by default — a direct placement lands at the terminal's *physical* cursor,
//! which tmux only parks inside our pane while that pane is active. A browser
//! view the user is not focused on would paint itself over whatever pane is.
//!
//! Unicode placeholders (`U=1`) solve both at once: the image is transmitted
//! once with no position, and its location is expressed as ordinary text
//! cells — U+10EEEE carrying row/column diacritics, tinted with the image id.
//! tmux lays out, clips and repaints those cells like any other text, so the
//! image follows the pane. Frames then cost only the image data: the cell grid
//! is painted once per resize and left alone.

use std::fmt;
use std::io::{self, Write};

/// The placeholder character every image cell carries.
const PLACEHOLDER: char = '\u{10EEEE}';

/// Kitty's canonical row/column diacritics, from `gen/rowcolumn-diacritics.txt`
/// (combining marks of class 230 in Unicode 6.0.0, minus the ones in common
/// use). Index N encodes row/column N, which caps a placement at 297 cells per
/// axis — far past any real pane.
const DIACRITICS: [u32; 297] = [
    0x0305, 0x030d, 0x030e, 0x0310, 0x0312, 0x033d, 0x033e, 0x033f, 0x0346, 0x034a, 0x034b, 0x034c,
    0x0350, 0x0351, 0x0352, 0x0357, 0x035b, 0x0363, 0x0364, 0x0365, 0x0366, 0x0367, 0x0368, 0x0369,
    0x036a, 0x036b, 0x036c, 0x036d, 0x036e, 0x036f, 0x0483, 0x0484, 0x0485, 0x0486, 0x0487, 0x0592,
    0x0593, 0x0594, 0x0595, 0x0597, 0x0598, 0x0599, 0x059c, 0x059d, 0x059e, 0x059f, 0x05a0, 0x05a1,
    0x05a8, 0x05a9, 0x05ab, 0x05ac, 0x05af, 0x05c4, 0x0610, 0x0611, 0x0612, 0x0613, 0x0614, 0x0615,
    0x0616, 0x0617, 0x0657, 0x0658, 0x0659, 0x065a, 0x065b, 0x065d, 0x065e, 0x06d6, 0x06d7, 0x06d8,
    0x06d9, 0x06da, 0x06db, 0x06dc, 0x06df, 0x06e0, 0x06e1, 0x06e2, 0x06e4, 0x06e7, 0x06e8, 0x06eb,
    0x06ec, 0x0730, 0x0732, 0x0733, 0x0735, 0x0736, 0x073a, 0x073d, 0x073f, 0x0740, 0x0741, 0x0743,
    0x0745, 0x0747, 0x0749, 0x074a, 0x07eb, 0x07ec, 0x07ed, 0x07ee, 0x07ef, 0x07f0, 0x07f1, 0x07f3,
    0x0816, 0x0817, 0x0818, 0x0819, 0x081b, 0x081c, 0x081d, 0x081e, 0x081f, 0x0820, 0x0821, 0x0822,
    0x0823, 0x0825, 0x0826, 0x0827, 0x0829, 0x082a, 0x082b, 0x082c, 0x082d, 0x0951, 0x0953, 0x0954,
    0x0f82, 0x0f83, 0x0f86, 0x0f87, 0x135d, 0x135e, 0x135f, 0x17dd, 0x193a, 0x1a17, 0x1a75, 0x1a76,
    0x1a77, 0x1a78, 0x1a79, 0x1a7a, 0x1a7b, 0x1a7c, 0x1b6b, 0x1b6d, 0x1b6e, 0x1b6f, 0x1b70, 0x1b71,
    0x1b72, 0x1b73, 0x1cd0, 0x1cd1, 0x1cd2, 0x1cda, 0x1cdb, 0x1ce0, 0x1dc0, 0x1dc1, 0x1dc3, 0x1dc4,
    0x1dc5, 0x1dc6, 0x1dc7, 0x1dc8, 0x1dc9, 0x1dcb, 0x1dcc, 0x1dd1, 0x1dd2, 0x1dd3, 0x1dd4, 0x1dd5,
    0x1dd6, 0x1dd7, 0x1dd8, 0x1dd9, 0x1dda, 0x1ddb, 0x1ddc, 0x1ddd, 0x1dde, 0x1ddf, 0x1de0, 0x1de1,
    0x1de2, 0x1de3, 0x1de4, 0x1de5, 0x1de6, 0x1dfe, 0x20d0, 0x20d1, 0x20d4, 0x20d5, 0x20d6, 0x20d7,
    0x20db, 0x20dc, 0x20e1, 0x20e7, 0x20e9, 0x20f0, 0x2cef, 0x2cf0, 0x2cf1, 0x2de0, 0x2de1, 0x2de2,
    0x2de3, 0x2de4, 0x2de5, 0x2de6, 0x2de7, 0x2de8, 0x2de9, 0x2dea, 0x2deb, 0x2dec, 0x2ded, 0x2dee,
    0x2def, 0x2df0, 0x2df1, 0x2df2, 0x2df3, 0x2df4, 0x2df5, 0x2df6, 0x2df7, 0x2df8, 0x2df9, 0x2dfa,
    0x2dfb, 0x2dfc, 0x2dfd, 0x2dfe, 0x2dff, 0xa66f, 0xa67c, 0xa67d, 0xa6f0, 0xa6f1, 0xa8e0, 0xa8e1,
    0xa8e2, 0xa8e3, 0xa8e4, 0xa8e5, 0xa8e6, 0xa8e7, 0xa8e8, 0xa8e9, 0xa8ea, 0xa8eb, 0xa8ec, 0xa8ed,
    0xa8ee, 0xa8ef, 0xa8f0, 0xa8f1, 0xaab0, 0xaab2, 0xaab3, 0xaab7, 0xaab8, 0xaabe, 0xaabf, 0xaac1,
    0xfe20, 0xfe21, 0xfe22, 0xfe23, 0xfe24, 0xfe25, 0xfe26, 0x10a0f, 0x10a38, 0x1d185, 0x1d186,
    0x1d187, 0x1d188, 0x1d189, 0x1d1aa, 0x1d1ab, 0x1d1ac, 0x1d1ad, 0x1d242, 0x1d243, 0x1d244,
];

/// The largest row or column a placement can address.
pub const MAX_CELLS: u16 = DIACRITICS.len() as u16;

/// Image id corc streams under. Kept below 2^24 so the id fits entirely in the
/// placeholder cells' 24-bit foreground colour and no third diacritic is
/// needed; reusing one id per pane also means each new frame *replaces* the
/// previous image rather than accumulating in the terminal's image store.
const IMAGE_ID: u32 = 0xC0_9C_01;

/// Payload bytes per `_G` chunk. The protocol caps a chunk at 4096 base64
/// characters and requires each to stay 4-aligned so it decodes on its own.
const CHUNK: usize = 4000;

/// How the *outer* terminal identifies itself — never `$TERM` as seen from
/// inside tmux, which only ever says `tmux-256color` or `screen-*`.
///
/// Both answers are carried because neither alone names every terminal:
/// konsole and wezterm both hand their client a plain `xterm-256color` and
/// only identify themselves in XTVERSION, while a terminal that never answers
/// XTVERSION can still be recognised from `$TERM` (`xterm-ghostty`,
/// `xterm-kitty`). Naming the terminal matters even when the answer is no —
/// telling a wezterm user that `xterm-256color` cannot draw images sends them
/// looking in the wrong place.
#[derive(Debug, Default, Clone)]
pub struct Terminal {
    /// `$TERM` the tmux client attached with.
    pub term: String,
    /// XTVERSION self-report, e.g. `WezTerm 20240203-110809-5046fc22`. Empty
    /// when the terminal stayed silent.
    pub program: String,
}

impl Terminal {
    /// Nothing to judge: no tmux client is attached, which means "don't know
    /// yet" rather than "cannot".
    pub fn is_unknown(&self) -> bool {
        self.term.is_empty() && self.program.is_empty()
    }

    /// Whether this terminal can be expected to draw kitty graphics *with
    /// unicode placeholders*, which is a much shorter list than the one for the
    /// protocol itself. `CORC_BROWSER_GRAPHICS=1` forces it on for a terminal
    /// not named here.
    ///
    /// Matched as whole tokens rather than substrings, so `xterm-256color`
    /// cannot be read as a terminal nobody is running.
    ///
    /// wezterm is deliberately *not* on the list, despite implementing the
    /// kitty image protocol: it ignores `U=1`. Probed on 20260815-143815, a
    /// virtual placement is drawn as an ordinary placement at the cursor and
    /// the placeholder cells are then rendered as literal glyphs — three rows
    /// of image plus three rows of junk — whether the placement is asked for as
    /// one `a=T,U=1` command or as `a=t` followed by `a=p,U=1`. Direct
    /// placements draw correctly, which is not what corc needs (see ADR-0002).
    /// The changelog has never mentioned `U+10EEEE`, so this is unimplemented
    /// rather than broken.
    pub fn draws_graphics(&self) -> bool {
        if std::env::var("CORC_BROWSER_GRAPHICS").is_ok_and(|v| v == "1") {
            return true;
        }
        const KNOWN: [&str; 4] = ["ghostty", "kitty", "konsole", "rio"];
        format!("{} {}", self.term, self.program)
            .to_lowercase()
            .split(|c: char| !c.is_ascii_alphanumeric())
            .any(|token| KNOWN.contains(&token))
    }
}

impl fmt::Display for Terminal {
    /// The XTVERSION name when there is one: it is the name the user knows
    /// their terminal by, and telling someone running WezTerm that
    /// `xterm-256color` cannot draw images is exactly the confusion this type
    /// exists to avoid.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.program.is_empty() {
            f.write_str(&self.term)
        } else {
            f.write_str(&self.program)
        }
    }
}

/// Wrap an escape sequence for tmux's passthrough (`allow-passthrough`).
/// Every ESC inside the payload has to be doubled or tmux ends the wrapper
/// early on the first one.
fn passthrough(payload: &str) -> String {
    format!("\x1bPtmux;{}\x1b\\", payload.replace('\x1b', "\x1b\x1b"))
}

/// Send one PNG frame as a virtual placement `cols`×`rows` cells in size.
///
/// `png_base64` arrives straight from CDP's `Page.screencastFrame`, which
/// already encodes exactly what the protocol wants — corc never decodes a
/// pixel. Transmitting under the same id each time replaces the stored image,
/// and the placeholder cells painted by `paint_grid` pick up the new content
/// without being rewritten.
///
/// Keep the virtual placement in this *one* command. The kitty docs also spell
/// it as `a=t` followed by `a=p,U=1`, and rio 0.5.25 draws nothing at all for
/// that spelling while drawing this one correctly.
pub fn transmit(out: &mut impl Write, png_base64: &str, cols: u16, rows: u16) -> io::Result<()> {
    let mut chunks = png_base64.as_bytes().chunks(CHUNK).peekable();
    let mut first = true;
    while let Some(chunk) = chunks.next() {
        let more = u8::from(chunks.peek().is_some());
        // Only the first chunk carries the image's control keys; the rest just
        // continue it. q=2 suppresses both the ok and the error replies, which
        // we have no terminal reader to consume.
        let control = if first {
            format!("a=T,q=2,f=100,t=d,i={IMAGE_ID},U=1,c={cols},r={rows},m={more}")
        } else {
            format!("m={more}")
        };
        first = false;
        out.write_all(
            passthrough(&format!(
                "\x1b_G{control};{}\x1b\\",
                std::str::from_utf8(chunk).unwrap_or_default()
            ))
            .as_bytes(),
        )?;
    }
    out.flush()
}

/// Forget the streamed image, so quitting does not leave it in the terminal's
/// image store.
pub fn delete(out: &mut impl Write) -> io::Result<()> {
    out.write_all(passthrough(&format!("\x1b_Ga=d,d=I,i={IMAGE_ID},q=2\x1b\\")).as_bytes())?;
    out.flush()
}

/// Paint the `cols`×`rows` block of placeholder cells at `top` that the
/// transmitted image shows through, anchored at column 1 of the pane.
///
/// This is plain text as far as tmux is concerned, which is the entire point:
/// tmux clips it to the pane and repaints it after a resize or window switch.
/// Only the first cell of each row spells out its row and column; kitty
/// continues the run for cells whose diacritics are omitted, which keeps a
/// full-pane grid at four bytes per cell.
pub fn paint_grid(out: &mut impl Write, top: u16, cols: u16, rows: u16) -> io::Result<()> {
    let cols = cols.min(MAX_CELLS);
    let rows = rows.min(MAX_CELLS);
    // The id lives in the cells' foreground colour, one byte per channel.
    let mut buf = format!(
        "\x1b[38;2;{};{};{}m",
        IMAGE_ID >> 16 & 0xff,
        IMAGE_ID >> 8 & 0xff,
        IMAGE_ID & 0xff
    );
    for row in 0..rows {
        buf.push_str(&format!("\x1b[{};1H", top + row + 1));
        buf.push(PLACEHOLDER);
        buf.push(diacritic(row));
        buf.push(diacritic(0));
        for _ in 1..cols {
            buf.push(PLACEHOLDER);
        }
    }
    buf.push_str("\x1b[0m");
    out.write_all(buf.as_bytes())?;
    out.flush()
}

fn diacritic(index: u16) -> char {
    char::from_u32(DIACRITICS[(index as usize).min(DIACRITICS.len() - 1)]).unwrap_or('\u{0305}')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passthrough_doubles_every_escape_so_tmux_forwards_the_whole_sequence() {
        let wrapped = passthrough("\x1b_Ga=T;AAAA\x1b\\");
        assert!(wrapped.starts_with("\x1bPtmux;"));
        assert!(wrapped.ends_with("\x1b\\"));
        // Both inner escapes doubled, and the wrapper's own two left alone.
        assert_eq!(wrapped.matches("\x1b\x1b").count(), 2);
    }

    #[test]
    fn a_frame_larger_than_one_chunk_is_split_with_continuation_markers() {
        let mut out = Vec::new();
        let payload = "A".repeat(CHUNK + 10);
        transmit(&mut out, &payload, 20, 10).unwrap();
        let text = String::from_utf8(out).unwrap();

        // First chunk carries the placement keys and says more is coming.
        assert!(text.contains("f=100,t=d,i=12622849,U=1,c=20,r=10,m=1"));
        // The tail is a bare continuation that closes the transmission.
        assert!(text.contains("_Gm=0;"));
        assert_eq!(text.matches("_G").count(), 2);
    }

    #[test]
    fn a_frame_within_one_chunk_is_sent_as_a_single_complete_transmission() {
        let mut out = Vec::new();
        transmit(&mut out, "AAAA", 4, 2).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("m=0"));
        assert_eq!(text.matches("_G").count(), 1);
    }

    #[test]
    fn the_grid_encodes_row_numbers_and_tints_cells_with_the_image_id() {
        let mut out = Vec::new();
        paint_grid(&mut out, 1, 3, 2).unwrap();
        let text = String::from_utf8(out).unwrap();

        // Image id split across the three colour channels.
        assert!(text.starts_with("\x1b[38;2;192;156;1m"));
        // One placeholder per cell; rows 0 and 1 carry their own diacritic.
        assert_eq!(text.matches(PLACEHOLDER).count(), 6);
        assert!(text.contains(&format!("{PLACEHOLDER}\u{0305}\u{0305}")));
        assert!(text.contains(&format!("{PLACEHOLDER}\u{030d}\u{0305}")));
        // Rows are placed below `top`, which is 0-indexed while CUP is not.
        assert!(text.contains("\x1b[2;1H"));
        assert!(text.contains("\x1b[3;1H"));
    }

    #[test]
    fn a_grid_beyond_the_diacritic_table_is_clamped_rather_than_panicking() {
        let mut out = Vec::new();
        paint_grid(&mut out, 0, MAX_CELLS + 50, 1).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.matches(PLACEHOLDER).count(), MAX_CELLS as usize);
    }

    fn terminal(term: &str, program: &str) -> Terminal {
        Terminal {
            term: term.to_string(),
            program: program.to_string(),
        }
    }

    #[test]
    fn graphics_support_follows_the_outer_terminal_not_the_tmux_term() {
        assert!(terminal("xterm-ghostty", "ghostty 1.3.1").draws_graphics());
        assert!(terminal("xterm-kitty", "kitty(0.32.2)").draws_graphics());
        // Konsole and rio are only ever named by XTVERSION; their $TERM says
        // nothing.
        assert!(terminal("xterm-256color", "Konsole 24.12.0").draws_graphics());
        assert!(terminal("xterm-256color", "Rio 0.5.25").draws_graphics());
        // Whole tokens, so a name is never read out of an unrelated $TERM.
        assert!(!terminal("xterm-256color-rioja", "").draws_graphics());
        // A terminal that stays silent on XTVERSION is still judged on $TERM.
        assert!(terminal("xterm-ghostty", "").draws_graphics());
        assert!(!terminal("alacritty", "").draws_graphics());
        assert!(!terminal("tmux-256color", "").draws_graphics());
        assert!(!terminal("xterm-256color", "XTerm(390)").draws_graphics());
        // wezterm draws kitty images but ignores `U=1`, in every build tested;
        // see `draws_graphics`.
        assert!(!terminal("xterm-256color", "WezTerm 20240203-110809-5046fc22").draws_graphics());
        assert!(!terminal("xterm-256color", "WezTerm 20260815-143815-9c04f79f").draws_graphics());
    }

    #[test]
    fn a_terminal_is_named_by_what_the_user_calls_it() {
        assert_eq!(
            terminal("xterm-256color", "WezTerm 20240203").to_string(),
            "WezTerm 20240203"
        );
        assert_eq!(terminal("alacritty", "").to_string(), "alacritty");
        assert!(Terminal::default().is_unknown());
        assert!(!terminal("alacritty", "").is_unknown());
    }
}
