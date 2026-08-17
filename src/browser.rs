//! The browser view: a pane beside the agent showing, live, whatever page the
//! agent is driving through Playwright.
//!
//! corc does not own the browser and deliberately does not want to. Playwright
//! launches Chromium lazily — nothing starts until the agent's first browser
//! tool call — and talks to it over `--remote-debugging-pipe`, which no other
//! process can join. The single thing corc needs is for that Chromium to
//! *also* listen on a TCP port, which one flag in the Playwright MCP config
//! arranges (`corc doctor` prints it). Port `0` lets the kernel choose, so
//! there is no allocation to coordinate and no per-conversation config file:
//! Chromium writes the chosen port into `DevToolsActivePort` in its user data
//! directory, and corc finds both by walking down from the agent pane's pid.
//!
//! From there corc is a pipe. CDP's `Page.startScreencast` delivers PNG frames
//! already base64-encoded, which is exactly the encoding the kitty graphics
//! protocol accepts, and Chromium does the scaling. No image is ever decoded.

use crate::{kitty, state, tmux, ws};
use anyhow::{Context, Result, bail};
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// How often to re-check which conversation is viewed, whether its browser is
/// up, and whether the pane was resized.
const RECHECK: Duration = Duration::from_millis(500);
/// Bounds a blocking frame read so the loop keeps reaching the recheck above.
const FRAME_WAIT: Duration = Duration::from_millis(120);
/// How long to let tmux consume the placeholder cells before asking it to
/// redraw — see `refresh_at` and `tmux::refresh_client`.
const GRID_SETTLE: Duration = Duration::from_millis(200);
/// Rough cell size, only used to pick a capture resolution. The terminal
/// scales the frame into the cell box regardless, so being a little off costs
/// nothing but a few pixels of sharpness.
const CELL_PX: (u32, u32) = (10, 20);
/// Keep a frame reasonable regardless of how wide the pane is.
const MAX_CAPTURE: (u32, u32) = (1600, 1200);

// ---------------------------------------------------------------------------
// Finding the browser
// ---------------------------------------------------------------------------

/// The CDP port of the Chromium running under `root_pid`, if there is one.
///
/// The chain is agent → MCP server → Chromium, at whatever depth, so this
/// walks every descendant. Chromium rewrites its own `/proc/pid/cmdline` into
/// a single space-joined blob, so arguments are matched against the flattened
/// string rather than argv entries. Helper processes are skipped by their
/// `--type=` flag, leaving the browser process itself.
///
/// A descendant whose flag leads nowhere is skipped rather than ending the
/// search: matching a flattened command line means anything *mentioning*
/// `--user-data-dir=` matches, and under an agent pane that includes the
/// agent's own shell commands — a `grep '--user-data-dir='` looking for this
/// very browser would otherwise hide it for as long as the grep ran.
pub fn cdp_port(root_pid: u32) -> Option<u16> {
    descendants(root_pid).into_iter().find_map(|pid| {
        let cmdline = cmdline(pid)?;
        if cmdline.contains("--type=") {
            return None;
        }
        let port_file = user_data_dir(&cmdline)?.join("DevToolsActivePort");
        let contents = std::fs::read_to_string(port_file).ok()?;
        contents.lines().next()?.trim().parse().ok()
    })
}

fn user_data_dir(cmdline: &str) -> Option<PathBuf> {
    let rest = cmdline.split("--user-data-dir=").nth(1)?;
    let end = rest.find(' ').unwrap_or(rest.len());
    Some(PathBuf::from(&rest[..end]))
}

fn cmdline(pid: u32) -> Option<String> {
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).ok()?;
    Some(String::from_utf8_lossy(&raw).replace('\0', " "))
}

/// Every process below `root`, breadth unbounded. One pass over `/proc` builds
/// the child lists; the walk is then pure memory.
fn descendants(root: u32) -> Vec<u32> {
    let mut children: std::collections::HashMap<u32, Vec<u32>> = std::collections::HashMap::new();
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    for entry in entries.flatten() {
        let Ok(pid) = entry.file_name().to_string_lossy().parse::<u32>() else {
            continue;
        };
        if let Some(parent) = parent_pid(pid) {
            children.entry(parent).or_default().push(pid);
        }
    }
    let mut out = Vec::new();
    let mut queue = vec![root];
    while let Some(pid) = queue.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            out.push(*child);
            queue.push(*child);
        }
    }
    out
}

