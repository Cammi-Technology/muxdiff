//! muxdiff: the changed files and their diffs in one list, like Zed's
//! multi-buffer diff, for the editor and AI agent in a tmux session. `muxdiff`
//! shows the uncommitted changes, `muxdiff main` the changes since a branch, tag or
//! commit, and c the changes since the commit before (again: one further
//! back; C one forward, up to uncommitted; m returns to uncommitted). Each
//! file is a header that Tab opens and
//! closes; its lines are syntax-coloured by bat (the terminal's colours, so
//! the theme's), with added and removed lines tinted green and red from the
//! palette. Enter opens the file at the line under the cursor in $EDITOR,
//! from the current folder, so the paths shown are relative to it. Inside
//! tmux, p opens the file in the editor already open in this window, if there
//! is one, or in a new pane beside the list; w opens it in a new window; s sends
//! the line, hunk or file to the AI running in this tmux session (claude,
//! codex, opencode …, found by its process in the session's panes) as
//! sidekick.nvim would: an optional prompt, then `@path :L12-L15`, pasted
//! into its pane and submitted. The diff is read again every few seconds
//! and after the editor closes.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent};
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
mod ui;
use ui::*;

// ---- Reading the diff ------------------------------------------------------

struct DiffLine {
    kind: char,         // ' ', '+' or '-'
    text: String,
    number: usize,      // the line's number in the file now (for a removed line, where it was)
    old_number: usize,  // its number in the file before (for an added line, where it went)
}

struct Hunk {
    header: String, // "@@ -10,6 +10,8 @@ fn main()"
    start: usize,   // the first line's number in the file now
    lines: Vec<DiffLine>,
}

struct File {
    path: String,     // from the repo's root, as git prints it
    status: String,   // "", "new", "deleted", "renamed", "binary"
    old_blob: String, // the file before, as git's "index" line names it; zeros for a new file
    new_blob: String, // and now: zeros for a deleted one
    added: usize,
    removed: usize,
    hunks: Vec<Hunk>,
}

/// git's unified diff, file by file.
fn parse(diff: &str) -> Vec<File> {
    let mut files: Vec<File> = vec![];
    let mut number = 0;
    let mut old = 0;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            // "a/old b/new": the new name, which follows " b/"
            let path = rest.rfind(" b/").map(|at| &rest[at + 3..]).unwrap_or(rest).to_string();
            files.push(File { path, status: String::new(), old_blob: String::new(), new_blob: String::new(), added: 0, removed: 0, hunks: vec![] });
            continue;
        }
        let Some(file) = files.last_mut() else { continue };
        if let Some(rest) = line.strip_prefix("@@ ") {
            // "-10,6 +10,8": where the hunk starts in the file before and now
            let at = |side: usize| rest.split_whitespace().nth(side)
                .and_then(|range| range[1..].split(',').next())
                .and_then(|n| n.parse().ok())
                .unwrap_or(1);
            old = at(0);
            number = at(1);
            file.hunks.push(Hunk { header: line.to_string(), start: number, lines: vec![] });
        } else if let Some(rest) = line.strip_prefix("index ") {
            if let Some((before, after)) = rest.split_once("..") {
                file.old_blob = before.to_string();
                file.new_blob = after.split_whitespace().next().unwrap_or_default().to_string();
            }
        } else if line.starts_with("new file mode") {
            file.status = "new".into();
        } else if line.starts_with("deleted file mode") {
            file.status = "deleted".into();
        } else if line.starts_with("rename to ") {
            file.status = "renamed".into();
        } else if line.starts_with("Binary files ") {
            file.status = "binary".into();
        } else if let Some(hunk) = file.hunks.last_mut() {
            let kind = line.chars().next().unwrap_or(' ');
            if !matches!(kind, ' ' | '+' | '-') {
                continue; // "\ No newline at end of file"
            }
            let text = line[1..].replace('\t', "    ");
            hunk.lines.push(DiffLine { kind, text, number: number.max(1), old_number: old.max(1) });
            match kind {
                '+' => { file.added += 1; number += 1; }
                '-' => { file.removed += 1; old += 1; }
                _ => { number += 1; old += 1; }
            }
        }
    }
    files
}

