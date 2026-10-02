# muxdiff

A terminal diff of every changed file in one list, like Zed's multi-buffer diff,
made to sit in a tmux session beside your editor and your AI agent.

```
muxdiff          # the uncommitted changes
muxdiff main     # everything since a branch, tag or commit
```

Each file is a header that opens to show its hunks. Bat colours the syntax, and
added and removed lines are tinted. The diff is read again every few seconds,
and again when the editor closes.

## Keys

Moving and selecting work as they do in [Helix](https://helix-editor.com).

| Key | Does |
|---|---|
| `j` `k` `↑` `↓` | Move |
| `gg` `ge` `G` | First or last line |
| `[` `]` | Previous or next file |
| `x` | Select the line (or the hunk, or the file, from its header). Press it again to take in the next line |
| `v` | Select mode: moving extends the selection |
| `;` `Alt-;` | Collapse the selection, or flip which end the cursor is on |
| `Esc` | Leave select mode, then drop the selection, then close |
| `/` `n` `N` | Search with a regex (case-insensitive unless it has a capital), then go to the next or previous match. Closed files are searched too, and open to show a match |
| `Tab` `Space` | Open or close the file under the cursor |
| `A` | Open or close every file |
| `Enter` | Open the file at that line in `$EDITOR` (`+line path`, so Helix, Neovim and Vim all work) |
| `p` | Open it in the editor already open in this tmux window (Helix, Neovim or Vim), or in a new pane |
| `w` | Open it in a new tmux window |
| `s` | Send the selection (or the line, hunk or file) to the AI agent in this tmux session, with an optional prompt |
| `i` | Comment on the selection in the branch's pull request |
| `S` | Submit your review of the pull request |
| `y` | Copy `path:line`, or `path:first-last` |
| `m` | Switch between the uncommitted changes and the base: the one you gave, or the default branch |
| `c` `C` | Go back one commit, or forward one |
| `R` | Read the diff again |
| `q` | Close |

`s` finds the agent by looking for its process in the session's panes: Claude
Code, Codex, Copilot, Gemini, opencode, Aider, Crush, Cursor, Grok, Qwen, pi, q,
Amp and Goose. It pastes a reference in sidekick.nvim's format
(`@src/main.rs :L12-L15`), after your prompt, and submits it.

## Pull request comments

When the branch has an open pull request, its number shows in the title, and `i`
comments on the selected lines, or on the whole file from its header. Type the
comment (`Alt-Enter` for a new line) and press `Enter`. If you have a review in
progress, the comment goes into it. Otherwise you choose:

- `c` posts it on its own now
- `r` starts a review with it

A review stays pending, visible only to you, until `S` submits it. `S` asks for
an optional summary, then whether to comment, approve or request changes. You
can also submit it on GitHub.

GitHub only takes comments on lines its diff of the pull request shows, all in
one hunk. muxdiff checks the lines and their text against that diff first. A
line you haven't committed and pushed is turned away, rather than posted on
the wrong line. If you're viewing uncommitted changes, `m` switches to the
branch's whole diff.

## Install

```
cargo install --git https://github.com/Cammi-Technology/muxdiff
```

or with mise:

```
mise use -g "cargo:https://github.com/Cammi-Technology/muxdiff@tag:v0.1.0"
```

It needs `git`, and uses these when they are there:

- `bat` for syntax colours
- `gh`, logged in, for pull request comments
- `tmux` for `p`, `w` and `s`
- `wl-copy`, `pbcopy` or `xclip` for `y`

## Colours

By default it uses the terminal's colours, and tints the added and removed
lines from the background the terminal reports. To match a theme, point
`MUXDIFF_PALETTE` at a `colors.toml` in [Omarchy](https://omarchy.org)'s palette
format:

```toml
accent = "#7aa2f7"
background = "#1a1b26"
foreground = "#a9b1d6"
red = "#f7768e"
green = "#9ece6a"
```

With a palette, the added and removed lines get a tinted background mixed from
it. The file is re-read every second, so a theme change shows up while
muxdiff is running.

Inside tmux, the background the terminal reports is the one tmux saw when it
attached. If you switch the terminal from light to dark (or back) after that,
the tints are mixed from the old background and the text gets hard to read. If
you switch themes, set the palette in `tmux.conf` so every pane has it:

```
set-environment -g MUXDIFF_PALETTE "$HOME/path/to/colors.toml"
```

## Licence

MIT
