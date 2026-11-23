# GeWu release formats

`gewu.release/v1`, `/v2` and `/v3` describe immutable plugin releases, separately
from the `gewu.plugin/v1` stdio protocol. V1 uses packed runtimes; v2 may declare
external/managed providers and a packed fallback. V3 selects prebuilt runtimes
by target and optional profile, with host-feature and system requirements.

Each release contains:

- a stable `releaseId`, plugin/version/channel, and GeWu compatibility range;
- a small package artifact with URL, byte count, and lowercase SHA-256;
- one or more target-specific runtime artifacts and their `runtime.json`;
- capabilities, permissions, release notes, and source/build provenance.

The host never activates a package solely because it appears in an index. It
downloads the package and selected runtime, verifies both digests, rejects
unsafe archive paths, validates `plugin.json` and `runtime.json`, runs the
runtime probe, and only then atomically commits `plugins/state.json`. The active
pointer is derived from committed state; failures retain the previous generation.

Release descriptors used by the shared packer are kept in each application
repository as `plugins/gewu/release.json`.
