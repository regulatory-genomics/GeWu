# GeWu Marketplace

The public catalog is served at:

```text
https://plugin.regulogen.cn
```

It hosts a static Marketplace UI, signed catalogs and immutable release assets.
Publishing credentials and infrastructure remain outside the application host.

`gewu.release/v3` records carry prebuilt, target/profile-aware runtimes and host
feature requirements in a `gewu.marketplace/v3` catalog. The host also reads
`gewu.release/v1` packed runtimes and `gewu.release/v2` runtime plans, including
their existing catalog URLs. New catalogs do not remove legacy snapshots.

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

In the GeWu-hosted Marketplace panel, Install selects a compatible prebuilt
runtime for v3, or follows the signed provider order for a legacy runtime plan.
The installer checks the selected runtime before activating it.
Open is available once the installed release is enabled and its effective
settings are valid. Runtime overrides are under Advanced.

Use the first line of `releaseNotes` for a short statement of the plugin's
purpose (for example, "Write research papers and export HTML/DOCX."). Put
version-specific changes after a blank line; the card keeps the full notes in
its tooltip. This reuses the existing release field without a schema change.

**Copy install instructions** copies the exact signed Marketplace install and
verification commands. Source-building instructions are available under
**Advanced → Install from source**, with pinned app/adapter/recipe/guide links.
Both actions fall back to selectable text when clipboard access is unavailable.
Copying does not install or download anything. Missing source metadata is shown
explicitly; a newer checkout is not a substitute for the requested version.

For new releases, pass `--application-source`, `--plugin-source`,
`--runtime-recipe` and `--installation-guide` to the marketplace publisher. Use
commit-pinned HTTPS links to the app, adapter, recipe and guide used to build the
artifacts. The publisher encodes them in the existing signed
`provenance.buildInputs` list; old clients still accept the index. See
[Install repair](install-repair.md).
