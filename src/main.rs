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
use std::thread::JoinHandle;
use std::time::Duration;

use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::Frame;
mod ui;
use ui::*;
use regex::Regex;

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

// ---- The pull request ------------------------------------------------------

/// gh, run from the repo so `{owner}/{repo}` and the branch's pull request
/// are this one's: what it printed, or the first line of its complaint.
fn gh(root: &str, args: &[&str]) -> Result<String, String> {
    let out = Command::new("gh").args(args).current_dir(root).stdin(Stdio::null()).output().map_err(|_| "gh is not installed".to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).to_string())
    } else {
        let error = String::from_utf8_lossy(&out.stderr);
        Err(error.lines().find(|line| !line.trim().is_empty()).unwrap_or("gh failed").trim().to_string())
    }
}

struct Pr {
    number: u64,
    id: String,             // GitHub's node id, which its GraphQL API takes
    head: String,           // the commit the pull request is at, which a comment is made on
    review: Option<Review>, // your review of it in progress, if there is one
}

/// A review started but not yet submitted: only its author sees it.
#[derive(Clone)]
struct Review {
    id: String,
    comments: usize,
}

/// GitHub's GraphQL API: `variables` as name=value, the line numbers as
/// numbers and the rest as strings. What `jq` makes of the answer.
fn graphql(root: &str, query: &str, variables: &[(&str, String)], jq: &str) -> Result<String, String> {
    let query = format!("query={query}");
    let fields: Vec<(&str, String)> = variables.iter()
        .map(|(name, value)| (if matches!(*name, "line" | "startLine") { "-F" } else { "-f" }, format!("{name}={value}")))
        .collect();
    let mut args = vec!["api", "graphql", "-f", &query, "--jq", jq];
    for (flag, field) in &fields {
        args.push(flag);
        args.push(field);
    }
    gh(root, &args).map(|out| out.trim().to_string())
}

