# deploy-roger

Rebuild and deploy roger after a code change (on the host running the service).

1. From the repo, build the release binary: `cargo build --release`.
2. Restart the service: `systemctl --user restart roger`. State lives in `~/.roger`,
   so the Matrix session restores — no re-login.
3. Verify: `journalctl --user -u roger --since "20 seconds ago"` should show
   `restored session for @…` and no `error`/`panic`.
4. Config-only changes don't need a rebuild — hot-reload instead:
   `systemctl --user reload roger` (SIGHUP) re-reads `config/` live.