/// The diff from git: the working tree against HEAD, or against `base`,
/// plus the files git doesn't know yet, each as a wholly added file.
fn changes(root: &str, base: Option<&str>) -> Vec<File> {
    let git = |args: &[&str]| {
        let mut all = vec!["-C", root, "-c", "core.quotePath=false", "-c", "diff.noprefix=false"];
        all.extend_from_slice(args);
        run("git", &all)
    };
    let mut files = parse(&git(&["diff", "--no-color", "--no-ext-diff", "-U3", base.unwrap_or("HEAD"), "--"]));
    for path in git(&["ls-files", "--others", "--exclude-standard"]).lines() {
        let full = format!("{root}/{path}");
        let mut file = parse(&git(&["diff", "--no-color", "--no-ext-diff", "-U3", "--no-index", "--", "/dev/null", &full]));
        if let Some(mut found) = file.pop() {
            found.path = path.to_string();
            found.status = "new".into();
            files.push(found);
        }
    }
    files
}

/// What a command printed, untrimmed: a file's lines, blank ones included.
fn output(program: &str, args: &[&str]) -> String {
    Command::new(program).args(args).stderr(Stdio::null()).output()
        .map(|out| String::from_utf8_lossy(&out.stdout).to_string())
        .unwrap_or_default()
}

enum Source<'a> {
    Working,       // the file as it is now, in the working tree
    Blob(&'a str), // the file before, from git's object store
}

/// A file's text syntax-coloured by bat, one Line per line of it: the colours
/// are the terminal's, so the theme's. `name` picks the language. Empty when
/// bat is not installed or the file is gone.
fn highlight(root: &str, name: &str, source: Source) -> Vec<Line<'static>> {
    const BAT: &str = "bat --color=always --style=plain --paging=never --wrap=never --tabs=4";
    let text = match source {
        Source::Working => output("sh", &["-c", &format!("{BAT} -- \"$1\""), "muxdiff", &format!("{root}/{name}")]),
        Source::Blob(hash) => output("sh", &["-c", &format!("git -C \"$1\" cat-file blob \"$2\" | {BAT} --file-name \"$3\" -"), "muxdiff", root, hash, name]),
    };
    ansi(&text).lines
}

/// The branch `m` compares against: origin's default, or main, or master.
fn default_branch(root: &str) -> String {
    let origin = run("git", &["-C", root, "symbolic-ref", "--short", "refs/remotes/origin/HEAD"]);
    if let Some(branch) = origin.strip_prefix("origin/") {
        return branch.to_string();
    }
    if ok("git", &["-C", root, "rev-parse", "--verify", "--quiet", "main"]) { "main".into() } else { "master".into() }
}

/// `to`, written from `from`: ../../x/y
fn relative(from: &Path, to: &Path) -> PathBuf {
    let from: Vec<_> = from.components().collect();
    let to: Vec<_> = to.components().collect();
    let shared = from.iter().zip(&to).take_while(|(a, b)| a == b).count();
    let mut path = PathBuf::new();
    for _ in shared..from.len() {
        path.push("..");
    }
    for part in &to[shared..] {
        path.push(part);
    }
    path
}

// ---- The page --------------------------------------------------------------

#[derive(Clone, Copy)]
enum Row {
    Header(usize),              // a file's line
    Hunk(usize, usize),         // a hunk's "@@" line
    Text(usize, usize, usize),  // a line of a hunk
    Note,                       // "No changes"
}

enum Where {
    Here,
    Pane,
    Window,
}

/// The AI command-line tools sidekick.nvim knows, by the name of their process.
const AI_TOOLS: [&str; 14] = ["claude", "codex", "copilot", "gemini", "opencode", "aider", "crush", "cursor", "grok", "qwen", "pi", "q", "amp", "goose"];

/// Editors, by the name of their process: the ones that can be told to open
/// a file at a line once running.
const EDITORS: [&str; 3] = ["hx", "nvim", "vim"];

