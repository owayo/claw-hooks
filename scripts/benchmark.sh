#!/usr/bin/env bash
#
# Benchmark script for claw-hooks startup time
#
# Usage:
#   ./scripts/benchmark.sh [iterations]
#
# Requirements:
#   - hyperfine: brew install hyperfine
#   - claw-hooks binary (release or debug)
#

set -euo pipefail

ITERATIONS="${1:-100}"
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
PROJECT_DIR="$(dirname "$SCRIPT_DIR")"

# Create temporary empty config to avoid local PC settings
TEMP_CONFIG=$(mktemp)
cat > "$TEMP_CONFIG" << 'EOF'
# Empty config for benchmarking
rm_block = false
kill_block = false
dd_block = false
debug = false
EOF
trap "rm -f $TEMP_CONFIG" EXIT

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[0;33m'
BLUE='\033[0;34m'
NC='\033[0m'

# Check hyperfine
if ! command -v hyperfine &> /dev/null; then
    echo -e "${RED}Error: hyperfine not found${NC}"
    echo -e "Install: ${GREEN}brew install hyperfine${NC}"
    exit 1
fi

# Find claw-hooks binary
find_binary() {
    local release_bin="$PROJECT_DIR/target/release/claw-hooks"
    local debug_bin="$PROJECT_DIR/target/debug/claw-hooks"

    if [[ -x "$release_bin" ]]; then
        echo "$release_bin"
    elif [[ -x "$debug_bin" ]]; then
        echo -e "${YELLOW}Warning: Using debug binary (slower)${NC}" >&2
        echo "$debug_bin"
    else
        echo -e "${RED}Error: claw-hooks binary not found. Run 'cargo build --release' first.${NC}" >&2
        exit 1
    fi
}

BINARY=$(find_binary)

# Test inputs
INPUT_PRETOOLUSE='{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"git status"},"session_id":"bench"}'
INPUT_PRETOOLUSE_BLOCK='{"hook_event_name":"PreToolUse","tool_name":"Bash","tool_input":{"command":"rm -rf /"},"session_id":"bench"}'
INPUT_POSTTOOLUSE='{"hook_event_name":"PostToolUse","tool_name":"Write","tool_input":{"file_path":"/tmp/test.ts","content":"const x = 1;"},"session_id":"bench"}'
INPUT_STOP='{"hook_event_name":"Stop","stop_hook_active":true,"session_id":"bench"}'

echo -e "${BLUE}========================================${NC}"
echo -e "${BLUE}  claw-hooks Startup Time Benchmark${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""
echo -e "Binary: ${GREEN}$BINARY${NC}"
echo -e "Config: ${GREEN}$TEMP_CONFIG${NC} (empty, no hooks)"
echo -e "Iterations: ${GREEN}$ITERATIONS${NC}"
echo ""

echo -e "${YELLOW}[1/4] PreToolUse (allow)${NC}"
hyperfine \
    --warmup 10 \
    --runs "$ITERATIONS" \
    "echo '$INPUT_PRETOOLUSE' | $BINARY hook --config $TEMP_CONFIG"

echo ""
echo -e "${YELLOW}[2/4] PreToolUse (block)${NC}"
hyperfine \
    --warmup 10 \
    --runs "$ITERATIONS" \
    "echo '$INPUT_PRETOOLUSE_BLOCK' | $BINARY hook --config $TEMP_CONFIG" \
    2>/dev/null || true

echo ""
echo -e "${YELLOW}[3/4] PostToolUse${NC}"
hyperfine \
    --warmup 10 \
    --runs "$ITERATIONS" \
    "echo '$INPUT_POSTTOOLUSE' | $BINARY hook --config $TEMP_CONFIG"

echo ""
echo -e "${YELLOW}[4/4] Stop${NC}"
hyperfine \
    --warmup 10 \
    --runs "$ITERATIONS" \
    "echo '$INPUT_STOP' | $BINARY hook --config $TEMP_CONFIG"

echo ""
echo -e "${BLUE}========================================${NC}"
echo -e "${GREEN}Benchmark complete!${NC}"
echo ""
echo "Target: < 10ms startup time"
