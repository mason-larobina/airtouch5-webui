# Examples

Reference configuration for running `airtouch5-controller-webui` as a system
service.

## systemd unit

[`airtouch5-controller-webui.service`](./airtouch5-controller-webui.service)
runs the web UI under a dedicated, hardened `airtouch5` service account with
the automation config persisted under `/var/lib/airtouch5/`.

Install steps (also inlined as comments at the top of the unit file):

```sh
# 1. Build + install the binary.
cargo build --release
sudo install -m 0755 target/release/airtouch5-controller-webui \
  /usr/local/bin/airtouch5-controller-webui

# 2. Create the service user.
sudo useradd --system --no-create-home --shell /usr/sbin/nologin airtouch5

# 3. Install the unit file.
sudo install -m 0644 examples/airtouch5-controller-webui.service \
  /etc/systemd/system/airtouch5-controller-webui.service

# 4. Reload + enable + start.
sudo systemctl daemon-reload
sudo systemctl enable --now airtouch5-controller-webui

# 5. Verify.
systemctl status airtouch5-controller-webui
journalctl -u airtouch5-controller-webui -f
```

All options are CLI flags (no env vars); pass them on the `ExecStart=` line in
the unit file (documented in its header). Logging is the one env-driven option
and stays on `RUST_LOG`.
