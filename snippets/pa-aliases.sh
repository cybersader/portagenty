# Short aliases for the most-used pa commands. Saves keystrokes,
# especially on a phone software keyboard over SSH.
#
# After this is installed and your shell is reloaded:
#
#   p        → pa
#   pl       → pa launch
#   pc       → pa claim
#   pls      → pa list
#   pe       → pa export
#   pi       → pa init
#   pad      → pa add
#   paclaim  → inside a tmux/zellij session, kick any other clients
#              so THIS terminal is the only one attached. Useful if
#              you're already in the session (pa claim from inside
#              refuses — nested mpx).
#
# All respect the walk-up workspace discovery, so they work from
# anywhere under your workspace file.

alias p='pa'
alias pl='pa launch'
alias pc='pa claim'
alias pls='pa list'
alias pe='pa export'
alias pi='pa init'
alias pad='pa add'

# `paclaim` — from inside a running session, force THIS terminal to
# be the only client attached. Useful when you SSH'd in from one
# device and now want to lock the session to this device without
# leaving and re-running `pa claim`.
#
# tmux: `detach-client -a` kicks all other clients but keeps you.
#       Fast, clean, works end-to-end.
# zellij: NO EQUIVALENT. Zellij doesn't support per-client
#         disconnection (upstream: no `action disconnect-client`,
#         no `kick-client`, nothing). `pa claim` on zellij doesn't
#         actually takeover either — it just attaches as another
#         shared client. If you need real takeover semantics, the
#         workspace should use tmux (press `m` in the pa session
#         list to switch). paclaim prints this info + the nuclear
#         option (`paclaim --nuclear` kills the session and recreates,
#         losing running state in it).
paclaim() {
    if [ -n "$TMUX" ]; then
        tmux detach-client -a
        echo "paclaim: detached other tmux clients; this terminal is now the only one attached."
        return 0
    fi
    if [ -n "$ZELLIJ" ]; then
        if [ "$1" = "--nuclear" ] || [ "$1" = "--restart" ]; then
            echo "paclaim $1: killing zellij session $ZELLIJ_SESSION_NAME (disconnects all clients, loses running state)."
            zellij kill-session "$ZELLIJ_SESSION_NAME"
            return $?
        fi
        echo "paclaim: zellij has no 'detach others' command (upstream limitation)." >&2
        echo "  Other clients attached to this session will stay attached." >&2
        echo "  Real takeover on zellij = kill + recreate (loses running state):" >&2
        echo "    - From outside:  pa claim --fresh  (cleanest — one command)" >&2
        echo "    - From in here:  paclaim --restart, then re-enter via pa" >&2
        echo "  Or accept shared clients — zellij is built for that." >&2
        echo "  Or switch the workspace to tmux: exit, run pa, press 'm' on the row." >&2
        return 1
    fi
    echo "paclaim: not inside a tmux or zellij session." >&2
    echo "  From outside a session, use 'pa claim <session>' to takeover-attach." >&2
    return 1
}