struct Pane {
    id: String,     // tmux's %12
    tool: String,
    window: String,
    cwd: PathBuf,   // the pane's folder, which the AI resolves @paths against
    current: bool,  // in the window on screen
}

/// The panes running one of the programs: found by walking each pane's
/// processes, since the pane's own command is a shell or node. `scope` is
/// tmux's: `-s` for the whole session, nothing for this window.
fn panes_running(programs: &[&str], scope: &[&str]) -> Vec<Pane> {
    let mut parents: HashMap<u32, Vec<(u32, String)>> = HashMap::new(); // ppid → (pid, program)
    let mut by_pid: HashMap<u32, String> = HashMap::new();
    for line in run("ps", &["-eo", "pid=,ppid=,args="]).lines() {
        let mut words = line.split_whitespace();
        let (Some(pid), Some(ppid)) = (words.next().and_then(|w| w.parse().ok()), words.next().and_then(|w| w.parse().ok())) else { continue };
        // the program: the command, or what node/bun/python runs
        let args: Vec<&str> = words.take(2).collect();
        let name = |arg: &str| arg.rsplit('/').next().unwrap_or(arg).to_string();
        let program = match args.first().map(|a| name(a)) {
            Some(runner) if ["node", "bun", "python", "python3"].contains(&runner.as_str()) => args.get(1).map(|a| name(a)).unwrap_or(runner),
            Some(program) => program,
            None => continue,
        };
        parents.entry(ppid).or_default().push((pid, program.clone()));
        by_pid.insert(pid, program);
    }
    // the pane's own process, or anything under it
    let tool_under = |pid: u32| -> Option<String> {
        if let Some(program) = by_pid.get(&pid).filter(|program| programs.contains(&program.as_str())) {
            return Some(program.clone());
        }
        let mut queue = vec![pid];
        while let Some(pid) = queue.pop() {
            for (child, program) in parents.get(&pid).into_iter().flatten() {
                if programs.contains(&program.as_str()) {
                    return Some(program.clone());
                }
                queue.push(*child);
            }
        }
        None
    };
    let mut args = vec!["list-panes"];
    args.extend_from_slice(scope);
    args.extend_from_slice(&["-F", "#{pane_id}\t#{pane_pid}\t#{window_name}\t#{window_active}\t#{pane_current_path}"]);
    run("tmux", &args).lines()
        .filter_map(|line| {
            let parts: Vec<&str> = line.split('\t').collect();
            let [id, pid, window, active, cwd] = parts[..] else { return None };
            let tool = tool_under(pid.parse().ok()?)?;
            Some(Pane { id: id.to_string(), tool, window: window.to_string(), cwd: PathBuf::from(cwd), current: active == "1" })
        })
        .collect()
}

/// The panes of this tmux session with an AI tool running in them.
fn ai_panes() -> Vec<Pane> {
    panes_running(&AI_TOOLS, &["-s"])
}

/// The editor already open in this window, if there is one (other than
/// whatever runs the list itself).
fn editor_pane() -> Option<Pane> {
    let own = std::env::var("TMUX_PANE").unwrap_or_default();
    panes_running(&EDITORS, &[]).into_iter().find(|pane| pane.id != own)
}

/// Tell a running editor to open a file at a line: Escape to normal mode
/// first, in case it was typing, and a pause after it, or the editor reads
/// Escape and the colon together as Alt-:.
fn open_in(pane: &Pane, path: &str, line: usize) -> bool {
    let command = match pane.tool.as_str() {
        "hx" => format!(":open {path}:{line}"),
        _ => format!(":edit +{line} {}", path.replace(' ', "\\ ")),
    };
    ok("tmux", &["send-keys", "-t", &pane.id, "Escape"]) && {
        std::thread::sleep(Duration::from_millis(100));
        ok("tmux", &["send-keys", "-t", &pane.id, &command, "Enter"]) && ok("tmux", &["select-pane", "-t", &pane.id])
    }
}

