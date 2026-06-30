# Deploy

Systemd user unit for running Roger on the `ai` machine.

## Install

```bash
# Build release binary first
cd ~/.openclaw/workspace/roger
cargo build --release

# Copy unit
mkdir -p ~/.config/systemd/user
cp deploy/roger.service ~/.config/systemd/user/

# Enable and start
systemctl --user daemon-reload
systemctl --user enable roger
systemctl --user start roger

# Check status
systemctl --user status roger
journalctl --user -u roger -f
```

## Lingering (survive logout)

```bash
loginctl enable-linger $USER
```

This keeps the user session alive so services run even when you're not logged in.