/// The ppid field of `/proc/pid/stat`. The process name sits in parentheses
/// and may itself contain spaces, so fields are counted from the last `)`.
fn parent_pid(pid: u32) -> Option<u32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_name = &stat[stat.rfind(')')? + 1..];
    after_name.split_whitespace().nth(1)?.parse().ok()
}

// ---------------------------------------------------------------------------
// CDP
// ---------------------------------------------------------------------------

/// The page target to mirror: its websocket path and its current url.
/// Extension and service-worker targets are ignored, and a page that has
/// navigated somewhere wins over a blank one.
fn page_target(port: u16) -> Result<(String, String)> {
    let body = http_get(port, "/json/list")?;
    let targets: Vec<serde_json::Value> =
        serde_json::from_str(&body).context("parsing /json/list")?;
    let page = targets
        .iter()
        .filter(|t| t["type"] == "page")
        .max_by_key(|t| u8::from(t["url"].as_str().is_some_and(|u| u != "about:blank")))
        .context("browser has no page target")?;
    let url = page["webSocketDebuggerUrl"]
        .as_str()
        .context("page target has no debugger url")?;
    let path = url
        .split_once("://")
        .and_then(|(_, rest)| rest.find('/').map(|i| rest[i..].to_string()))
        .context("malformed debugger url")?;
    Ok((path, page["url"].as_str().unwrap_or_default().to_string()))
}

/// Minimal GET against the debugging port, used once per connection to find
/// the page target.
///
/// Two things Chromium's DevTools endpoint insists on, both of which it
/// signals by closing the connection with no response at all rather than by
/// returning an error: the request must be HTTP/1.1, and `Host` must carry the
/// port it is actually listening on (its DNS-rebinding guard). `Connection:
/// close` is sent as a courtesy, but Chromium does not always honour it — so
/// the body is read by `Content-Length` rather than by waiting for EOF, which
/// would otherwise just stall until the read timeout.
fn http_get(port: u16, path: &str) -> Result<String> {
    let mut stream = TcpStream::connect(("127.0.0.1", port))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
    )?;

    let mut raw = Vec::new();
    let mut chunk = [0u8; 8192];
    let body_at = loop {
        if let Some(i) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        match stream.read(&mut chunk)? {
            0 => bail!("CDP closed the connection before sending headers"),
            n => raw.extend_from_slice(&chunk[..n]),
        }
    };
    let length: usize = String::from_utf8_lossy(&raw[..body_at])
        .lines()
        .find_map(|line| {
            line.to_ascii_lowercase()
                .strip_prefix("content-length:")?
                .trim()
                .parse()
                .ok()
        })
        .context("CDP response has no content-length")?;
    while raw.len() < body_at + length {
        match stream.read(&mut chunk)? {
            0 => bail!("CDP closed the connection mid-body"),
            n => raw.extend_from_slice(&chunk[..n]),
        }
    }
    Ok(String::from_utf8_lossy(&raw[body_at..body_at + length]).into_owned())
}

/// A live screencast of one page.
struct Screencast {
    socket: ws::WebSocket,
    next_id: u64,
    /// Where the page is now. Seeded from the target list, then kept current
    /// from `Page`'s navigation events, so it follows navigation without
    /// re-querying `/json/list`.
    url: String,
}

