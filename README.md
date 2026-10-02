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

| Key | Does |
|---|---|
| `j` `k` `↑` `↓` | Move |
| `Tab` `Space` | Open or close the file under the cursor |
| `A` | Open or close every file |
| `[` `]` | Previous or next file |
| `g` `G` | First or last line |
| `Enter` | Open the file at that line in `$EDITOR` (`+line path`, so Helix, Neovim and Vim all work) |
| `p` | Open it in the editor already open in this tmux window (Helix, Neovim or Vim), or in a new pane |
| `w` | Open it in a new tmux window |
| `s` | Send the line, hunk or file to the AI agent in this tmux session, with an optional prompt |
| `y` | Copy `path:line` |
| `m` | Switch between the uncommitted changes and the changes since the default branch |
| `c` `C` | Go back one commit, or forward one |
| `R` | Read the diff again |
| `q` `Esc` | Close |

`s` finds the agent by looking for its process in the session's panes: Claude
Code, Codex, Copilot, Gemini, opencode, Aider, Crush, Cursor, Grok, Qwen, pi, q,
Amp and Goose. It pastes a reference in sidekick.nvim's format
(`@src/main.rs :L12-L15`), after your prompt, and submits it.

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
- `tmux` for `p`, `w` and `s`
- `wl-copy`, `pbcopy` or `xclip` for `y`

## Colours

By default it uses the terminal's colours. To match a theme, point
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

## Licence

MIT
