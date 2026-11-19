# GeWu Marketplace

The first distribution repository is the internal Gitea repository:

```text
ssh://git@172.16.211.53:2222/gilbert/gewu-marketplace.git
```

The repository publishes a canonical `marketplace.v1` JSON snapshot and a
detached Ed25519 signature. Package/runtime artifacts are versioned Gitea
Release assets; binary runtimes are never committed to the Git tree.

GeWu verifies:

1. the official repository key and detached index signature;
2. repository/key identity and index freshness;
3. package/runtime byte counts and SHA-256 digests;
4. target/API compatibility and runtime probe results.

The signed index and verified signature are cached under the host-owned
`.gewu` directory and can be read offline. Repository credentials, if a
private repository is used later, remain host-side and never enter adapter
settings or webviews.