impl Screencast {
    fn start(port: u16, capture: (u32, u32)) -> Result<Self> {
        let (path, url) = page_target(port)?;
        let socket = ws::WebSocket::connect("127.0.0.1", port, &path, FRAME_WAIT)?;
        let mut cast = Self {
            socket,
            next_id: 1,
            url,
        };
        cast.call("Page.enable", serde_json::json!({}))?;
        cast.set_capture(capture)?;
        Ok(cast)
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<()> {
        let id = self.next_id;
        self.next_id += 1;
        let message = serde_json::json!({ "id": id, "method": method, "params": params });
        self.socket.send_text(&message.to_string())
    }

    /// (Re)start the cast at a new capture size — what a pane resize needs.
    fn set_capture(&mut self, (width, height): (u32, u32)) -> Result<()> {
        self.call("Page.stopScreencast", serde_json::json!({}))?;
        self.call(
            "Page.startScreencast",
            serde_json::json!({
                "format": "png",
                "maxWidth": width,
                "maxHeight": height,
                "everyNthFrame": 1,
            }),
        )
    }

    /// Next frame as base64 PNG, or `None` if none arrived in this slice.
    /// Frames must be acknowledged or Chromium stops sending after the first.
    ///
    /// Navigation events are picked off the same stream. A screencast frame
    /// carries only geometry — `Page.ScreencastFrameMetadata` has no url — so
    /// the header would otherwise be stuck on whatever `/json/list` said when
    /// the cast was started.
    fn next_frame(&mut self) -> Result<Option<String>> {
        let Some(text) = self.socket.recv()? else {
            return Ok(None);
        };
        let message: serde_json::Value = serde_json::from_str(&text)?;
        let params = &message["params"];
        match message["method"].as_str() {
            // Subframes navigate on their own; only the main frame is the page.
            Some("Page.frameNavigated") => {
                let frame = &params["frame"];
                if frame["parentId"].is_null() {
                    if let Some(url) = frame["url"].as_str() {
                        // `url` is stripped of the fragment, which arrives
                        // beside it with its own `#`.
                        let fragment = frame["urlFragment"].as_str().unwrap_or_default();
                        self.url = format!("{url}{fragment}");
                    }
                }
                return Ok(None);
            }
            // Same-document navigation: history.pushState, a fragment link.
            Some("Page.navigatedWithinDocument") => {
                if let Some(url) = params["url"].as_str() {
                    self.url = url.to_string();
                }
                return Ok(None);
            }
            Some("Page.screencastFrame") => {}
            _ => return Ok(None),
        }
        // Chromium captures the next frame only once the last one is
        // acknowledged, so a missing or misspelled ack does not slow the stream
        // down - it stops it, until some unrelated repaint shakes another frame
        // loose seconds later. The method is `screencastFrameAck`, not
        // `screencastAck`: the latter is answered with an error nobody reads.
        if let Some(session) = params["sessionId"].as_i64() {
            self.call(
                "Page.screencastFrameAck",
                serde_json::json!({ "sessionId": session }),
            )?;
        }
        Ok(params["data"].as_str().map(str::to_string))
    }
}

// ---------------------------------------------------------------------------
// The pane
// ---------------------------------------------------------------------------

/// Timestamped line into `$CORC_BROWSER_TRACE`, for finding out where the delay
/// between a pane appearing and an image appearing actually goes. The pane owns
/// stdout - it is the terminal - so a file is the only place to say anything.
/// Set it in tmux's *session* environment: the pane inherits that, not a shell's.
fn trace(what: &str) {
    let Ok(path) = std::env::var("CORC_BROWSER_TRACE") else {
        return;
    };
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default();
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{at:.3} {what}");
    }
}

/// `corc __browser`: the private entry point tmux runs in the browser pane.
///
/// The pane follows whichever conversation is currently in the content slot
/// rather than being bound to one, so switching conversations in the sidebar
/// switches the browser view with it and no pane has to be respawned.
pub fn run() -> Result<()> {
    let pane = std::env::var("TMUX_PANE").context("corc __browser must run inside tmux")?;
    let mut view = View::new(pane);
    let mut out = std::io::stdout();
    write!(out, "\x1b[?25l")?; // the pane has no cursor of its own
    let result = view.event_loop(&mut out);
    let _ = kitty::delete(&mut out);
    let _ = write!(out, "\x1b[0m\x1b[2J\x1b[H\x1b[?25h");
    result
}

struct View {
    pane: String,
    /// Conversation being mirrored and the pid of its agent pane.
    target: Option<(String, u32)>,
    cast: Option<Screencast>,
    /// Pane geometry the current placeholder grid was painted for.
    grid: Option<(u16, u16)>,
    /// The grid is due but not painted yet, because no frame has arrived for it
    /// to show. A placeholder cell binds to whatever image the id names *at the
    /// moment the cell is written*, so cells written ahead of the image bind to
    /// nothing and stay blank until something happens to rewrite them — which,
    /// in a pane nobody is typing in, can be many seconds later.
    grid_pending: bool,
    /// When to ask tmux for a redraw after painting the grid. Deferred rather
    /// than immediate because tmux reads the pane's output asynchronously: a
    /// redraw asked for in the same breath as the cells can be served before
    /// tmux has read them, which is the very problem it is there to fix.
    refresh_at: Option<Instant>,
    /// Pane width, for truncating the header.
    width: u16,
    header: String,
    last_check: Instant,
}

