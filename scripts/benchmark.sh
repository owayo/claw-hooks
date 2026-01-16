#!/usr/bin/env bash
#
# Benchmark script for claw-hooks startup time and memory usage
#
# Usage:
#   ./scripts/benchmark.sh [iterations]
#   ./scripts/benchmark.sh --memory-only
#
# Requirements:
#   - hyperfine: brew install hyperfine (for timing)
#   - /usr/bin/time (built-in on macOS/Linux)
#   - claw-hooks binary (release or debug)
#

set -euo pipefail

ITERATIONS="${1:-100}"
MEMORY_ONLY=false

if [[ "${1:-}" == "--memory-only" ]]; then
    MEMORY_ONLY=true
fi
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

# Check hyperfine (only required for timing benchmarks)
check_hyperfine() {
    if ! command -v hyperfine &> /dev/null; then
        echo -e "${RED}Error: hyperfine not found${NC}"
        echo -e "Install: ${GREEN}brew install hyperfine${NC}"
        exit 1
    fi
}

# Detect platform and set time command flags
detect_platform() {
    if [[ "$(uname)" == "Darwin" ]]; then
        # macOS: use -l for memory stats
        TIME_CMD="/usr/bin/time -l"
        MEMORY_FIELD="maximum resident set size"
    else
        # Linux: use -v for memory stats
        TIME_CMD="/usr/bin/time -v"
        MEMORY_FIELD="Maximum resident set size"
    fi
}

# Measure memory usage (returns peak RSS in KB)
measure_memory() {
    local input="$1"
    local description="$2"
    local temp_output
    temp_output=$(mktemp)

    # Run with /usr/bin/time and capture stderr
    echo "$input" | $TIME_CMD "$BINARY" hook --config "$TEMP_CONFIG" 2>"$temp_output" >/dev/null || true

    # Extract memory from output
    local memory_bytes
    if [[ "$(uname)" == "Darwin" ]]; then
        # macOS: "maximum resident set size" is in bytes
        memory_bytes=$(grep "$MEMORY_FIELD" "$temp_output" | awk '{print $1}')
        local memory_kb=$((memory_bytes / 1024))
        local memory_mb=$(echo "scale=2; $memory_bytes / 1024 / 1024" | bc)
    else
        # Linux: "Maximum resident set size" is in KB
        memory_bytes=$(grep "$MEMORY_FIELD" "$temp_output" | awk '{print $NF}')
        local memory_kb=$memory_bytes
        local memory_mb=$(echo "scale=2; $memory_bytes / 1024" | bc)
    fi

    rm -f "$temp_output"
    printf "  %-25s %s KB (%s MB)\n" "$description:" "$memory_kb" "$memory_mb"
}

detect_platform

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
echo -e "${BLUE}  claw-hooks Benchmark${NC}"
echo -e "${BLUE}========================================${NC}"
echo ""
echo -e "Binary: ${GREEN}$BINARY${NC}"
echo -e "Config: ${GREEN}$TEMP_CONFIG${NC} (empty, no hooks)"
if [[ "$MEMORY_ONLY" == "false" ]]; then
    echo -e "Iterations: ${GREEN}$ITERATIONS${NC}"
fi
echo ""

# Memory benchmark
echo -e "${BLUE}--- Memory Usage (Peak RSS) ---${NC}"
echo ""
measure_memory "$INPUT_PRETOOLUSE" "PreToolUse (allow)"
measure_memory "$INPUT_PRETOOLUSE_BLOCK" "PreToolUse (block)"
measure_memory "$INPUT_POSTTOOLUSE" "PostToolUse"
measure_memory "$INPUT_STOP" "Stop"
echo ""
echo -e "Target: ${GREEN}< 5 MB${NC}"
echo ""

# Skip timing benchmark if --memory-only
if [[ "$MEMORY_ONLY" == "true" ]]; then
    echo -e "${BLUE}========================================${NC}"
    echo -e "${GREEN}Memory benchmark complete!${NC}"
    exit 0
fi

# Timing benchmark (requires hyperfine)
check_hyperfine

echo -e "${BLUE}--- Startup Time ---${NC}"
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
echo "Targets:"
echo "  - Startup time: < 10ms"
echo "  - Memory usage: < 5 MB"
