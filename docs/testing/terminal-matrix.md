# Terminal compatibility matrix (manual test script)

Run this checklist at milestones M3, M4, and M11. TUI streaming apps have well-known
failure modes across terminals and multiplexers; this catches them before users do.

## Terminals / multiplexers to cover

- tmux (per-PR in CI via the PTY harness), Zellij, GNU screen
- SSH session (loopback), Windows Terminal, iTerm2, kitty, Ghostty, Apple Terminal.app

## Checklist per environment

1. **Streaming + scrollback:** run a turn that streams a long markdown answer. After it
   finishes, scroll up with the *terminal's own* scrollback. History must be intact and
   selectable/copyable. (Inline viewport commits finalized cells to native scrollback.)
2. **Alt-screen auto-off under Zellij:** confirm `alternate_screen = auto` disables the
   alternate screen when `ZELLIJ` is set; scrollback still works.
3. **OSC 10/11 theme detection:** no escape-sequence garbage leaks onto the screen or into
   the input buffer (a known tmux failure). Background detection times out gracefully.
4. **Multi-line paste:** paste a 200-line block; it becomes a single paste pill, not 200
   submitted messages. Pasting a `.env`-like block is redacted before send.
5. **Emoji / CJK width:** lines with emoji, ZWJ sequences, and CJK characters do not corrupt
   the gutter or cursor position in the composer.
6. **Resize storm:** rapidly resize the terminal during streaming; no panic, no torn frames
   (synchronized output / DEC mode 2026).
7. **Diff rendering:** a fullscreen diff renders correctly at truecolor, 256-color, and
   16-color (ANSI-16 = foreground-only) terminals.
8. **Hyperlinks:** URL-only lines are not hard-wrapped (terminal hyperlink detection intact).

## Recording results

Note terminal name + version and pass/fail per item. File issues for any failure with the
`terminal-compat` label.