/// Paste text into a tmux pane and submit it, the way sidekick.nvim does.
fn send_to_pane(pane: &str, text: &str) -> bool {
    use std::io::Write;
    let loaded = Command::new("tmux").args(["load-buffer", "-b", "muxdiff", "-"]).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()
        .and_then(|mut child| {
            child.stdin.take().map(|mut stdin| stdin.write_all(text.as_bytes())).transpose()?;
            child.wait()
        })
        .is_ok_and(|status| status.success());
    loaded
        && ok("tmux", &["paste-buffer", "-b", "muxdiff", "-d", "-r", "-t", pane])
        && ok("tmux", &["send-keys", "-t", pane, "Enter"])
}

/// Put text on the clipboard: Wayland's, the Mac's or X's, whichever is there.
fn copy(text: &str) -> bool {
    let command = "wl-copy \"$1\" 2>/dev/null || printf %s \"$1\" | pbcopy 2>/dev/null || printf %s \"$1\" | xclip -selection clipboard";
    ok("sh", &["-c", command, "muxdiff", text])
}

struct Muxdiff {
    root: String,
    cwd: PathBuf,
    base: Option<String>, // None: uncommitted
    base_title: String,   // the base commit, for the title: "77693b3 tui: the desktop's TUIs"
    back: usize,          // how many commits back c has gone
    branch: String,       // what m switches to
    highlighted: HashMap<String, Vec<Line<'static>>>, // a blob's lines, coloured, by "<blob>:<path>"
    files: Vec<File>,
    open: Vec<String>,    // the paths whose diffs are shown
    rows: Vec<Row>,
    menu: Menu,
    in_tmux: bool,
    note: String,         // a word about the last thing done, after the title
    sending: Option<(Pane, String, String)>, // s pressed: the AI pane, the reference, the prompt being typed
}

impl Muxdiff {
    fn shown(&self, path: &str) -> String {
        relative(&self.cwd, &Path::new(&self.root).join(path)).to_string_lossy().to_string()
    }

    fn reload(&mut self) {
        let selected = self.menu.selected().unwrap_or(0);
        self.files = changes(&self.root, self.base.as_deref());
        self.base_title = match &self.base {
            Some(base) => run("git", &["-C", &self.root, "log", "-1", "--format=%h %s", base]),
            None => String::new(),
        };
        self.list();
        self.menu.select(selected.min(self.rows.len().saturating_sub(1)));
    }

    /// The coloured lines of the open files' two sides, made once per blob.
    fn colour_open_files(&mut self) {
        let mut wanted = vec![];
        for file in self.files.iter().filter(|file| self.open.contains(&file.path)) {
            for (blob, working) in [(&file.new_blob, true), (&file.old_blob, false)] {
                if !blob.is_empty() && !blob.chars().all(|c| c == '0') {
                    wanted.push((format!("{blob}:{}", file.path), file.path.clone(), blob.clone(), working));
                }
            }
        }
        for (key, path, blob, working) in wanted {
            if !self.highlighted.contains_key(&key) {
                let source = if working { Source::Working } else { Source::Blob(&blob) };
                let lines = highlight(&self.root, &path, source);
                self.highlighted.insert(key, lines);
            }
        }
    }

