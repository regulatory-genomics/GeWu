# Plugin compatibility and acceptance

- Public adapter API: `gewu.plugin/v1`, see [protocol and fixtures](protocol-v1.md).
- Verified development platform: Linux. On macOS independent process-group registration is not implemented; only use adapters that can end and reap their workers on their own. No Windows lifecycle support is claimed.
- The reference package is `gewu-server/tests/fixtures/plugin-adapter`. It exercises an opaque iframe, a native service through the separate GUI proxy, file results, progress, cancellation and real shutdown. Browser refresh can restore layouts and attach to current server activity. Restarting the server does not resume native work.
- Run `scripts/check-plugin-host.sh --contracts --browser` for host regressions. Run `--applications` with a real release test manifest when application packages and their native runtime dependencies are ready. Fixture results are not a substitute for those smoke tests.
