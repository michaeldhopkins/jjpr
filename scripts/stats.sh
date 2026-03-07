#!/bin/bash
#
# Generate codebase statistics for jjpr.
#
# Usage: ./scripts/stats.sh

set -e

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
SRC_DIR="$REPO_ROOT/src"

# Count source files
file_count=$(find "$SRC_DIR" -name "*.rs" | wc -l | tr -d ' ')

# Count total lines
total_lines=$(find "$SRC_DIR" -name "*.rs" -exec cat {} + | wc -l | tr -d ' ')

# Count test lines (everything after #[cfg(test)] in each file)
test_lines=0
for file in $(find "$SRC_DIR" -name "*.rs"); do
    test_start=$(grep -n "#\[cfg(test)\]" "$file" 2>/dev/null | head -1 | cut -d: -f1)
    if [ -n "$test_start" ]; then
        file_total=$(wc -l < "$file" | tr -d ' ')
        file_test_lines=$((file_total - test_start + 1))
        test_lines=$((test_lines + file_test_lines))
    fi
done

app_lines=$((total_lines - test_lines))

# Count test functions
test_funcs=$(grep -r "#\[test\]" "$SRC_DIR" --include="*.rs" 2>/dev/null | wc -l | tr -d ' ')

# Count structs and enums
types=$(grep -r "^pub struct\|^struct\|^pub enum\|^enum" "$SRC_DIR" --include="*.rs" 2>/dev/null | wc -l | tr -d ' ')

# Count direct dependencies (non-dev, non-build)
deps=$(grep -A 100 "^\[dependencies\]" "$REPO_ROOT/Cargo.toml" | grep -B 100 "^\[" | grep -v "^\[" | grep -v "^#" | grep -v "^$" | grep "=" | wc -l | tr -d ' ')

# Calculate percentages
if [ "$total_lines" -gt 0 ]; then
    app_pct=$((app_lines * 100 / total_lines))
    test_pct=$((test_lines * 100 / total_lines))
else
    app_pct=0
    test_pct=0
fi

# Format numbers with commas
format_num() {
    printf "%'d" "$1" 2>/dev/null || printf "%d" "$1"
}

echo "## Jjpr Codebase Statistics"
echo ""
printf "| %-22s | %-18s |\n" "Metric" "Count"
printf "| %-22s | %-18s |\n" "----------------------" "------------------"
printf "| %-22s | %-18s |\n" "**Total source files**" "$file_count"
printf "| %-22s | %-18s |\n" "**Total lines**" "$(format_num $total_lines)"
printf "| %-22s | %-18s |\n" "**Application lines**" "~$(format_num $app_lines) (${app_pct}%)"
printf "| %-22s | %-18s |\n" "**Test lines**" "~$(format_num $test_lines) (${test_pct}%)"
printf "| %-22s | %-18s |\n" "**Test functions**" "$test_funcs"
printf "| %-22s | %-18s |\n" "**Structs/Enums**" "$types"
printf "| %-22s | %-18s |\n" "**Direct dependencies**" "$deps"
echo ""

echo "### Directory Structure"
echo "\`\`\`"
echo "src/"
for dir in "$SRC_DIR"/*/; do
    if [ -d "$dir" ]; then
        dirname=$(basename "$dir")
        count=$(find "$dir" -maxdepth 1 -name "*.rs" | wc -l | tr -d ' ')
        printf "├── %-12s (%d files)\n" "$dirname/" "$count"
        # Check for subdirectories
        for subdir in "$dir"/*/; do
            if [ -d "$subdir" ]; then
                subdirname=$(basename "$subdir")
                subcount=$(find "$subdir" -maxdepth 1 -name "*.rs" | wc -l | tr -d ' ')
                printf "│   └── %-8s (%d files)\n" "$subdirname/" "$subcount"
            fi
        done
    fi
done
root_count=$(find "$SRC_DIR" -maxdepth 1 -name "*.rs" | wc -l | tr -d ' ')
printf "└── (%d files in root)\n" "$root_count"
echo "\`\`\`"
echo ""

echo "### Largest Files"
printf "| %-20s | %5s | %10s | %9s |\n" "File" "Lines" "Test Lines" "App Lines"
printf "| %-20s | %5s | %10s | %9s |\n" "--------------------" "-----" "----------" "---------"

# Get top 10 files by line count with test breakdown
find "$SRC_DIR" -name "*.rs" -exec wc -l {} + | sort -rn | head -11 | tail -10 | while read -r lines file; do
    if [ -n "$file" ] && [ -f "$file" ]; then
        relpath="${file#$SRC_DIR/}"
        # Find test section start
        test_start=$(grep -n "#\[cfg(test)\]" "$file" 2>/dev/null | head -1 | cut -d: -f1)
        if [ -n "$test_start" ]; then
            file_test=$((lines - test_start + 1))
            file_app=$((test_start - 1))
        else
            file_test=0
            file_app=$lines
        fi
        printf "| %-20s | %5s | %10s | %9s |\n" "\`$relpath\`" "$lines" "$file_test" "$file_app"
    fi
done