impl View {
    fn new(pane: String) -> Self {
        Self {
            pane,
            target: None,
            cast: None,
            grid: None,
            grid_pending: false,
            refresh_at: None,
            width: 80,
            header: String::new(),
            last_check: Instant::now() - RECHECK,
        }
    }

    fn event_loop(&mut self, out: &mut impl Write) -> Result<()> {
        trace("pane started");
        loop {
            if self.refresh_at.is_some_and(|at| Instant::now() >= at) {
                self.refresh_at = None;
                tmux::refresh_client();
                trace("asked tmux to redraw");
            }
            if self.last_check.elapsed() >= RECHECK {
                self.last_check = Instant::now();
                self.recheck(out)?;
            }
            match self.cast.as_mut().map(Screencast::next_frame) {
                Some(Ok(Some(frame))) => {
                    trace(&format!("frame {} bytes -> transmit", frame.len()));
                    self.draw_frame(out, &frame)?
                }
                Some(Ok(None)) => {}
                // The browser went away (agent closed it, conversation ended).
                Some(Err(_)) => self.disconnect(out)?,
                None => std::thread::sleep(FRAME_WAIT),
            }
        }
    }

    /// Re-resolve what should be on screen: which conversation is viewed,
    /// whether its browser is reachable, and whether the pane was resized.
    ///
    /// Finding the browser means walking every process on the machine, so it
    /// happens only while disconnected. Once a cast is running the connection
    /// itself is the liveness signal — it errors out the moment the browser
    /// goes away — and this stays a couple of cheap tmux queries.
    fn recheck(&mut self, out: &mut impl Write) -> Result<()> {
        let (cols, rows, terminal) =
            tmux::pane_info(&self.pane).unwrap_or((80, 24, kitty::Terminal::default()));
        self.width = cols;
        // Re-asked every tick rather than settled at startup: the corc session
        // can be running before any client attaches, and an unknown terminal
        // means "no client yet", not "this terminal cannot".
        if !terminal.is_unknown() && !terminal.draws_graphics() {
            return self.draw_header(
                out,
                &format!("{terminal} cannot draw images — needs ghostty, kitty or rio"),
            );
        }

        let agent = viewed_agent();
        if agent.as_ref().map(|(id, _)| id) != self.target.as_ref().map(|(id, _)| id) {
            self.disconnect(out)?;
        }
        self.target = agent;

        let Some((_, pid)) = self.target else {
            return self.draw_header(out, "no conversation in view");
        };
        // One row goes to the header; the rest is the image.
        let size = (cols.max(1), rows.saturating_sub(1).max(1));

        match self.cast.as_mut() {
            None => {
                let Some(port) = cdp_port(pid) else {
                    return self.draw_header(out, "no browser — the agent has not opened one");
                };
                trace(&format!("found browser on port {port}, connecting"));
                match Screencast::start(port, capture_size(size)) {
                    Ok(cast) => {
                        trace("cast started, grid pending until the first frame");
                        self.cast = Some(cast);
                        self.grid = Some(size);
                        self.grid_pending = true;
                    }
                    Err(e) => return self.draw_header(out, &format!("connecting: {e}")),
                }
            }
            Some(cast) if self.grid != Some(size) => {
                // Resized: recapture at the new resolution, and re-lay the
                // cells once a frame at that size has arrived for them to bind
                // to.
                let _ = cast.set_capture(capture_size(size));
                self.grid = Some(size);
                self.grid_pending = true;
            }
            Some(_) => {}
        }
        if let Some(url) = self.cast.as_ref().map(|c| c.url.clone()) {
            self.draw_header(out, &url)?;
        }
        Ok(())
    }

    fn repaint_grid(&self, out: &mut impl Write) -> Result<()> {
        let (cols, rows) = self.grid.unwrap_or((1, 1));
        kitty::paint_grid(out, 1, cols, rows)?;
        Ok(())
    }

    /// The image goes out first and the cells that show it follow, because a
    /// cell binds to the image the id names when the cell is written — see
    /// `grid_pending`. Every later frame replaces the stored image under the
    /// same id, which the already-written cells pick up on their own.
    fn draw_frame(&mut self, out: &mut impl Write, png_base64: &str) -> Result<()> {
        let Some((cols, rows)) = self.grid else {
            return Ok(());
        };
        kitty::transmit(out, png_base64, cols, rows)?;
        if self.grid_pending {
            self.repaint_grid(out)?;
            self.grid_pending = false;
            self.refresh_at = Some(Instant::now() + GRID_SETTLE);
            trace("grid painted, after the first frame");
        }
        Ok(())
    }

