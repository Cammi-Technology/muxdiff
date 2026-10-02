//! The screen: the palette, running commands, the loop, the frame and the
//! list, so main.rs is only the diff.
//!
//! Colours come from a palette file when `MUXDIFF_PALETTE` names one (a
//! colors.toml in Omarchy's format: `accent = "#7aa2f7"`, `red`, `green`,
//! `background`, `foreground`, `selection`), falling back to the terminal's own.
//! Without one, the background is asked of the terminal, so the added and
//! removed lines are still tinted.

use std::collections::HashMap;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::crossterm::terminal::{EnterAlternateScreen, enable_raw_mode};
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, List, ListItem, ListState, Padding};
use ratatui::Frame;

// ---- The palette -----------------------------------------------------------

static PALETTE: Mutex<Option<(Instant, HashMap<String, Color>)>> = Mutex::new(None);

/// A colour of the palette, or `fallback` when there is none. The file is
/// read again at most once a second, so a theme switch shows while it runs.
pub fn theme(key: &str, fallback: Color) -> Color {
    let mut cached = PALETTE.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let stale = cached.as_ref().is_none_or(|(at, _)| at.elapsed() > Duration::from_secs(1));
    if stale {
        let path = std::env::var("MUXDIFF_PALETTE").unwrap_or_default();
        let colours = std::fs::read_to_string(path).unwrap_or_default().lines()
            .filter_map(|line| {
                let (key, value) = line.split_once('=')?;
                let hex = value.trim().trim_matches('"').trim_start_matches('#');
                let rgb = u32::from_str_radix(hex, 16).ok().filter(|_| hex.len() == 6)?;
                Some((key.trim().to_string(), Color::Rgb((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)))
            })
            .collect();
        *cached = Some((Instant::now(), colours));
    }
    cached.as_ref().and_then(|(_, colours)| colours.get(key).copied()).unwrap_or(fallback)
}

static TERMINAL_BACKGROUND: OnceLock<Option<Color>> = OnceLock::new();

/// The terminal's background colour, asked with OSC 11 (tmux answers for its
/// pane), or None when it doesn't say within a moment. Asked once, before the
/// screen starts, so the answer isn't read as keys.
fn ask_background() -> Option<Color> {
    use std::io::{Read, Write};
    use std::os::fd::AsRawFd;
    let mut tty = std::fs::OpenOptions::new().read(true).write(true).open("/dev/tty").ok()?;
    enable_raw_mode().ok()?;
    let answer = (|| {
        tty.write_all(b"\x1b]11;?\x1b\\").ok()?;
        tty.flush().ok()?;
        let mut answer = vec![];
        let mut byte = [0u8; 1];
        // until the answer ends (BEL or ST), giving up after 100ms of silence
        while !answer.ends_with(b"\x07") && !answer.ends_with(b"\x1b\\") {
            let mut poll = libc::pollfd { fd: tty.as_raw_fd(), events: libc::POLLIN, revents: 0 };
            if unsafe { libc::poll(&mut poll, 1, 100) } <= 0 || tty.read(&mut byte).ok()? == 0 {
                return None;
            }
            answer.push(byte[0]);
        }
        Some(String::from_utf8_lossy(&answer).to_string())
    })();
    ratatui::crossterm::terminal::disable_raw_mode().ok();
    // "\e]11;rgb:1a1a/1b1b/2626\e\\": the first two digits of each are enough
    let rgb = answer?.split("rgb:").nth(1)?.to_string();
    let parts: Vec<u8> = rgb.split('/').take(3).filter_map(|part| u8::from_str_radix(part.get(..2)?, 16).ok()).collect();
    let [r, g, b] = parts[..] else { return None };
    Some(Color::Rgb(r, g, b))
}

/// The background, from the palette or else the terminal: Reset when neither says.
pub fn background() -> Color {
    let terminal = TERMINAL_BACKGROUND.get_or_init(ask_background).unwrap_or(Color::Reset);
    theme("background", terminal)
}

pub fn accent() -> Color {
    theme("accent", Color::Blue)
}
pub fn red() -> Color {
    theme("red", Color::Red)
}
pub fn green() -> Color {
    theme("green", Color::Green)
}

/// `a` with `percent` of `b` mixed in: a tint of the background with a little
/// green, say. None unless both are palette colours, since the terminal's own
/// can't be mixed.
pub fn mix(a: Color, b: Color, percent: u8) -> Option<Color> {
    let (Color::Rgb(r1, g1, b1), Color::Rgb(r2, g2, b2)) = (a, b) else { return None };
    let part = |x: u8, y: u8| ((x as u32 * (100 - percent as u32) + y as u32 * percent as u32) / 100) as u8;
    Some(Color::Rgb(part(r1, r2), part(g1, g2), part(b1, b2)))
}

pub fn bold() -> Style {
    Style::new().add_modifier(Modifier::BOLD)
}
pub fn dim() -> Style {
    Style::new().add_modifier(Modifier::DIM)
}
pub fn colour(c: Color) -> Style {
    Style::new().fg(c)
}

// ---- Running commands ------------------------------------------------------

/// What a command printed, trimmed. Empty when it failed or isn't installed.
pub fn run(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .stderr(Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_string())
        .unwrap_or_default()
}

/// Run a command for what it does. True when it worked.
pub fn ok(program: &str, args: &[&str]) -> bool {
    Command::new(program)
        .args(args)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|status| status.success())
}

