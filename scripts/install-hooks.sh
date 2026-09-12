#!/usr/bin/env sh
# 安装本仓库的 git 钩子（每个 clone 需执行一次；钩子文件本身不入 git）
#
# 用法：scripts/install-hooks.sh

set -e

root=$(git rev-parse --show-toplevel)
cd "$root"

hooks_dir=$(git rev-parse --git-path hooks)
mkdir -p "$hooks_dir"

# 只复制单个钩子文件，保留 git 的默认 hooks 行为
cp scripts/pre-commit "$hooks_dir/pre-commit"
chmod +x "$hooks_dir/pre-commit"

printf '已安装 pre-commit 钩子：%s/pre-commit\n' "$hooks_dir"
printf '跳过检查可用：git commit --no-verify\n'