    fn draw_header(&mut self, out: &mut impl Write, text: &str) -> Result<()> {
        if self.header == text {
            return Ok(());
        }
        self.header = text.to_string();
        write!(
            out,
            "\x1b[1;1H\x1b[2K\x1b[90m{}\x1b[0m",
            crate::truncate(text, self.width as usize)
        )?;
        out.flush()?;
        Ok(())
    }

    /// Drop the cast and wipe the pane, taking the placeholder cells with it
    /// so no stale frame is left behind.
    fn disconnect(&mut self, out: &mut impl Write) -> Result<()> {
        self.cast = None;
        self.grid = None;
        self.grid_pending = false;
        self.header.clear();
        write!(out, "\x1b[2J\x1b[H")?;
        out.flush()?;
        Ok(())
    }
}

/// Capture resolution for a cell box, clamped so a wide pane does not make
/// every frame enormous.
fn capture_size((cols, rows): (u16, u16)) -> (u32, u32) {
    (
        (cols as u32 * CELL_PX.0).min(MAX_CAPTURE.0),
        (rows as u32 * CELL_PX.1).min(MAX_CAPTURE.1),
    )
}

/// The conversation in the content slot and the pid of its agent pane — the
/// root of the process tree the browser hides in. `None` when nothing is
/// viewed or the conversation is dead.
fn viewed_agent() -> Option<(String, u32)> {
    let state = state::State::load().ok()?;
    let conv = crate::viewed_conversation(&state)?;
    let pid = tmux::pane_pid(conv.pane_id.as_deref()?)?;
    Some((conv.id.clone(), pid))
}

// ---------------------------------------------------------------------------
// Turning the view on and off
// ---------------------------------------------------------------------------

/// What a request asks of a conversation's browser flag.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Request {
    On,
    Off,
}

impl Request {
    fn word(self) -> &'static str {
        match self {
            Self::On => "on",
            Self::Off => "off",
        }
    }

    fn parse(word: &str) -> Option<Self> {
        match word {
            "on" => Some(Self::On),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    fn wants_view(self) -> bool {
        self == Self::On
    }
}

/// The mailbox `corc browser` drops requests into for the TUI to pick up.
///
/// The TUI owns `state.json` and rewrites it wholesale from its in-memory
/// copy, so a second process editing a conversation's flag there would simply
/// be overwritten on the next save — the toggle would silently do nothing, and
/// a save lands whenever a turn starts or ends, which is exactly when this
/// command gets run. A mailbox keeps the single-writer rule intact: the CLI
/// only ever appends here, and the TUI applies and clears the requests on its
/// next refresh, within a second.
fn mailbox() -> Result<PathBuf> {
    let state = state::state_file()?;
    let dir = state.parent().context("state file has no parent dir")?;
    Ok(dir.join("browser-requests"))
}

/// Append one request. Appending rather than overwriting means two toggles in
/// quick succession both land.
fn append_request(path: &std::path::Path, id: &str, request: Request) -> Result<()> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("opening {}", path.display()))?;
    writeln!(file, "{id}\t{}", request.word())
        .with_context(|| format!("writing {}", path.display()))
}

/// Apply every pending request to `state` and empty the mailbox, returning
/// whether anything changed — the TUI's half of the handover, run once per
/// refresh. Best-effort throughout: an unreadable file, a malformed line or a
/// request naming a conversation that no longer exists is dropped, since a
/// lost toggle costs one keystroke while a stuck one would fight the user on
/// every tick.
pub fn apply_requests(state: &mut state::State) -> bool {
    let Ok(path) = mailbox() else {
        return false;
    };
    apply_requests_at(&path, state)
}

fn apply_requests_at(path: &std::path::Path, state: &mut state::State) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let _ = std::fs::remove_file(path);
    let mut changed = false;
    for line in text.lines() {
        let Some((id, word)) = line.split_once('\t') else {
            continue;
        };
        let Some(request) = Request::parse(word) else {
            continue;
        };
        if let Some(conv) = state.conversation_mut(id)
            && conv.browser != request.wants_view()
        {
            conv.browser = request.wants_view();
            changed = true;
        }
    }
    changed
}

