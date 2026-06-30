# Deploy

Systemd user units for running Roger and LM Studio on the `ai` machine.

## Install

```bash
# Build release binary first
cd ~/.openclaw/workspace/roger
cargo build --release

# Copy units
mkdir -p ~/.config/systemd/user
cp deploy/lmstudio-server.service ~/.config/systemd/user/
cp deploy/roger.service ~/.config/systemd/user/

# Enable and start
systemctl --user daemon-reload
systemctl --user enable lmstudio-server roger
systemctl --user start lmstudio-server roger

# Check status
systemctl --user status lmstudio-server roger
journalctl --user -u roger -f
```

## Lingering (survive logout)

```bash
loginctl enable-linger $USER
```

This keeps the user session alive so services run even when you're not logged in.
