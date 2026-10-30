#!/usr/bin/env bash
# Record (or dry-run) the mqsc-lsp feature demo for docs/demo.cast.
#
#   tools/record_demo.sh            # drive the tour, verify every step, no cast
#   tools/record_demo.sh --record   # same tour, captured with asciinema
#
# The tour runs inside tmux driving real Neovim against the real server, so
# every keystroke round-trips through the LSP. Each step asserts the pane (or
# the saved file) shows what it should before moving on: a green verify run
# means the recorded frames contain the same content at the same moments.
#
# Env:  MQSC_LSP_BIN (server binary)   DEMO_CAST (output .cast path)
#       DEMO_SCALE   (pause multiplier, e.g. 2 while re-timing)
set -euo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="${MQSC_LSP_BIN:-$REPO/server/target/release/mqsc-lsp}"
OUT="${DEMO_CAST:-$REPO/docs/demo.cast}"
DEMO_DIR="$REPO/tools/demo"
W="${DEMO_W:-110}" H="${DEMO_H:-30}"
SESS="mqscdemo-$$"
SCALE="${DEMO_SCALE:-1}"
RECORD=0
[ "${1:-}" = "--record" ] && RECORD=1

[ -x "$BIN" ] || { echo "server binary not found: $BIN (run cargo build --release)"; exit 1; }
command -v tmux >/dev/null || { echo "tmux is required"; exit 1; }
if [ "$RECORD" = 1 ]; then
  command -v asciinema >/dev/null || { echo "asciinema is required for --record"; exit 1; }
  mkdir -p "$(dirname "$OUT")"
fi

WORK="$(mktemp -d)"
cp "$DEMO_DIR/fixture.mqsc" "$WORK/demo.mqsc"
# Stage config and a binary link inside the work dir: whatever gets typed is
# captured in the cast/gif, so the tour must only ever type work-relative
# paths — no home paths, no usernames.
cp "$DEMO_DIR/init.lua" "$WORK/init.lua"
ln -s "$BIN" "$WORK/mqsc-lsp"
cleanup() { tmux kill-session -t "$SESS" 2>/dev/null || true; rm -rf "$WORK"; }
trap cleanup EXIT

P() { sleep "$(awk -v b="$1" -v s="$SCALE" 'BEGIN{printf "%.2f", b*s}')"; }
S() { tmux send-keys -t "$SESS" "$@"; }     # named keys
L() { tmux send-keys -t "$SESS" -l "$1"; } # literal text
wait_pane() { # wait_pane blurb pattern [pattern...]
  local blurb="$1"; shift
  local end=$((SECONDS + 25)) cap miss=""
  while :; do
    cap="$(tmux capture-pane -p -t "$SESS")"
    miss=""
    for pat in "$@"; do grep -qF -- "$pat" <<<"$cap" || miss="$miss [$pat]"; done
    [ -z "$miss" ] && return 0
    [ "$SECONDS" -lt "$end" ] || { echo "FAIL ($blurb): pane never showed:$miss"
      echo "----- pane -----"; echo "$cap"; echo "----------------"; exit 1; }
    sleep 0.2
  done
}
wait_file() { grep -qF -- "$1" "$WORK/demo.mqsc" \
  || { echo "FAIL: saved file lacks '$1'"; exit 1; }; }

tmux -f "$DEMO_DIR/tmux.demo.conf" new-session -d -s "$SESS" -x "$W" -y "$H"

# Neutral prompt before capture starts: the default PS1 carries
# user@host and the shell echoes it into the first frames.
L "PS1='demo$ '; clear"; S Enter; sleep 1

REC_PID=""
if [ "$RECORD" = 1 ]; then
  # `script` gives asciinema a tty whose size we fix, so the cast header and
  # the tmux window agree and nothing reflows mid-recording (attach-session
  # has no -x/-y; the client size comes from this pty and the window already
  # has it from new-session).
  script -qec "stty rows $H cols $W; TERM=xterm-256color asciinema rec --overwrite --idle-time-limit 2.5 \
    -c 'tmux attach -d -t $SESS' '$OUT'" /dev/null &
  REC_PID=$!
  sleep 1
fi

S "cd '$WORK' && MQSC_LSP_BIN=./mqsc-lsp exec nvim -u init.lua -i NONE demo.mqsc" Enter

echo "[1] open file; server attaches and publishes diagnostics"
wait_pane "diagnostics published" "is not a parameter" "not DEFINEd"
P 2.2

echo "[2] quickfix: MAXMSGS -> MAXMSGL via code action"
L "/MAXMSGS"; S Enter; P 0.7
S Space c a
wait_pane "code action picker" "Code actions:" "Change to \`MAXMSGL\`"
P 0.9; S 1 Enter
wait_pane "fixed line" "MAXMSGL(10)"
L ":w"; S Enter; P 0.5; wait_file "MAXMSGL(10)"