/// The last request already queued for `id` — what the TUI will have applied
/// by the time a new one lands. Without it, two toggles inside the same
/// refresh tick would both read the same stale flag out of `state.json` and
/// the second would ask for what the first already asked for.
fn pending(path: &std::path::Path, id: &str) -> Option<Request> {
    let text = std::fs::read_to_string(path).ok()?;
    text.lines().rev().find_map(|line| {
        let (line_id, word) = line.split_once('\t')?;
        (line_id == id).then_some(())?;
        Request::parse(word)
    })
}

/// `corc browser [on|off]`: turn the browser view on or off for the
/// conversation this command runs in — the point being that it can be reached
/// from inside the agent pane (`!corc browser` in Claude Code) without going
/// to the sidebar. With no argument it toggles.
pub fn command(word: Option<&str>) -> Result<()> {
    let state = state::State::load()?;
    let conv = current_conversation(&state)
        .context("no conversation to toggle — run this from inside an agent pane")?;
    let mailbox = mailbox()?;
    // Resolved here rather than sent as a "toggle", so what this prints is
    // what the TUI will do.
    let current = pending(&mailbox, &conv.id)
        .map(Request::wants_view)
        .unwrap_or(conv.browser);
    let request = match word {
        None if current => Request::Off,
        None => Request::On,
        Some(word) => Request::parse(word).context("usage: corc browser [on|off]")?,
    };
    if request.wants_view() {
        check_ready()?;
    }
    append_request(&mailbox, &conv.id, request)?;
    println!("browser view: {}", request.word());
    Ok(())
}

/// `corc __browser-toggle`: what `Ctrl+b` runs. The same toggle, except that
/// it reports a failure on tmux's message line and still exits 0 — tmux
/// throws away a `run-shell` command's stderr, so an error left to the exit
/// code reaches the user as nothing but `returned 1`.
pub fn key_toggle() -> Result<()> {
    if let Err(e) = command(None) {
        tmux::show_message(&format!("corc: {e:#}"));
    }
    Ok(())
}

