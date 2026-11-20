# GeWu Marketplace

The public catalog is served at:

```text
https://plugin.regulogen.cn
```

It hosts a static Marketplace UI, the signed `marketplace.v2` index, and
immutable release assets. The internal Gitea repository remains the publishing
source:

```text
ssh://git@172.16.211.53:2222/gilbert/gewu-marketplace.git
```

Each release record is `gewu.release/v1` (packed runtime) or
`gewu.release/v2` (package-only, `runtimePlan`, optional packed fallback).
The index is `gewu.marketplace/v2` as soon as any release is v2; older clients
still read the same release fields they understand.

GeWu verifies:

1. the official repository key and detached index signature;
2. repository/key identity and index freshness;
3. package/runtime byte counts and SHA-256 digests;
4. target/API compatibility and runtime probe results, whether the runtime is
   external, managed, or bundled.

The signed index and verified signature are cached under the host-owned
`.gewu` directory and can be read offline. Repository credentials remain
host-side and never enter adapter settings or webviews.

The website is display-only: it renders the signed metadata, deep links, and
download links. It does not install plugins and does not claim to verify Ed25519
in the browser; the GeWu host remains the installation authority.
