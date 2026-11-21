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

In the GeWu-hosted Marketplace panel, Install prefers a compatible packed
runtime where present. The installer checks the runtime before activating it.
Open is available once the installed release is enabled and its effective
settings are valid. Runtime overrides are under Advanced.

Use the first line of `releaseNotes` for a short statement of the plugin's
purpose (for example, "Write research papers and export HTML/DOCX."). Put
version-specific changes after a blank line; the card keeps the full notes in
its tooltip. This reuses the existing release field without a schema change.

**Install instructions** opens a copyable source-installation handoff before
installation, including the exact release, source commit, adapter, recipe and
guide. **Copy instructions** falls back to selectable text on HTTP LAN hosts.
It does not start an install or download a runtime. Missing source metadata is
shown explicitly; a newer source checkout is not a substitute for the requested
plugin version. The existing **Install** action remains available.

For new releases, pass `--application-source`, `--plugin-source`,
`--runtime-recipe` and `--installation-guide` to the marketplace publisher. Use
commit-pinned HTTPS links to the app, adapter, recipe and guide used to build the
artifacts. The publisher encodes them in the existing signed
`provenance.buildInputs` list; old clients still accept the index. See
[Install repair](install-repair.md).