/// The conversation this command belongs to: the one whose agent pane it runs
/// inside — `TMUX_PANE` is inherited all the way down into the agent's own
/// tool calls — falling back to whichever conversation is in view, which is
/// what running it from the sidebar or a plain shell should mean.
fn current_conversation(state: &state::State) -> Option<&state::Conversation> {
    std::env::var("TMUX_PANE")
        .ok()
        .and_then(|pane| {
            state
                .conversations
                .iter()
                .find(|c| c.pane_id.as_deref() == Some(pane.as_str()))
        })
        .or_else(|| crate::viewed_conversation(state))
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// The Playwright MCP config corc needs in place. Everything else about the
/// user's Playwright setup is left alone — this only adds the debugging port.
pub const PLAYWRIGHT_CONFIG: &str = "{\n  \"browser\": {\n    \"launchOptions\": {\n      \"args\": [\"--remote-debugging-port=0\"]\n    }\n  }\n}\n";

pub fn config_path() -> Result<PathBuf> {
    let base = match std::env::var("XDG_CONFIG_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => PathBuf::from(std::env::var("HOME").context("HOME not set")?).join(".config"),
    };
    Ok(base.join("corc/playwright.json"))
}

/// Write the config file if it is missing, returning its path. Never
/// overwrites: the file is the user's once it exists.
pub fn ensure_config() -> Result<PathBuf> {
    let path = config_path()?;
    if !path.exists() {
        let dir = path.parent().context("config path has no parent")?;
        std::fs::create_dir_all(dir)?;
        std::fs::write(&path, PLAYWRIGHT_CONFIG)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(path)
}

/// Whether a Playwright MCP server is configured to load corc's config file.
/// Read from Claude Code's config, the only provider whose MCP setup corc can
/// currently inspect; an unreadable or absent config just reads as "not wired".
pub fn config_is_wired() -> bool {
    let Ok(path) = config_path() else {
        return false;
    };
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let Ok(raw) = std::fs::read_to_string(format!("{home}/.claude.json")) else {
        return false;
    };
    raw.contains(&path.to_string_lossy().to_string())
}

/// Fail early with an actionable message rather than opening a pane that can
/// never show anything.
pub fn check_ready() -> Result<()> {
    if !config_is_wired() {
        let path = ensure_config()?;
        bail!(
            "Playwright is not exposing a debugging port. Add `--config {}` \
             to the playwright MCP server args, then restart the agent.",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_user_data_dir_is_extracted_from_chromiums_flattened_cmdline() {
        // Chromium rewrites its cmdline into one blob, so the flag sits in the
        // middle of a long space-separated string.
        let cmdline = "/usr/lib/chromium/chromium --headless --no-sandbox \
                       --remote-debugging-port=0 \
                       --user-data-dir=/home/h/.cache/ms-playwright-mcp/mcp-chrome-abc \
                       --remote-debugging-pipe --noerrdialogs";
        assert_eq!(
            user_data_dir(cmdline),
            Some(PathBuf::from(
                "/home/h/.cache/ms-playwright-mcp/mcp-chrome-abc"
            ))
        );
        // A trailing flag value at the very end has no space to stop at.
        assert_eq!(
            user_data_dir("chromium --user-data-dir=/tmp/p"),
            Some(PathBuf::from("/tmp/p"))
        );
        assert_eq!(user_data_dir("chromium --headless"), None);
    }

    #[test]
    fn the_current_process_is_its_own_ancestors_descendant() {
        let me = std::process::id();
        let parent = parent_pid(me).expect("own ppid is readable");
        assert!(descendants(parent).contains(&me));
    }

    #[test]
    fn capture_resolution_tracks_the_pane_but_stays_bounded() {
        assert_eq!(capture_size((80, 24)), (800, 480));
        assert_eq!(capture_size((400, 200)), MAX_CAPTURE);
    }

    /// The whole path, against a real browser: launch the Playwright MCP with
    /// corc's config, make the agent's first tool call, then find Chromium the
    /// way the browser pane does — down the process tree, via
    /// `DevToolsActivePort` — and pull a frame through corc's own websocket
    /// and CDP code.
    ///
    /// Ignored by default because it needs `npx`, Chromium and the network.
    /// Run it with `cargo test -- --ignored --nocapture` after touching
    /// anything in this file, `ws.rs` or `kitty.rs`.
    #[test]
    #[ignore = "needs npx, chromium and network"]
    fn a_real_playwright_browser_is_discovered_and_streamed() {
        use std::io::{BufRead, BufReader};
        use std::process::{Command, Stdio};

        let config = ensure_config().expect("writing the config");
        // `--isolated` gives this run its own profile. Playwright refuses to
        // open a second browser against a profile already in use, so without
        // it the test collides with any live agent's browser — and, more to
        // the point, so do two conversations (see README).
        let mut mcp = Command::new("npx")
            .args(["-y", "@playwright/mcp@latest", "--headless", "--isolated"])
            .arg("--config")
            .arg(&config)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("starting the playwright MCP server");
        let mut stdin = mcp.stdin.take().unwrap();
        let mut stdout = BufReader::new(mcp.stdout.take().unwrap());

        let mut request = |stdin: &mut std::process::ChildStdin, line: &str| {
            writeln!(stdin, "{line}").unwrap();
            let mut response = String::new();
            stdout.read_line(&mut response).unwrap();
            response
        };
        request(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"corc","version":"1"}}}"#,
        );
        writeln!(
            stdin,
            r#"{{"jsonrpc":"2.0","method":"notifications/initialized"}}"#
        )
        .unwrap();
        // Nothing has launched yet — this is the lazy start corc relies on.
        assert!(
            cdp_port(mcp.id()).is_none(),
            "chromium should not exist before the first browser tool call"
        );

        let navigate = request(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com"}}}"#,
        );
        assert!(
            !navigate.contains("\"isError\":true"),
            "navigate: {navigate}"
        );

        let port = wait_for(|| cdp_port(mcp.id())).expect("chromium's CDP port");
        let mut cast = Screencast::start(port, (800, 600)).expect("starting the screencast");
        assert!(cast.url.contains("example.com"), "url was {}", cast.url);

        let frame = wait_for(|| cast.next_frame().ok().flatten()).expect("a screencast frame");
        // PNG magic, base64-encoded — exactly what kitty wants, undecoded.
        assert!(frame.starts_with("iVBORw0KGgo"), "frame was not a PNG");

        let mut emitted = Vec::new();
        crate::kitty::transmit(&mut emitted, &frame, 60, 30).unwrap();
        let text = String::from_utf8(emitted).unwrap();
        assert!(
            text.starts_with("\x1bPtmux;"),
            "frame was not wrapped for tmux"
        );
        assert!(text.contains("U=1,c=60,r=30"));

        // The header has to follow the agent around. A screencast frame says
        // nothing about where the page is, so this rides on the navigation
        // events instead — the part that is easy to get wrong and impossible
        // to notice, since a stale url still looks like a url.
        let navigate = request(
            &mut stdin,
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"browser_navigate","arguments":{"url":"https://example.com/#history"}}}"#,
        );
        assert!(
            !navigate.contains("\"isError\":true"),
            "navigate: {navigate}"
        );
        wait_for(|| {
            let _ = cast.next_frame();
            cast.url.ends_with("#history").then_some(())
        })
        .unwrap_or_else(|| panic!("url did not follow the navigation, still {}", cast.url));

        let _ = mcp.kill();
        let _ = mcp.wait();
    }

    /// Poll `f` for up to five seconds. Browser startup and the first paint
    /// are both asynchronous, so every step here is a wait rather than a
    /// single try.
    #[cfg(test)]
    fn wait_for<T>(mut f: impl FnMut() -> Option<T>) -> Option<T> {
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if let Some(value) = f() {
                return Some(value);
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        None
    }

    /// The whole handover between the two processes: `corc browser`, run in an
    /// agent pane, appends requests; the TUI applies them to its own state on
    /// its next refresh and leaves the mailbox empty. Nothing here touches
    /// state.json, which is the point — the TUI stays its only writer.
    #[test]
    fn requests_from_the_agent_pane_reach_the_tuis_state_and_are_consumed() {
        let path = std::env::temp_dir().join(format!("corc-mailbox-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut state = state::State::default();
        state.add_conversation("one".into(), "/tmp".into(), "%1".into(), "claude".into());
        state.add_conversation("two".into(), "/tmp".into(), "%2".into(), "claude".into());

        // Nothing pending: a refresh must not report a change or it would
        // save state.json every second.
        assert!(!apply_requests_at(&path, &mut state));

        // Two toggles in quick succession, plus noise no reader should choke
        // on: a malformed line and a conversation that has since gone away.
        append_request(&path, "one", Request::On).unwrap();
        append_request(&path, "two", Request::On).unwrap();
        std::fs::write(
            &path,
            std::fs::read_to_string(&path).unwrap() + "garbage\nghost\ton\n",
        )
        .unwrap();

        assert!(apply_requests_at(&path, &mut state));
        assert!(state.conversation("one").unwrap().browser);
        assert!(state.conversation("two").unwrap().browser);
        // Consumed: the mailbox is gone and a second pass is a no-op.
        assert!(!path.exists());
        assert!(!apply_requests_at(&path, &mut state));

        // Turning one off leaves the other alone, and a request that asks for
        // what is already true is not a change worth saving for.
        append_request(&path, "one", Request::Off).unwrap();
        append_request(&path, "two", Request::On).unwrap();
        assert!(apply_requests_at(&path, &mut state));
        assert!(!state.conversation("one").unwrap().browser);
        assert!(state.conversation("two").unwrap().browser);

        append_request(&path, "two", Request::On).unwrap();
        assert!(!apply_requests_at(&path, &mut state));
        let _ = std::fs::remove_file(&path);

        // A second toggle before the TUI has drained the first reads what is
        // queued, not the flag still sitting in state.json.
        assert_eq!(pending(&path, "one"), None);
        append_request(&path, "one", Request::On).unwrap();
        assert_eq!(pending(&path, "one"), Some(Request::On));
        append_request(&path, "one", Request::Off).unwrap();
        assert_eq!(pending(&path, "one"), Some(Request::Off));
        assert_eq!(pending(&path, "two"), None);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn the_shipped_config_only_adds_the_debugging_port() {
        let parsed: serde_json::Value = serde_json::from_str(PLAYWRIGHT_CONFIG).unwrap();
        assert_eq!(
            parsed["browser"]["launchOptions"]["args"],
            serde_json::json!(["--remote-debugging-port=0"])
        );
        // Port 0 is the whole point: the kernel picks, so two conversations
        // never contend for one port.
        assert_eq!(parsed["browser"].as_object().unwrap().len(), 1);
    }
}