    /// The rows from the files: a header each, and the hunks of the open ones.
    fn list(&mut self) {
        self.colour_open_files();
        let background = theme("background", Color::Reset);
        let added_tint = mix(background, green(), 18);
        let removed_tint = mix(background, red(), 18);
        let divider = mix(background, theme("foreground", Color::Reset), 10); // a band behind each hunk's "@@" line
        let mut rows = vec![];
        let mut lines = vec![];
        for (f, file) in self.files.iter().enumerate() {
            let open = self.open.contains(&file.path);
            let mut header = vec![
                Span::styled(if open { "▾ " } else { "▸ " }, dim()),
                Span::styled(self.shown(&file.path), bold()),
                Span::raw("  "),
            ];
            if !file.status.is_empty() {
                header.push(Span::styled(format!("{}  ", file.status), dim()));
            }
            if file.added > 0 {
                header.push(Span::styled(format!("+{} ", file.added), colour(green())));
            }
            if file.removed > 0 {
                header.push(Span::styled(format!("-{}", file.removed), colour(red())));
            }
            rows.push(Row::Header(f));
            lines.push(Line::from(header));
            if !open {
                continue;
            }
            let now = self.highlighted.get(&format!("{}:{}", file.new_blob, file.path));
            let before = self.highlighted.get(&format!("{}:{}", file.old_blob, file.path));
            for (h, hunk) in file.hunks.iter().enumerate() {
                rows.push(Row::Hunk(f, h));
                let band = match divider {
                    Some(bg) => colour(accent()).bg(bg),
                    None => dim(),
                };
                lines.push(Line::styled(format!("        {}", hunk.header), band));
                for (l, line) in hunk.lines.iter().enumerate() {
                    let (gutter, style, tint, coloured) = match line.kind {
                        '+' => (format!("{:>5}", line.number), colour(green()), added_tint, now.and_then(|lines| lines.get(line.number - 1))),
                        '-' => ("     ".to_string(), colour(red()), removed_tint, before.and_then(|lines| lines.get(line.old_number - 1))),
                        _ => (format!("{:>5}", line.number), Style::new(), None, now.and_then(|lines| lines.get(line.number - 1))),
                    };
                    let mut spans = vec![Span::styled(gutter, dim()), Span::styled(format!(" {} ", line.kind), style)];
                    match coloured {
                        // bat resets the background behind its text; the row's tint goes there too
                        Some(syntax) => spans.extend(syntax.spans.iter().cloned().map(|mut span| { span.style.bg = tint; span })),
                        None => spans.push(Span::styled(line.text.clone(), style)),
                    }
                    rows.push(Row::Text(f, h, l));
                    let row = Line::from(spans);
                    lines.push(match tint {
                        Some(bg) => row.style(Style::new().bg(bg)),
                        None => row,
                    });
                }
            }
            rows.push(Row::Note);
            lines.push(Line::raw(""));
        }
        if self.files.is_empty() {
            rows.push(Row::Note);
            lines.push(Line::styled("No changes", dim()));
        }
        self.rows = rows;
        self.menu.set(lines);
    }

    /// The file under the cursor, and the line to open it at.
    fn at(&self) -> Option<(&File, usize)> {
        match *self.rows.get(self.menu.selected()?)? {
            Row::Header(f) => Some((&self.files[f], self.files[f].hunks.first().map_or(1, |hunk| hunk.start))),
            Row::Hunk(f, h) => Some((&self.files[f], self.files[f].hunks[h].start)),
            Row::Text(f, h, l) => Some((&self.files[f], self.files[f].hunks[h].lines[l].number)),
            Row::Note => None,
        }
    }

    fn toggle(&mut self) {
        let Some((file, _)) = self.at() else { return };
        let path = file.path.clone();
        let header = self.rows.iter().position(|row| matches!(row, Row::Header(f) if self.files[*f].path == path));
        match self.open.iter().position(|open| *open == path) {
            Some(at) => { self.open.remove(at); }
            None => self.open.push(path),
        }
        self.list();
        if let Some(row) = header {
            self.menu.select(row);
        }
    }

    fn toggle_all(&mut self) {
        self.open = if self.open.len() < self.files.len() { self.files.iter().map(|file| file.path.clone()).collect() } else { vec![] };
        self.list();
    }

    /// Next (or previous) file's header.
    fn jump(&mut self, forward: bool) {
        let at = self.menu.selected().unwrap_or(0);
        let headers = self.rows.iter().enumerate().filter(|(_, row)| matches!(row, Row::Header(_))).map(|(i, _)| i);
        let target = if forward { headers.filter(|&i| i > at).next() } else { headers.filter(|&i| i < at).last() };
        if let Some(row) = target {
            self.menu.select(row);
        }
    }

