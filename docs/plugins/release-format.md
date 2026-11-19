# GeWu release format v1

`release.v1` describes one immutable plugin release. It is separate from the
existing `plugin.v1` stdio protocol.

Each release contains:

- a stable `releaseId`, plugin/version/channel, and GeWu compatibility range;
- a small package artifact with URL, byte count, and lowercase SHA-256;
- one or more target-specific runtime artifacts and their `runtime.json`;
- capabilities, permissions, release notes, and source/build provenance.

The host never activates a package solely because it appears in an index. It
downloads the package and selected runtime, verifies both digests, rejects
unsafe archive paths, validates `plugin.json` and `runtime.json`, runs the
runtime probe, and only then atomically writes the active generation pointer.

Release descriptors used by the shared packer are kept in each application
repository as `plugins/gewu/release.json`.