/// The open pull request for the branch checked out (or the one numbered),
/// with your review in progress.
fn pull_request(root: &str, number: Option<u64>) -> Option<Pr> {
    let mut args = vec!["pr", "view"];
    let number = number.map(|n| n.to_string());
    args.extend(number.as_deref());
    args.extend(["--json", "id,number,headRefOid,state", "--jq", r#"select(.state == "OPEN") | "\(.number) \(.id) \(.headRefOid)""#]);
    let out = gh(root, &args).ok()?;
    let [number, id, head] = out.split_whitespace().collect::<Vec<_>>()[..] else { return None };
    let query = "query($id: ID!) { node(id: $id) { ... on PullRequest { reviews(states: PENDING, first: 1) { nodes { id comments { totalCount } } } } } }";
    let review = graphql(root, query, &[("id", id.to_string())], r#".data.node.reviews.nodes[0] // empty | "\(.id) \(.comments.totalCount)""#).ok()
        .and_then(|out| {
            let (id, comments) = out.split_once(' ')?;
            Some(Review { id: id.to_string(), comments: comments.parse().ok()? })
        });
    Some(Pr { number: number.parse().ok()?, id: id.to_string(), head: head.to_string(), review })
}

/// Which side of the diff a line is on, as GitHub names it: a removed line
/// by its number before, any other by its number now.
fn side(line: &DiffLine) -> (&'static str, usize) {
    if line.kind == '-' { ("LEFT", line.old_number) } else { ("RIGHT", line.number) }
}

/// The hunk of the pull request's diff that has this line, on this side,
/// with the same text: a comment can only go on a line the pull request
/// shows, and only on lines of one hunk.
fn hunk_with(file: &File, line: &DiffLine) -> Option<usize> {
    let (want_side, number) = side(line);
    file.hunks.iter().position(|hunk| hunk.lines.iter().any(|theirs| {
        let found = if want_side == "LEFT" { theirs.kind == '-' && theirs.old_number == number } else { theirs.kind != '-' && theirs.number == number };
        found && theirs.text == line.text
    }))
}

/// Where a comment goes: the file, and the lines (None for the whole file).
struct Comment {
    pr: u64,
    pr_id: String,
    head: String,
    review: Option<Review>, // the review in progress it joins, if there is one
    path: String,
    lines: Option<((&'static str, usize), (&'static str, usize))>, // first and last, by side and number; None: the file
    label: String,                      // "src/main.rs:12-15", to show
}

/// Post a review comment on the pull request.
fn post(root: &str, comment: &Comment, body: &str) -> Result<(), String> {
    let endpoint = format!("repos/{{owner}}/{{repo}}/pulls/{}/comments", comment.pr);
    let mut fields = vec![format!("body={body}"), format!("commit_id={}", comment.head), format!("path={}", comment.path)];
    match comment.lines {
        None => fields.push("subject_type=file".into()),
        Some((start, (side, line))) => {
            fields.push(format!("side={side}"));
            fields.push(format!("line={line}"));
            if start != (side, line) {
                fields.push(format!("start_side={}", start.0));
                fields.push(format!("start_line={}", start.1));
            }
        }
    }
    let mut args = vec!["api", "--method", "POST", &endpoint];
    for field in &fields {
        // numbers as numbers, the rest as strings
        args.push(if field.starts_with("line=") || field.starts_with("start_line=") { "-F" } else { "-f" });
        args.push(field);
    }
    gh(root, &args).map(|_| ())
}

/// Start a review of the pull request, unsubmitted, with nothing in it yet.
fn start_review(root: &str, comment: &Comment) -> Result<Review, String> {
    let query = "mutation($pr: ID!, $head: GitObjectID!) { addPullRequestReview(input: { pullRequestId: $pr, commitOID: $head }) { pullRequestReview { id } } }";
    let id = graphql(root, query, &[("pr", comment.pr_id.clone()), ("head", comment.head.clone())], ".data.addPullRequestReview.pullRequestReview.id")?;
    Ok(Review { id, comments: 0 })
}

/// Add the comment to a review in progress.
fn add_to_review(root: &str, review: &Review, comment: &Comment, body: &str) -> Result<(), String> {
    let mut variables = vec![("review", review.id.clone()), ("path", comment.path.clone()), ("body", body.to_string())];
    match comment.lines {
        None => variables.push(("subject", "FILE".into())),
        Some((start, (side, line))) => {
            variables.extend([("subject", "LINE".into()), ("side", side.into()), ("line", line.to_string())]);
            if start != (side, line) {
                variables.extend([("startSide", start.0.into()), ("startLine", start.1.to_string())]);
            }
        }
    }
    let query = "mutation($review: ID!, $path: String!, $body: String!, $subject: PullRequestReviewThreadSubjectType, $line: Int, $side: DiffSide, $startLine: Int, $startSide: DiffSide) { \
        addPullRequestReviewThread(input: { pullRequestReviewId: $review, path: $path, body: $body, subjectType: $subject, line: $line, side: $side, startLine: $startLine, startSide: $startSide }) { thread { id } } }";
    graphql(root, query, &variables, ".data.addPullRequestReviewThread.thread.id").and_then(|id| {
        if id.is_empty() || id == "null" { Err("GitHub did not take the comment".into()) } else { Ok(()) }
    })
}

/// Submit the review in progress: COMMENT, APPROVE or REQUEST_CHANGES.
fn submit_review(root: &str, review: &Review, verdict: &str, body: &str) -> Result<(), String> {
    let query = "mutation($review: ID!, $event: PullRequestReviewEvent!, $body: String) { submitPullRequestReview(input: { pullRequestReviewId: $review, event: $event, body: $body }) { pullRequestReview { state } } }";
    graphql(root, query, &[("review", review.id.clone()), ("event", verdict.into()), ("body", body.to_string())], ".data.submitPullRequestReview.pullRequestReview.state").map(|_| ())
}

// ---- Searching -------------------------------------------------------------

/// A search as Helix does it: a regex, ignoring case unless it has a capital.
fn search_for(pattern: &str) -> Result<Regex, regex::Error> {
    let smart = if pattern.chars().any(char::is_uppercase) { "" } else { "(?i)" };
    Regex::new(&format!("{smart}{pattern}"))
}

/// Where a row is, in an order that holds whether files are open or not:
/// file, then hunk, then line. A header is 0 within its file, a hunk's "@@"
/// line 0 within its hunk.
type Place = (usize, usize, usize);

// ---- The page, with selection ------------------------------------------------

enum Typing {
    Send(Pane, String),    // s: the AI's pane, the reference
    Comment(Comment),      // i: where the comment goes
    Post(Comment, String), // the comment typed, with no review in progress: post it, or start one?
    Summary,               // S: the review's summary
    Verdict(String),       // and then: comment, approve or request changes?
    Search(Origin),        // /: where it started, to go back to on Esc
}

/// Where the cursor was when / was pressed: a live search moves on from
/// there as each key is typed, and Esc goes back.
struct Origin {
    row: usize,
    anchor: Option<usize>,
    open: Vec<String>,
    search: Option<(String, Regex)>,
}

/// What the keys act on: the selection, or else the row under the cursor.
enum Picked {
    File(usize),
    Lines(usize, Vec<(usize, usize)>), // the file, and (hunk, line) of each line
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
    lines: Vec<Line<'static>>, // the rows as drawn, before the selection is marked on them
    painted: Option<(Option<(usize, usize)>, Option<String>)>, // the selection and search the menu's rows show
    menu: Menu,
    in_tmux: bool,
    note: String,         // a word about the last thing done, after the title
    typing: Option<(Typing, String)>, // a prompt, comment or search being typed
    anchor: Option<usize>, // the selection's other end (the cursor is one end), as Helix keeps it
    extending: bool,      // v: select mode, where moving extends the selection
    g: bool,              // g pressed, waiting for the second key of gg or ge
    search: Option<(String, Regex)>, // what was typed, and the search made of it
    matches_shown: bool,  // the search's matches highlighted (Esc hides them, n and N show them again)
    pr: Option<Pr>,
    looking_for_pr: Option<JoinHandle<Option<Pr>>>,
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
        let background = background();
        // the palette's green and red, or a plain pair to tint the terminal's background with
        let added_tint = mix(background, theme("green", Color::Rgb(0x3f, 0xb9, 0x50)), 18);
        let removed_tint = mix(background, theme("red", Color::Rgb(0xf8, 0x51, 0x49)), 18);
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
        if rows.len() != self.rows.len() {
            // the rows moved under the selection
            self.anchor = None;
            self.extending = false;
        }
        self.rows = rows;
        self.lines = lines;
        self.painted = None;
        self.menu.set(self.lines.clone());
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

    // ---- Selecting, as Helix does --------------------------------------------

    fn file_of(&self, row: usize) -> Option<usize> {
        match *self.rows.get(row)? {
            Row::Header(f) | Row::Hunk(f, _) | Row::Text(f, _, _) => Some(f),
            Row::Note => None,
        }
    }

    /// The selection's first and last rows, when there is one.
    fn span(&self) -> Option<(usize, usize)> {
        let (anchor, cursor) = (self.anchor?, self.menu.selected()?);
        Some((anchor.min(cursor), anchor.max(cursor)))
    }

    /// The file the selection is in: the anchor's, since a selection keeps to
    /// one file, or else the cursor's.
    fn selected_file(&self) -> Option<usize> {
        self.anchor.and_then(|anchor| self.file_of(anchor)).or_else(|| self.file_of(self.menu.selected()?))
    }

    /// What the keys act on: the selected lines, or else the line, hunk or
    /// file under the cursor.
    fn picked(&self) -> Option<Picked> {
        let cursor = self.menu.selected()?;
        if let Some((first, last)) = self.span() {
            let f = self.selected_file()?;
            let lines: Vec<_> = (first..=last).filter_map(|row| match self.rows[row] {
                Row::Text(rf, h, l) if rf == f => Some((h, l)),
                _ => None,
            }).collect();
            return Some(if lines.is_empty() { Picked::File(f) } else { Picked::Lines(f, lines) });
        }
        Some(match self.rows[cursor] {
            Row::Header(f) => Picked::File(f),
            Row::Hunk(f, h) => Picked::Lines(f, (0..self.files[f].hunks[h].lines.len()).map(|l| (h, l)).collect()),
            Row::Text(f, h, l) => Picked::Lines(f, vec![(h, l)]),
            Row::Note => return None,
        })
    }

    /// The picked lines' first and last numbers in the file now (a removed
    /// line's, where it was), or None for a whole file.
    fn picked_range(&self, picked: &Picked) -> Option<(usize, usize)> {
        let Picked::Lines(f, lines) = picked else { return None };
        let all: Vec<&DiffLine> = lines.iter().map(|&(h, l)| &self.files[*f].hunks[h].lines[l]).collect();
        let kept: Vec<usize> = all.iter().filter(|line| line.kind != '-').map(|line| line.number).collect();
        let numbers = if kept.is_empty() { all.iter().map(|line| line.number).collect() } else { kept };
        Some((*numbers.iter().min()?, *numbers.iter().max()?))
    }

    /// Before a move: in select mode the selection grows from where the
    /// cursor was; otherwise it goes.
    fn before_move(&mut self) {
        self.anchor = if self.extending { self.anchor.or(self.menu.selected()) } else { None };
    }

    /// x: select the line, or the hunk or file under the cursor; again, take
    /// in the file's next line too.
    fn select_line(&mut self) {
        let Some(cursor) = self.menu.selected() else { return };
        let Some(f) = self.file_of(cursor) else { return };
        if self.anchor.is_some_and(|anchor| anchor <= cursor && self.file_of(anchor) == Some(f)) {
            let next = (cursor + 1..self.rows.len())
                .take_while(|&row| self.file_of(row) == Some(f))
                .find(|&row| matches!(self.rows[row], Row::Text(..)));
            if let Some(row) = next {
                self.menu.select(row);
            }
            return;
        }
        let end = match self.rows[cursor] {
            Row::Hunk(_, h) => (cursor..self.rows.len()).take_while(|&row| matches!(self.rows[row], Row::Hunk(rf, rh) | Row::Text(rf, rh, _) if rf == f && rh == h)).last(),
            Row::Header(_) => (cursor..self.rows.len()).take_while(|&row| self.file_of(row) == Some(f)).last(),
            _ => None,
        };
        self.anchor = Some(cursor);
        if let Some(row) = end {
            self.menu.select(row);
        }
    }

    // ---- Searching ---------------------------------------------------------

    /// Where a row is, for searching: None for a blank one.
    fn place(&self, row: usize) -> Option<Place> {
        match *self.rows.get(row)? {
            Row::Header(f) => Some((f, 0, 0)),
            Row::Hunk(f, h) => Some((f, h + 1, 0)),
            Row::Text(f, h, l) => Some((f, h + 1, l + 1)),
            Row::Note => None,
        }
    }

    /// n, N: the next (or previous) path or line the search matches, in the
    /// closed files too, which open to show it.
    fn find(&mut self, forward: bool) {
        let Some((pattern, search)) = &self.search else {
            self.note = "nothing searched for yet: /".into();
            return;
        };
        self.matches_shown = true;
        let here = self.menu.selected()
            .and_then(|at| (0..=at).rev().find_map(|row| self.place(row)))
            .unwrap_or((0, 0, 0));
        let mut found: Vec<Place> = vec![];
        for (f, file) in self.files.iter().enumerate() {
            if search.is_match(&file.path) {
                found.push((f, 0, 0));
            }
            for (h, hunk) in file.hunks.iter().enumerate() {
                for (l, line) in hunk.lines.iter().enumerate() {
                    if search.is_match(&line.text) {
                        found.push((f, h + 1, l + 1));
                    }
                }
            }
        }
        let next = if forward {
            found.iter().find(|&&place| place > here).or(found.first())
        } else {
            found.iter().rev().find(|&&place| place < here).or(found.last())
        };
        let Some(&place) = next else {
            self.note = format!("no match for /{pattern}");
            return;
        };
        if (forward && place <= here) || (!forward && place >= here) {
            self.note = "search wrapped".into();
        }
        self.before_move();
        let (f, h, _) = place;
        if h > 0 && !self.open.contains(&self.files[f].path) {
            self.open.push(self.files[f].path.clone());
            self.list();
        }
        if let Some(row) = (0..self.rows.len()).find(|&row| self.place(row) == Some(place)) {
            self.menu.select(row);
        }
    }

    // ---- Sending, copying, commenting ----------------------------------------

    /// What is picked, for the AI: the file, or the lines, as sidekick.nvim
    /// writes a location, with the path as the AI's pane sees it.
    fn reference(&self, from: &Path) -> Option<String> {
        let picked = self.picked()?;
        let f = match &picked { Picked::File(f) | Picked::Lines(f, _) => *f };
        let path = relative(from, &Path::new(&self.root).join(&self.files[f].path)).to_string_lossy().to_string();
        Some(match self.picked_range(&picked) {
            None => format!("@{path}"),
            Some((start, end)) if start == end => format!("@{path} :L{start}"),
            Some((start, end)) => format!("@{path} :L{start}-L{end}"),
        })
    }

    /// What is picked, for people: src/main.rs:12-15
    fn label(&self, picked: &Picked) -> String {
        let f = match picked { Picked::File(f) | Picked::Lines(f, _) => *f };
        let path = self.shown(&self.files[f].path);
        match self.picked_range(picked) {
            None => path,
            Some((start, end)) if start == end => format!("{path}:{start}"),
            Some((start, end)) => format!("{path}:{start}-{end}"),
        }
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
        self.typing = Some((Typing::Send(pane, reference), String::new()));
    }

    fn send(&mut self, pane: Pane, reference: String, prompt: String) {
        let text = if prompt.trim().is_empty() { reference } else { format!("{} {reference}", prompt.trim()) };
        self.note = if send_to_pane(&pane.id, &text) {
            format!("sent to {} in {}", pane.tool, pane.window)
        } else {
            format!("could not send to {}", pane.tool)
        };
    }

    fn yank(&mut self) {
        let Some(picked) = self.picked() else { return };
        let reference = self.label(&picked);
        if !copy(&reference) {
            self.note = "no clipboard: wl-copy, pbcopy or xclip".into();
            return;
        }
        self.note = format!("copied {reference}");
    }

    /// i: check the picked lines are in the pull request's diff, then take
    /// the comment (Enter posts, Esc cancels).
    fn start_comment(&mut self) {
        let Some(number) = self.pr.as_ref().map(|pr| pr.number) else {
            self.note = if self.looking_for_pr.is_some() { "still looking for a pull request".into() } else { "no open pull request for this branch".into() };
            return;
        };
        let Some(picked) = self.picked() else { return };
        match self.comment_on(number, &picked) {
            Ok(comment) => self.typing = Some((Typing::Comment(comment), String::new())),
            Err(error) => self.note = error,
        }
    }

    /// Where a comment on the picked lines goes. GitHub only takes one on
    /// lines the pull request's diff shows, all in one hunk, so they are
    /// checked against it, text and all: a line not yet committed or pushed
    /// is turned away here rather than landing on the wrong line.
    fn comment_on(&self, number: u64, picked: &Picked) -> Result<Comment, String> {
        // the pull request as it is now: it may have moved on since muxdiff started
        let pr = pull_request(&self.root, Some(number)).ok_or(format!("#{number} is closed, or gh can't see it"))?;
        let theirs = parse(&gh(&self.root, &["pr", "diff", &number.to_string(), "--color=never"])?);
        let label = self.label(picked);
        let not_in = format!("{label} is not in #{number}'s diff: {}", self.out_of_step(number, &pr.head));
        let f = match picked { Picked::File(f) | Picked::Lines(f, _) => *f };
        let file = &self.files[f];
        let their_file = theirs.iter().find(|theirs| theirs.path == file.path).ok_or(not_in.clone())?;
        let lines = match picked {
            Picked::File(_) => None,
            Picked::Lines(_, lines) => {
                let line = |&(h, l): &(usize, usize)| &file.hunks[h].lines[l];
                let (Some(first), Some(last)) = (lines.first().map(line), lines.last().map(line)) else { return Err(not_in) };
                let (Some(a), Some(b)) = (hunk_with(their_file, first), hunk_with(their_file, last)) else { return Err(not_in) };
                if a != b {
                    return Err(format!("{label} spans two of #{number}'s hunks: a comment's lines must be in one"));
                }
                Some((side(first), side(last)))
            }
        };
        Ok(Comment { pr: pr.number, pr_id: pr.id, head: pr.head, review: pr.review, path: file.path.clone(), lines, label })
    }

    /// Enter on a comment: into the review in progress, if there is one;
    /// otherwise ask whether to post it now or start a review with it.
    fn comment_typed(&mut self, comment: Comment, body: String) {
        if body.trim().is_empty() {
            self.note = "nothing to post".into();
            return;
        }
        match comment.review.clone() {
            Some(review) => self.review_comment(comment, review, body),
            None => self.typing = Some((Typing::Post(comment, body), String::new())),
        }
    }

    /// c, after a comment: post it on its own.
    /// Why a line may not be in the pull request's diff: this checkout and
    /// the pull request are at different commits, or the line isn't committed.
    fn out_of_step(&self, number: u64, head: &str) -> String {
        let git = |args: &[&str]| run("git", &[&["-C", &self.root][..], args].concat());
        let here = git(&["rev-parse", "HEAD"]);
        if here == head {
            return "commit and push it first".into();
        }
        let commits = |n: String| if n == "1" { "1 commit".to_string() } else { format!("{n} commits") };
        let known = ok("git", &["-C", &self.root, "cat-file", "-e", &format!("{head}^{{commit}}")]);
        if !known || ok("git", &["-C", &self.root, "merge-base", "--is-ancestor", "HEAD", head]) {
            return match known {
                true => format!("this checkout is {} behind #{number}: pull first", commits(git(&["rev-list", "--count", &format!("HEAD..{head}")]))),
                false => format!("#{number} has commits this checkout hasn't fetched: pull first"),
            };
        }
        if ok("git", &["-C", &self.root, "merge-base", "--is-ancestor", head, "HEAD"]) {
            return format!("{} here aren't pushed: push first", commits(git(&["rev-list", "--count", &format!("{head}..HEAD")])));
        }
        format!("this checkout and #{number} have gone different ways: pull or push first")
    }

    fn post_comment(&mut self, comment: Comment, body: String) {
        self.note = match post(&self.root, &comment, body.trim()) {
            Ok(()) => format!("commented on #{} at {}", comment.pr, comment.label),
            Err(error) => format!("could not comment: {error}"),
        };
        self.anchor = None;
        self.extending = false;
    }

    /// r, after a comment: start a review with it.
    fn start_review_with(&mut self, comment: Comment, body: String) {
        match start_review(&self.root, &comment) {
            Ok(review) => self.review_comment(comment, review, body),
            Err(error) => self.note = format!("could not start a review: {error}"),
        }
    }

    fn review_comment(&mut self, comment: Comment, mut review: Review, body: String) {
        if let Err(error) = add_to_review(&self.root, &review, &comment, body.trim()) {
            self.note = format!("could not add to the review: {error}");
            return;
        }
        review.comments += 1;
        self.note = format!("{} in your review of #{} ({} so far) · S submits it", comment.label, comment.pr, review.comments);
        if let Some(pr) = self.pr.as_mut().filter(|pr| pr.number == comment.pr) {
            pr.review = Some(review);
        }
        self.anchor = None;
        self.extending = false;
    }

    /// S: submit the review in progress, after a summary and a verdict.
    fn start_submitting(&mut self) {
        match self.pr.as_ref().map(|pr| pr.review.is_some()) {
            Some(true) => self.typing = Some((Typing::Summary, String::new())),
            Some(false) => self.note = "no review in progress: i, then r, starts one".into(),
            None => self.note = "no open pull request for this branch".into(),
        }
    }

    fn submit(&mut self, verdict: &str, summary: String) {
        let Some(pr) = self.pr.as_mut() else { return };
        let Some(review) = pr.review.clone() else { return };
        self.note = match submit_review(&self.root, &review, verdict, summary.trim()) {
            Ok(()) => {
                pr.review = None;
                let done = match verdict { "APPROVE" => "approved", "REQUEST_CHANGES" => "requested changes on", _ => "reviewed" };
                format!("{done} #{}", pr.number)
            }
            Err(error) => format!("could not submit the review: {error}"),
        };
    }

    /// /: start a live search from here.
    fn start_search(&mut self) {
        let origin = Origin {
            row: self.menu.selected().unwrap_or(0),
            anchor: self.anchor,
            open: self.open.clone(),
            search: self.search.clone(),
        };
        self.typing = Some((Typing::Search(origin), String::new()));
    }

    /// Back to where / was pressed: the cursor, the selection and the files
    /// open then, which the search may have opened more of.
    fn back_to(&mut self, origin: &Origin) {
        if self.open != origin.open {
            self.open = origin.open.clone();
            self.list();
        }
        self.menu.select(origin.row);
        self.anchor = origin.anchor;
    }

    /// Each key typed after /: from where it started, to the first match of
    /// what is typed so far. A regex half typed (an open bracket) keeps the
    /// last whole one's matches.
    fn live_search(&mut self, origin: &Origin, text: &str) {
        self.back_to(origin);
        if text.is_empty() {
            self.search = origin.search.clone();
            self.matches_shown = false;
            return;
        }
        if let Ok(search) = search_for(text) {
            self.search = Some((text.to_string(), search));
            self.find(true);
        }
    }

    /// Enter, after typing.
    fn finish(&mut self, typing: Typing, text: String) {
        match typing {
            Typing::Send(pane, reference) => self.send(pane, reference, text),
            Typing::Comment(comment) => self.comment_typed(comment, text),
            Typing::Summary => self.typing = Some((Typing::Verdict(text), String::new())),
            Typing::Post(..) | Typing::Verdict(_) => {} // a key chooses, not Enter
            Typing::Search(origin) => {
                if text.is_empty() {
                    // an empty search searches again for the last
                    self.search = origin.search;
                    if self.search.is_some() {
                        self.find(true);
                    }
                } else if search_for(&text).is_err() {
                    self.back_to(&origin);
                    self.search = origin.search;
                    self.note = format!("not a regex: {text}");
                }
                // otherwise the live search is already there
            }
        }
    }
}

/// Highlight what the search matches in some of a line's spans (the text,
/// past the gutter), splitting the spans at the matches' edges.
fn highlight_matches(line: &mut Line<'static>, spans: std::ops::Range<usize>, search: &Regex, style: Style) {
    let end = spans.end.min(line.spans.len());
    let start = spans.start.min(end);
    let text: String = line.spans[start..end].iter().map(|span| span.content.as_ref()).collect();
    let found: Vec<(usize, usize)> = search.find_iter(&text).map(|m| (m.start(), m.end())).filter(|(a, b)| a < b).collect();
    if found.is_empty() {
        return;
    }
    let mut out = line.spans[..start].to_vec();
    let mut at = 0;
    for span in &line.spans[start..end] {
        let content = span.content.as_ref();
        let (from, to) = (at, at + content.len());
        let mut cuts = vec![from, to];
        cuts.extend(found.iter().flat_map(|&(a, b)| [a, b]).filter(|&cut| cut > from && cut < to));
        cuts.sort();
        cuts.dedup();
        for pair in cuts.windows(2) {
            let (a, b) = (pair[0], pair[1]);
            let hit = found.iter().any(|&(s, e)| s <= a && b <= e);
            out.push(Span::styled(content[a - from..b - from].to_string(), if hit { span.style.patch(style) } else { span.style }));
        }
        at = to;
    }
    out.extend(line.spans[end..].iter().cloned());
    line.spans = out;
}

/// Show a row as selected: the palette's selection colour behind it, or
/// reversed without one.
fn mark(line: &mut Line<'static>, bg: Option<Color>) {
    match bg {
        Some(bg) => {
            line.style = line.style.bg(bg);
            for span in &mut line.spans {
                span.style.bg = Some(bg);
            }
        }
        None => line.style = line.style.add_modifier(Modifier::REVERSED),
    }
}

impl App for Muxdiff {
    fn draw(&mut self, frame: &mut Frame) {
        if let Some(lookup) = self.looking_for_pr.take_if(|lookup| lookup.is_finished()) {
            self.pr = lookup.join().ok().flatten();
        }
        let span = self.span();
        let shown = self.search.as_ref().filter(|_| self.matches_shown);
        let wanted = (span, shown.map(|(pattern, _)| pattern.clone()));
        if self.painted.as_ref() != Some(&wanted) {
            let mut lines = self.lines.clone();
            if let (Some((first, last)), Some(f)) = (span, self.selected_file()) {
                let bg = Some(theme("selection", Color::Reset)).filter(|bg| *bg != Color::Reset);
                for row in (first..=last).filter(|&row| self.file_of(row) == Some(f)) {
                    mark(&mut lines[row], bg);
                }
            }
            if let Some((_, search)) = shown {
                // the matches as Helix shows them: dark text on yellow
                let text = match background() { Color::Rgb(r, g, b) => Color::Rgb(r, g, b), _ => Color::Black };
                let style = Style::new().bg(theme("yellow", Color::Yellow)).fg(text);
                for (row, line) in lines.iter_mut().enumerate() {
                    match self.rows[row] {
                        Row::Header(_) => highlight_matches(line, 1..2, search, style),
                        Row::Text(..) => highlight_matches(line, 2..usize::MAX, search, style),
                        _ => {}
                    }
                }
            }
            self.menu.set(lines);
            self.painted = Some(wanted);
        }
        let what = match &self.base {
            Some(base) if self.base_title.is_empty() => format!("since {base}"),
            Some(base) => format!("since {base} ({})", self.base_title.chars().take(48).collect::<String>()),
            None => "uncommitted".into(),
        };
        let files = match self.files.len() {
            1 => "1 file".to_string(),
            n => format!("{n} files"),
        };
        let mut note = format!("{what} · {files}");
        if let Some(pr) = &self.pr {
            note.push_str(&format!(" · #{}", pr.number));
            if let Some(review) = &pr.review {
                note.push_str(&format!(" · review in progress, {} {}", review.comments, if review.comments == 1 { "comment" } else { "comments" }));
            }
        }
        if let (Some(_), Some(Picked::Lines(_, lines))) = (self.anchor, self.picked()) {
            note.push_str(&match lines.len() { 1 => " · 1 line".to_string(), n => format!(" · {n} lines") });
        }
        let tmux = if self.in_tmux { " · p w pane, window · s AI" } else { "" };
        let comment = match &self.pr {
            Some(pr) if pr.review.is_some() => " · i comment · S submit",
            Some(_) => " · i comment",
            None => "",
        };
        let keys = match &self.typing {
            Some((Typing::Send(pane, reference), text)) => format!("to {} in {}: › {text}▏ {reference} · Enter send · Esc cancel", pane.tool, pane.window),
            Some((Typing::Comment(comment), text)) => format!("#{} {}: › {}▏ · Enter post · Alt+Enter new line · Esc cancel", comment.pr, comment.label, text.replace('\n', " ⏎ ")),
            Some((Typing::Post(comment, _), _)) => format!("#{} {}: c comment now · r start a review · Esc cancel", comment.pr, comment.label),
            Some((Typing::Summary, text)) => format!("review of #{}: › {}▏ · Enter next · Alt+Enter new line · Esc cancel", self.pr.as_ref().map_or(0, |pr| pr.number), text.replace('\n', " ⏎ ")),
            Some((Typing::Verdict(_), _)) => "submit the review: c comment · a approve · r request changes · Esc cancel".into(),
            Some((Typing::Search(_), text)) => format!("/{text}▏ · Enter keep · Esc go back"),
            None => format!(
                "{}x line · v extend · / search · Tab fold · Enter edit{tmux}{comment} · [ ] file · m {} · c C commit · y copy · q close",
                if self.extending { "SEL · " } else { "" },
                if self.base.is_some() { "uncommitted" } else { &self.branch },
            ),
        };
        let area = page(frame, "muxdiff", &note, &self.note, &keys);
        self.menu.draw(frame, area);
    }

    fn key(&mut self, key: KeyEvent) -> Flow {
        self.note.clear();
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        if let Some((Typing::Post(..) | Typing::Verdict(_), _)) = &self.typing {
            // a choice: one key
            let Some((typing, _)) = self.typing.take() else { return Flow::Go };
            match (typing, key.code) {
                (Typing::Post(comment, body), KeyCode::Char('c')) => self.post_comment(comment, body),
                (Typing::Post(comment, body), KeyCode::Char('r')) => self.start_review_with(comment, body),
                (Typing::Verdict(summary), KeyCode::Char('c')) => self.submit("COMMENT", summary),
                (Typing::Verdict(summary), KeyCode::Char('a')) => self.submit("APPROVE", summary),
                (Typing::Verdict(summary), KeyCode::Char('r')) => self.submit("REQUEST_CHANGES", summary),
                (_, KeyCode::Esc) => {}
                (typing, _) => self.typing = Some((typing, String::new())), // not one of the choices: ask again
            }
            return Flow::Go;
        }
        if let Some((typing, text)) = &mut self.typing {
            match key.code {
                KeyCode::Esc => {
                    if let Some((Typing::Search(origin), _)) = self.typing.take() {
                        self.back_to(&origin);
                        self.search = origin.search;
                        self.matches_shown = false;
                    }
                }
                KeyCode::Enter if alt && matches!(typing, Typing::Comment(_) | Typing::Summary) => text.push('\n'),
                KeyCode::Enter => {
                    if let Some((typing, text)) = self.typing.take() {
                        self.finish(typing, text);
                    }
                }
                KeyCode::Backspace => { text.pop(); }
                KeyCode::Char(c) => text.push(c),
                _ => {}
            }
            if let Some((Typing::Search(origin), text)) = self.typing.take() {
                self.live_search(&origin, &text);
                self.typing = Some((Typing::Search(origin), text));
            }
            return Flow::Go;
        }
        let last = self.rows.len().saturating_sub(1);
        if std::mem::take(&mut self.g) {
            // gg, ge
            match key.code {
                KeyCode::Char('g') => { self.before_move(); self.menu.select(0); }
                KeyCode::Char('e') => { self.before_move(); self.menu.select(last); }
                _ => {}
            }
            return Flow::Go;
        }
        match key.code {
            KeyCode::Esc if self.extending => self.extending = false,
            KeyCode::Esc if self.anchor.is_some() => self.anchor = None,
            KeyCode::Esc if self.matches_shown => self.matches_shown = false,
            KeyCode::Esc | KeyCode::Char('q') => return Flow::Quit,
            KeyCode::Char('x') => self.select_line(),
            KeyCode::Char('v') => self.extending = !self.extending,
            KeyCode::Char(';') if alt => {
                // flip: the cursor goes to the selection's other end
                if let (Some(anchor), Some(cursor)) = (self.anchor, self.menu.selected()) {
                    self.anchor = Some(cursor);
                    self.menu.select(anchor);
                }
            }
            KeyCode::Char(';') => self.anchor = None,
            KeyCode::Char('/') => self.start_search(),
            KeyCode::Char('n') => self.find(true),
            KeyCode::Char('N') => self.find(false),
            KeyCode::Char('i') => self.start_comment(),
            KeyCode::Char('S') => self.start_submitting(),
            KeyCode::Tab | KeyCode::Char(' ') => self.toggle(),
            KeyCode::Char('A') => self.toggle_all(),
            KeyCode::Enter => self.edit(Where::Here),
            KeyCode::Char('p') if self.in_tmux => self.edit(Where::Pane),
            KeyCode::Char('w') if self.in_tmux => self.edit(Where::Window),
            KeyCode::Char(']') => { self.before_move(); self.jump(true); }
            KeyCode::Char('[') => { self.before_move(); self.jump(false); }
            KeyCode::Char('g') => self.g = true,
            KeyCode::Char('G') => { self.before_move(); self.menu.select(last); }
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
            _ => {
                let anchor = self.anchor;
                self.before_move();
                if !self.menu.key(key) {
                    self.anchor = anchor; // not a move after all
                }
            }
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
    background(); // asked of the terminal now, before the screen takes its keys
    let looking_for_pr = {
        let root = root.clone();
        std::thread::spawn(move || pull_request(&root, None))
    };
    let mut app = Muxdiff {
        // m goes back and forth between the uncommitted changes and the base
        // given, or the default branch
        branch: base.clone().unwrap_or_else(|| default_branch(&root)),
        root,
        cwd,
        base,
        base_title: String::new(),
        back: 0,
        highlighted: HashMap::new(),
        files: vec![],
        open: vec![],
        rows: vec![],
        lines: vec![],
        painted: None,
        matches_shown: false,
        menu: Menu::new(vec![]),
        in_tmux: std::env::var_os("TMUX").is_some(),
        note: String::new(),
        typing: None,
        anchor: None,
        extending: false,
        g: false,
        search: None,
        pr: None,
        looking_for_pr: Some(looking_for_pr),
    };
    app.reload();
    start(&mut app, Duration::from_secs(3));
}
