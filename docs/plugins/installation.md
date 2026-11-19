# Install an application plugin

## From the signed marketplace (preferred)

GeWu ships the official Ed25519 public key and the internal Gitea catalog URL:

```sh
gewu plugin search tachyon
gewu plugin info tachyon --version 0.1.0
gewu plugin marketplace-install tachyon --version 0.1.0
gewu plugin doctor
gewu plugin rollback tachyon
gewu plugin disable tachyon
gewu plugin enable tachyon
gewu plugin uninstall tachyon
```

The host downloads the package and the matching packed runtime, verifies both
SHA-256 digests, rejects unsafe archives, validates `plugin.json` and
`runtime.json`, runs the runtime probe, and only then switches `active.json`.
A failed download, digest, or probe leaves the previous generation active.

The GUI exposes the same path in the **Marketplace** tab of the Applications
panel. A website or launcher can hand one release to the same installer with:

```sh
gewu plugin open-link 'gewu://plugin/install?repo=official&id=tachyon&version=0.1.0'
```

Deep links accept only `repo=official`, `id`, `version`, and `channel`; local
paths, tokens, and runtime executables are never accepted.

Overrides for a mirror or a test key:

```text
GEWU_MARKETPLACE_BASE_URL   index directory URL
GEWU_MARKETPLACE_PUBLIC_KEY base64 raw Ed25519 public key
```

The current Gitea instance is LAN-only. On a machine that cannot reach it
directly (for example this development workstation), point
`GEWU_MARKETPLACE_BASE_URL` at a tunnel or mirror that serves the same signed
index and assets.

## From a local package directory

```sh
gewu plugin install /path/to/package
gewu plugin list --json
gewu plugin check tachyon
gewu plugin check tachyon --probe --workspace /path/to/workspace --settings settings.json
```

`install` copies the package into a host-owned generation and records it in
`~/.gewu/plugins/state.json`. It does not execute the application.

## From downloaded release archives

For offline release verification:

```sh
gewu plugin install-archive release.json package.zip runtime.zip --workspace /path/to/workspace
```

This performs the same digest, archive, manifest, and runtime probe checks as a
marketplace install.

## Configuration notes

Marketplace-installed plugins do not need a `package` path in `config.json`.
Explicit `plugins` entries still win and keep their legacy settings. A
development entry still looks like this:

```json
{
  "plugins": {
    "paper": {
      "enabled": true,
      "package": "/home/example/.gewu/plugins/paper/0.1.0",
      "settings": {"executable": "/usr/local/bin/paper"}
    }
  }
}
```

`workspaceSettings` can replace `settings` for selected workspace IDs. The
optional `serviceOrigins` list permits specific non-loopback HTTP origins for
registered service routes; literal loopback addresses are used for locally
managed services. `pluginHost` limits are `startupMs`, `callMs`, `shutdownMs`,
`cleanupMs`, and `stderrBytes`.

## Lifecycle

- **disable** stops active instances and keeps the installed generation.
- **enable** reopens the plugin on the next start or instance open.
- **rollback** switches to the previously active generation.
- **uninstall** removes the plugin's generations and state only; workspaces,
  Canvas documents, Paper outputs, and Manimble decks are never removed.
- The bundled `gewu-marketplace` system plugin cannot be disabled or removed.

An invalid installed package is shown as a blocked entry with its error instead
of disappearing from the panel.
