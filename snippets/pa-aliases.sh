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
# zellij: no equivalent built-in (upstream limitation). Best you can
# do is detach + re-attach (`Ctrl+O d` then `zellij attach <name>`)
# from this device — that runs pa's takeover path naturally.
paclaim() {
    if [ -n "$TMUX" ]; then
        tmux detach-client -a
        echo "paclaim: detached other tmux clients; this terminal is now the only one attached."
    elif [ -n "$ZELLIJ" ]; then
        echo "paclaim: zellij has no 'detach others' command." >&2
        echo "  Workaround: detach (Ctrl+O d), then 'zellij attach $ZELLIJ_SESSION_NAME'." >&2
        echo "  That runs the pa takeover path naturally." >&2
        return 1
    else
        echo "paclaim: not inside a tmux or zellij session." >&2
        echo "  From outside a session, use 'pa claim <session>' to takeover-attach." >&2
        return 1
    fi
}