// ---- The loop --------------------------------------------------------------

pub enum Flow {
    Go,
    Quit,
}

/// How the app draws itself, what a key does, and what happens every tick.
pub trait App {
    fn draw(&mut self, frame: &mut Frame);
    fn key(&mut self, key: KeyEvent) -> Flow;
    fn tick(&mut self) {}
}

static REDRAW: AtomicBool = AtomicBool::new(false);

/// Draw, wait for a key or for `tick` to pass, repeat until the app quits.
/// Ctrl+C always quits.
pub fn start(app: &mut impl App, tick: Duration) {
    let mut terminal = ratatui::init();
    let mut last_tick = Instant::now();
    loop {
        if REDRAW.swap(false, Ordering::Relaxed) {
            terminal.clear().ok();
        }
        terminal.draw(|frame| app.draw(frame)).ok();

        let wait = tick.saturating_sub(last_tick.elapsed());
        if event::poll(wait).unwrap_or(false) {
            if let Ok(Event::Key(key)) = event::read() {
                let ctrl_c = key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL);
                if key.kind == KeyEventKind::Press {
                    if ctrl_c || matches!(app.key(key), Flow::Quit) {
                        break;
                    }
                }
            }
        }
        if last_tick.elapsed() >= tick {
            app.tick();
            last_tick = Instant::now();
        }
    }
    ratatui::restore();
}

/// Leave the screen for a program that draws on the terminal itself (the
/// editor), then come back.
pub fn leave(what: impl FnOnce()) {
    ratatui::restore();
    what();
    enable_raw_mode().ok();
    ratatui::crossterm::execute!(std::io::stdout(), EnterAlternateScreen).ok();
    REDRAW.store(true, Ordering::Relaxed);
}

// ---- The frame -------------------------------------------------------------

/// Draw the frame and give back the space inside it.
///   title  top left, bold
///   note   after the title, dim
///   alert  along the bottom, before the keys and bold: what the last key did
///   keys   along the bottom: "↑↓ move · Enter choose · q close"
pub fn page(frame: &mut Frame, title: &str, note: &str, alert: &str, keys: &str) -> Rect {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(dim())
        .padding(Padding::new(2, 2, 1, 1))
        .title_top(Line::from(vec![
            Span::styled(format!(" {title} "), bold().fg(accent())),
            Span::styled(if note.is_empty() { String::new() } else { format!("{note} ") }, dim()),
        ]))
        .title_bottom(Line::from(vec![
            Span::styled(if alert.is_empty() { String::new() } else { format!(" {alert} ") }, bold().fg(accent())),
            Span::styled(format!(" {keys} "), dim()),
        ]));
    let inside = block.inner(frame.area());
    frame.render_widget(block, frame.area());
    inside
}

/// Text with escape codes in it (bat's colours) as text ratatui can draw.
pub fn ansi(text: &str) -> ratatui::text::Text<'static> {
    use ansi_to_tui::IntoText;
    text.into_text().unwrap_or_default()
}

// ---- The list --------------------------------------------------------------

/// A list with one row selected. Each row is a Line, so it can carry colours.
pub struct Menu {
    pub rows: Vec<Line<'static>>,
    state: ListState,
}

impl Menu {
    pub fn new(rows: Vec<Line<'static>>) -> Menu {
        let mut menu = Menu { rows: vec![], state: ListState::default() };
        menu.set(rows);
        menu
    }

    /// New rows, keeping the selection where it was.
    pub fn set(&mut self, rows: Vec<Line<'static>>) {
        self.rows = rows;
        let last = self.rows.len().saturating_sub(1);
        self.state.select(Some(self.state.selected().unwrap_or(0).min(last)));
    }

    /// Which row is selected. None when there are no rows.
    pub fn selected(&self) -> Option<usize> {
        self.state.selected().filter(|&at| at < self.rows.len())
    }

    pub fn select(&mut self, row: usize) {
        if row < self.rows.len() {
            self.state.select(Some(row));
        }
    }

    /// Arrows and j/k move. True when the key was used.
    pub fn key(&mut self, key: KeyEvent) -> bool {
        let last = self.rows.len().saturating_sub(1);
        let at = self.state.selected().unwrap_or(0);
        match key.code {
            KeyCode::Up | KeyCode::Char('k') => self.state.select(Some(at.saturating_sub(1))),
            KeyCode::Down | KeyCode::Char('j') => self.state.select(Some((at + 1).min(last))),
            KeyCode::PageUp => self.state.select(Some(at.saturating_sub(10))),
            KeyCode::PageDown => self.state.select(Some((at + 10).min(last))),
            KeyCode::Home => self.state.select(Some(0)),
            KeyCode::End => self.state.select(Some(last)),
            _ => return false,
        }
        true
    }

    pub fn draw(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self.rows.iter().map(|row| ListItem::new(row.clone())).collect();
        let list = List::new(items)
            .highlight_symbol("▌ ")
            .highlight_style(bold().fg(accent()));
        frame.render_stateful_widget(list, area, &mut self.state);
    }
}
