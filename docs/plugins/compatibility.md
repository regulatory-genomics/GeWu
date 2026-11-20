# Plugin compatibility and acceptance

- Public adapter API: `gewu.plugin/v1`, see [protocol and fixtures](protocol-v1.md).
- Verified development platform: Linux. On macOS independent process-group registration is not implemented; only use adapters that can end and reap their workers on their own. No Windows lifecycle support is claimed.
- The reference package is `gewu-server/tests/fixtures/plugin-adapter`. It exercises an opaque iframe, a native service through the separate GUI proxy, file results, progress, cancellation and real shutdown. Browser refresh can restore layouts and attach to current server activity. Restarting the server does not resume native work.
- Run `scripts/check-plugin-host.sh --contracts --browser` for host regressions. Run `--applications` with a real release test manifest when application packages and their native runtime dependencies are ready. Fixture results are not a substitute for those smoke tests.

## Real application gate

`python3 scripts/check-plugin-applications.py /path/to/applications.json` requires
one entry each for Paper, Tachyon and Manimble. Each entry supplies `id`,
`package`, `settings`, `workspace`, `scenario`, `guiSpec` and `realSmokeArgv`
(an argv array, not shell text). Paths are absolute or relative to the manifest.
Use disposable workspaces and configure native dependencies explicitly.

The gate runs the native smoke, stdio conformance scenario and each application's
registered browser tests against the actual GeWu server/GUI. Set `GEWU_BIN` if
the binary is not `target/debug/gewu`. An application browser spec must scope
Open actions to its plugin row because the bundled Marketplace is also listed.

The default browser suite does not register application tests without both
`GEWU_APP_SPEC` and `GEWU_APP_PLUGIN`; its success alone certifies only the
fixture host. Record package versions/hashes and passed, failed or blocked native
and GUI checks when reporting application compatibility.