    /// Open the file at the line in $EDITOR: here, in place of the list until
    /// it closes, or in a new tmux pane or window beside it.
    fn edit(&mut self, place: Where) {
        let Some((file, line)) = self.at() else { return };
        if file.status == "deleted" {
            self.note = "deleted: nothing to open".into();
            return;
        }
        let path = self.shown(&file.path);
        let editor = std::env::var("EDITOR").ok().filter(|e| !e.is_empty()).unwrap_or_else(|| "hx".into());
        let command = format!("{editor} +{line} '{}'", path.replace('\'', "'\\''"));
        let cwd = self.cwd.to_string_lossy().to_string();
        match place {
            Where::Here => {
                leave(|| { Command::new("sh").args(["-c", &command]).status().ok(); });
                self.reload();
            }
            Where::Pane => match editor_pane() {
                Some(pane) => {
                    let full = Path::new(&self.root).join(&file.path).to_string_lossy().to_string();
                    open_in(&pane, &full, line);
                    self.note = format!("{path}:{line} in {}", pane.tool);
                    return;
                }
                None => { ok("tmux", &["split-window", "-h", "-c", &cwd, &command]); }
            },
            Where::Window => { ok("tmux", &["new-window", "-c", &cwd, &command]); }
        }
        self.note = format!("{path}:{line}");
    }

    /// What the row under the cursor is, for the AI: the file, the hunk's
    /// lines or the one line, as sidekick.nvim writes a location, with the
    /// path as the AI's pane sees it.
    fn reference(&self, from: &Path) -> Option<String> {
        let row = *self.rows.get(self.menu.selected()?)?;
        let (file, lines) = match row {
            Row::Header(f) => (&self.files[f], None),
            Row::Hunk(f, h) => {
                let hunk = &self.files[f].hunks[h];
                let last = hunk.lines.iter().filter(|line| line.kind != '-').map(|line| line.number).max().unwrap_or(hunk.start);
                (&self.files[f], Some((hunk.start, last)))
            }
            Row::Text(f, h, l) => {
                let line = &self.files[f].hunks[h].lines[l];
                (&self.files[f], Some((line.number, line.number)))
            }
            Row::Note => return None,
        };
        let path = relative(from, &Path::new(&self.root).join(&file.path)).to_string_lossy().to_string();
        Some(match lines {
            None => format!("@{path}"),
            Some((start, end)) if start == end => format!("@{path} :L{start}"),
            Some((start, end)) => format!("@{path} :L{start}-L{end}"),
        })
    }

    /// s: find the AI in this tmux session, then take a prompt to send with
    /// the reference (Enter sends, Esc cancels).
    fn start_sending(&mut self) {
        if !self.in_tmux {
            self.note = "not in tmux: no AI to send to".into();
            return;
        }
        let mut panes = ai_panes();
        panes.sort_by_key(|pane| !pane.current); // the window on screen first
        let Some(pane) = panes.into_iter().next() else {
            self.note = "no AI running in this tmux session".into();
            return;
        };
        let Some(reference) = self.reference(&pane.cwd) else { return };
        self.sending = Some((pane, reference, String::new()));
    }

    fn send(&mut self) {
        let Some((pane, reference, prompt)) = self.sending.take() else { return };
        let text = if prompt.trim().is_empty() { reference } else { format!("{} {reference}", prompt.trim()) };
        self.note = if send_to_pane(&pane.id, &text) {
            format!("sent to {} in {}", pane.tool, pane.window)
        } else {
            format!("could not send to {}", pane.tool)
        };
    }

    fn yank(&mut self) {
        let Some((file, line)) = self.at() else { return };
        let reference = format!("{}:{line}", self.shown(&file.path));
        if !copy(&reference) {
            self.note = "no clipboard: wl-copy, pbcopy or xclip".into();
            return;
        }
        self.note = format!("copied {reference}");
    }
}

