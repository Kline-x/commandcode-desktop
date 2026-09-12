#!/usr/bin/env sh
# 本地「提交前自检」脚本 —— 与 CI 完全一致的检查集合
#
# 用法：scripts/check.sh
# 这是**唯一的事实来源**：CI 与 pre-commit 都调用它，避免两套标准漂移。

set -e

root=$(git rev-parse --show-toplevel)
cd "$root"

GREEN=$(printf '\033[32m'); RESET=$(printf '\033[0m')
step() { printf '\n%s==> %s%s\n' "$GREEN" "$1" "$RESET"; }

step "cargo fmt --all -- --check"
cargo fmt --all -- --check

step "cargo clippy -p cc-server --all-targets -- -D warnings"
cargo clippy -p cc-server --all-targets -- -D warnings

step "cargo test -p cc-server"
cargo test -p cc-server

if command -v typos >/dev/null 2>&1; then
  step "typos"
  typos
else
  printf '\n(跳过 typos：未安装。安装：brew install typos-cli)\n'
fi

if command -v cargo-deny >/dev/null 2>&1; then
  step "cargo deny check"
  cargo deny check
else
  printf '\n(跳过 cargo-deny：未安装。安装：cargo install cargo-deny --locked)\n'
fi

printf '\n%s全部检查通过%s\n' "$GREEN" "$RESET"