echo "[3] completion: new command gets defined names inside TARGET()"
S 1 0 G o
L "ALTER QALIAS(SHOP.FRONT) TARGET(ORDE"; P 0.6
S C-x C-o; P 1.2
S C-y; P 0.4
L ")"; S Escape
wait_pane "completed name" "TARGET(ORDERS.IN)"
L ":w"; S Enter; P 0.5; wait_file "ALTER QALIAS(SHOP.FRONT) TARGET(ORDERS.IN)"

echo "[4] next-word completion: what can legally come next, by command slot"
S G; P 0.3
S O                            # open a line above the dangling continuation
L "DEFINE "; P 1.5
S C-x C-o; P 1.2               # verb committed -> the object-type slot
wait_pane "object types after DEFINE" "AUTHINFO" "BUFFPOOL" "CFSTRUCT"
P 1.0
S C-e                          # dismiss the asserted menu: nvim would commit
L "Q"; P 0.4                   # its highlighted candidate on mismatched typing
S C-x C-o; P 1.2               # base "Q" -> only the Q* object types
wait_pane "filtered to Q* object types" "QMODEL" "QREMOTE" "QUEUE"
S C-n; P 0.5                   # step down from QALIAS to QLOCAL
wait_pane "QLOCAL selected" "DEFINE QLOCAL"
S C-y; P 0.3
L "(ADMIN.DESK) "; P 1.5       # positional name supplied -> the parameter slot
S C-x C-o; P 1.2
wait_pane "parameters offered after the name" "ACCTQ"
S C-e
L "DEFPSIST("; P 1.0            # the group opens the value slot: YES/NO appear
S C-x C-o; P 1.2
wait_pane "enum values of DEFPSIST" "YES"
S C-n; P 0.5                   # step down from NO to YES
wait_pane "YES selected" "DEFPSIST(YES"
S C-y; P 0.3                   # accept inside the group; what follows is only
L ") DESCR('Admin desk alias')"; P 0.5   # non-trigger keys (the nvim-safe path)
S Escape; P 0.5; S Escape; P 0.3          # first closes a popped menu, second ends insert
wait_pane "finished command" "DEFINE QLOCAL(ADMIN.DESK) DEFPSIST(YES) DESCR('Admin desk alias')"
L ":w"; S Enter; P 0.5
wait_file "DEFINE QLOCAL(ADMIN.DESK) DEFPSIST(YES) DESCR('Admin desk alias')"

echo "[5] point out the remaining diagnostics"
L "/NO.SUCH.QUEUE"; S Enter; P 0.7
wait_pane "undefined-object hint" "not DEFINEd"
L "/WHOSE"; S Enter; P 0.5
S g l
wait_pane "name-length warning" "holds at most 48"
P 1.2; S C-w o; P 0.3
S G; P 0.4
S g l
wait_pane "dangling continuation" "never continued"
P 1.4; S C-w o; P 0.4
echo "[6] hover on the queue name: the server knows the definition"
L "/ORDERS.IN"; S Enter; P 0.5
S K
wait_pane "hover float" "this is the definition"
P 1.6
S C-w o; P 0.4

echo "[7] rename the queue at its definition; references follow"
S Space r n; P 0.8
S C-u; P 0.3  # drop the <cword> prefill
L "ORDERS.INPUT"; S Enter
wait_pane "renamed definition" "QLOCAL(ORDERS.INPUT)"
P 1.0
L ":w"; S Enter; P 0.5
[ "$(grep -o "ORDERS\.INPUT" "$WORK/demo.mqsc" | wc -l)" -ge 4 ] \
  || { echo "FAIL: rename did not reach all references"; exit 1; }
wait_file "ALTER QLOCAL(ORDERS.INPUT)"

echo "[8] references: all uses of the queue in a quickfix window"
L "/ORDERS.INPUT"; S Enter; P 0.4
S Space g r
wait_pane "quickfix window" "Quickfix" "ORDERS.INPUT"
P 1.3
L ":cclose"; S Enter; P 0.4

echo "[9] folds from the server (continuation chains, comment blocks)"
S z M
wait_pane "closed folds" "lines"
P 1.1
S z R; P 0.6

echo "[10] final view; quit"
S g g; P 1.2
L "/ORDERS.INPUT"; S Enter; P 1.0
L ":q"; S Enter

if [ "$RECORD" = 1 ]; then
  wait "$REC_PID" || { echo "FAIL: asciinema rec exited non-zero"; exit 1; }
  head -c 200 "$OUT" | head -1 | grep -q "\"width\": *$W" \
    || { echo "WARN: cast header does not match ${W}x${H}; check before uploading"; }
  echo
  echo "cast written: $OUT"
  echo "review it:    asciinema play --include-output a --out-format raw $OUT | less -R"
  echo "  (or just)   asciinema play $OUT"
  echo "then commit and, if publishing: asciinema upload $OUT"
else
  echo
  echo "verify tour completed (add --record to capture $OUT)"
fi