impl App for Muxdiff {
    fn draw(&mut self, frame: &mut Frame) {
        let what = match &self.base {
            Some(base) if self.base_title.is_empty() => format!("since {base}"),
            Some(base) => format!("since {base} ({})", self.base_title.chars().take(48).collect::<String>()),
            None => "uncommitted".into(),
        };
        let files = match self.files.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        let note = if self.note.is_empty() { format!("{what} · {files}") } else { format!("{what} · {files} · {}", self.note) };
        let tmux = if self.in_tmux { " · p pane · w window · s send to AI" } else { "" };
        let keys = match &self.sending {
            Some((pane, reference, prompt)) => format!("to {} in {}: › {prompt}▏ {reference} · Enter send · Esc cancel", pane.tool, pane.window),
            None => format!("Tab fold · Enter edit{tmux} · [ ] file · m {} · c C a commit back, forward · y copy · q close", if self.base.is_some() { "uncommitted" } else { &self.branch }),
        };
        let area = page(frame, "muxdiff", &note, &keys);
        self.menu.draw(frame, area);
    }

    fn key(&mut self, key: KeyEvent) -> Flow {
        self.note.clear();
        if let Some((_, _, prompt)) = &mut self.sending {
            match key.code {
                KeyCode::Esc => self.sending = None,
                KeyCode::Enter => self.send(),
                KeyCode::Backspace => { prompt.pop(); }
                KeyCode::Char(c) => prompt.push(c),
                _ => {}
            }
            return Flow::Go;
        }
        match key.code {
            _ if closes(key) => return Flow::Quit,
            KeyCode::Tab | KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('A') => self.toggle_all(),
            KeyCode::Enter => self.edit(Where::Here),
            KeyCode::Char('p') if self.in_tmux => self.edit(Where::Pane),
            KeyCode::Char('w') if self.in_tmux => self.edit(Where::Window),
            KeyCode::Char(']') => self.jump(true),
            KeyCode::Char('[') => self.jump(false),
            KeyCode::Char('g') => self.menu.select(0),
            KeyCode::Char('G') => self.menu.select(self.rows.len().saturating_sub(1)),
            KeyCode::Char('y') => self.yank(),
            KeyCode::Char('s') => self.start_sending(),
            KeyCode::Char('R') => self.reload(),
            KeyCode::Char('m') => {
                self.base = if self.base.is_some() { None } else { Some(self.branch.clone()) };
                self.back = 0;
                self.reload();
            }
            KeyCode::Char('c') => {
                let back = self.back + 1;
                if ok("git", &["-C", &self.root, "rev-parse", "--verify", "--quiet", &format!("HEAD~{back}^{{commit}}")]) {
                    self.back = back;
                    self.base = Some(format!("HEAD~{back}"));
                    self.reload();
                } else {
                    self.note = "no earlier commit".into();
                }
            }
            KeyCode::Char('C') => {
                if self.back == 0 {
                    self.note = "already at the working tree".into();
                } else {
                    self.back -= 1;
                    self.base = (self.back > 0).then(|| format!("HEAD~{}", self.back));
                    self.reload();
                }
            }
            _ => { self.menu.key(key); }
        }
        Flow::Go
    }

    fn tick(&mut self) {
        self.reload();
    }
}

fn main() {
    let root = run("git", &["rev-parse", "--show-toplevel"]);
    if root.is_empty() {
        eprintln!("muxdiff: not in a git repository");
        std::process::exit(1);
    }
    let base = std::env::args().nth(1);
    if let Some(base) = &base {
        if !ok("git", &["rev-parse", "--verify", "--quiet", &format!("{base}^{{commit}}")]) {
            eprintln!("muxdiff: no such branch, tag or commit: {base}");
            std::process::exit(1);
        }
    }
    let cwd = std::env::current_dir().expect("the current folder");
    let mut app = Muxdiff {
        branch: default_branch(&root),
        root,
        cwd,
        base,
        base_title: String::new(),
        back: 0,
        highlighted: HashMap::new(),
        files: vec![],
        open: vec![],
        rows: vec![],
        menu: Menu::new(vec![]),
        in_tmux: std::env::var_os("TMUX").is_some(),
        note: String::new(),
        sending: None,
    };
    app.reload();
    start(&mut app, Duration::from_secs(3));
}
