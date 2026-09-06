#!/bin/sh
# rustinfo 部署: 构建 → root 属主副本到 /usr/local/bin → sudoers 免密 → 用户侧包装器
# 改代码重新发布后重跑本脚本即可 (sudoers 已存在则跳过)
set -e
cd "$(dirname "$0")"

"$HOME/.cargo/bin/cargo" build --release
SRC="$PWD/target/release/rustinfo"

# 二进制: root 属主副本, sudoers 只信任这份
pkexec install -m755 -o root -g root "$SRC" /usr/local/bin/rustinfo

# sudoers 免密规则 (仅首次创建)
SUDOERS=/etc/sudoers.d/rustinfo
if ! pkexec test -f "$SUDOERS"; then
    USER_NAME="$(id -un)"
    if ! pkexec sh -c "printf '%s ALL=(root) NOPASSWD: /usr/local/bin/rustinfo\n' '$USER_NAME' > $SUDOERS && chown root:root $SUDOERS && chmod 440 $SUDOERS && visudo -cf $SUDOERS"; then
        pkexec rm -f "$SUDOERS"
        echo "sudoers 写入校验失败, 已回滚" >&2
        exit 1
    fi
fi

# 用户侧包装器: 有免密规则就走 sudo (拿到 RAPL Package 功率), 否则直接跑
mkdir -p "$HOME/.local/bin"
cat > "$HOME/.local/bin/rustinfo" <<'EOF'
#!/bin/sh
BIN=/usr/local/bin/rustinfo
[ "$(id -u)" -eq 0 ] && exec "$BIN" "$@"
exec sudo -n "$BIN" "$@" 2>/dev/null || exec "$BIN" "$@"
EOF
chmod +x "$HOME/.local/bin/rustinfo"

echo "部署完成: rustinfo → /usr/local/bin (root 属主) + ~/.local/bin/rustinfo 包装器"
