# Small footgun-removal + ergonomics pack for shells you reach over
# SSH from Termux (or any similarly-constrained mobile terminal).
# Idempotent — safe to source repeatedly, won't duplicate anything.
#
# What this does:
#
# 1. Disables software flow control (Ctrl+S / Ctrl+Q). These freeze
#    the terminal on some Android on-screen keyboards and their
#    interactions with SSH. If you've ever had a terminal mysteriously
#    stop accepting input, this is the fix.
#
# 2. Makes Ctrl-L (clear) and Ctrl-R (history search) work reliably
#    under less-common terminal emulators by binding them explicitly.
#
# 3. Shortens some common agentic-workflow commands via small
#    helpers you can actually type on a phone.

# --- flow control --------------------------------------------------
# stty ignores errors when stdin isn't a tty (e.g. cron, non-interactive
# script), so this is safe to run from .bashrc unconditionally.
stty -ixon -ixoff 2>/dev/null || true

# --- key bindings (bash readline) ----------------------------------
if [ -n "${BASH_VERSION-}" ]; then
    bind 'Control-l: clear-screen' 2>/dev/null || true
    bind 'Control-r: reverse-search-history' 2>/dev/null || true
fi

# --- agentic shortcuts ---------------------------------------------
# Quick way to drop into whichever pa workspace is nearest. `pa-here`
# prints the resolved workspace without launching anything — useful
# for confirming you're in the right tree before hitting `pa claim`.
pa-here() {
    pa list "$@"
}

# Fast resume-most-recent Claude session (`claude --continue` jumps
# straight into the last session in cwd, which is usually what you
# want when hopping back to a project from a new device).
alias ccc='claude --continue'
alias cccy='claude --continue --dangerously-skip-permissions'
