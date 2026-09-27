#!/bin/sh
# Zed → okra ACP wire wrapper: launches the okra ACP agent while teeing
# BOTH wire directions to timestamped logs, so a real editor drive leaves
# durable evidence of the ACP traffic (initialize / session/new /
# session/prompt / session/update). Used by the G4 real-Zed drive; the
# workspace is the directory Zed is opened on (passed via session/new
# cwd, honored by the agent).
#
#   client → agent : "$OKRA_ACP_WIRE_DIR/client-to-agent.log"
#   agent → client : "$OKRA_ACP_WIRE_DIR/agent-to-client.log"
set -u
WIRE_DIR="${OKRA_ACP_WIRE_DIR:-$HOME/.okra-zed-acp}"
mkdir -p "$WIRE_DIR"
OKRA_BIN="${OKRA_BIN:-/Users/mohsin/zee/okra/target/debug/okra}"
exec 2>>"$WIRE_DIR/agent-stderr.log"
echo "=== wrapper start $(date -u +%FT%TZ) args:$* ===" >>"$WIRE_DIR/agent-stderr.log"
tee "$WIRE_DIR/client-to-agent.log" | "$OKRA_BIN" serve --acp ${OKRA_ACP_CWD:+--cwd "$OKRA_ACP_CWD"} | tee "$WIRE_DIR/agent-to-client.log"
