# Install an application plugin

## From the signed marketplace (preferred)

Prebuilt (release/v3) plugins need no language runtime or package manager
installed separately. GeWu selects a bundle for the execution host, verifies
the signed metadata and artifact digests, prepares the runtime in its final
private directory, and runs the plugin's functional smoke check before
activation. On a remote GeWu installation, the execution target is the server,
not the computer displaying the browser UI.

For cached artifacts, `--offline` prevents artifact downloads and excludes
managed runtime resolution. The cached signed catalog must still be current.
`--dry-run` uses the cached catalog to show the target, profile and estimated
missing download bytes without preparing a runtime. It does not refresh the
catalog; run `gewu plugin search <plugin> --refresh` first if needed.

Optional PDF or TeX runtimes, where published for a target, can be prepared
from the release's Advanced menu or with `--profile pdf` / `--profile tex`.
They get a separate generation; rollback restores the previous one. Ordinary
installs select the core profile. Opening server-backed Studio views requires
an isolated application origin; local GeWu GUI defaults to `gui.gewu.localhost`, and a
remote administrator configures the public and per-view origins once.

GeWu ships the official Ed25519 public key and the public catalog:

For the quickest setup, choose **Install** in Marketplace or use the exact
release's **Copy install instructions** action. The default instructions run
the signed Marketplace installer with `--runtime auto`, then require a plugin
check and one real operation. Source-building instructions are under
**Advanced → Install from source**.

```sh
gewu plugin search manimble
gewu plugin info manimble --version 0.2.0
gewu plugin marketplace-install manimble
gewu plugin doctor
gewu plugin rollback manimble
gewu plugin disable manimble
gewu plugin enable manimble
gewu plugin uninstall manimble
```

The host downloads the package, verifies its SHA-256, rejects unsafe archives,
validates `plugin.json`, resolves a runtime, runs the runtime probe, and only
then switches `active.json`. A failed download, digest, provider install, or
probe leaves the previous generation active.

## Runtime policies

A release may advertise a signed runtime plan instead of a packed runtime.
Choose a policy with `--runtime`:

```sh
# Reuse a compatible interpreter already on this host
gewu plugin runtime discover manimble --probe
gewu plugin runtime probe manimble --executable /path/to/python
gewu plugin marketplace-install manimble --runtime external --executable /path/to/python

# Let GeWu create a host-owned environment from the plan
gewu plugin marketplace-install manimble --runtime managed

# Download the packed runtime (legacy or explicit fallback)
gewu plugin marketplace-install paper --runtime bundled

# Inspect the choice without writing anything
gewu plugin marketplace-install manimble --dry-run
gewu plugin runtime plan manimble --provider external --json
```

`auto` (the default) follows the release's declared order, normally
`external`, then `managed`, then `bundled`. External and managed runtimes are
materialized as the same `runtime.json` + launcher + probe layout a packed
runtime uses, so verification and rollback are unchanged. The resolved provider,
executable or prefix, target, and probe result are recorded in
`plugins/state.json`.

External discovery is bounded to the explicit `--executable`, the provider's
declared environment hint, and its declared command names on PATH/user bin
directories. It does not scan all virtual environments. An activated venv is
usable through PATH; for a different environment, supply its interpreter:

```sh
gewu plugin marketplace-install gewu-browser --version 0.1.2 --channel stable \
  --runtime auto --executable /absolute/path/to/venv/bin/python
```

The wrapper keeps the selected interpreter path, including venv symlinks,
instead of replacing it with the base Python executable. Keep that environment
in place while using the plugin. Auto falls back to the next declared provider
if the external environment fails its probe; explicit `--runtime external`
reports the failure instead. The Browser plan recipe uses external then bundled
and checks actual headless page rendering and PNG capture without network
access. Older signed release plans retain their own declared order.

Managed installations never modify your system Python or conda base. GeWu
creates a prefix under its own installation root. Set `GEWU_MICROMAMBA` to
choose a micromamba binary and `GEWU_PIP_INDEX_URL` to choose a pip mirror.

```sh
GEWU_PIP_INDEX_URL=https://pypi.org/simple gewu plugin marketplace-install manimble --runtime managed
```

The public Marketplace UI is at <https://plugin.regulogen.cn/>. It shows
capabilities, provenance, runtime providers, and the exact install command for
each release; installation is always performed by the host.

The GUI exposes the same path in the **Marketplace** tab of the Applications
panel. A website or launcher can hand one release to the same installer with:

```sh
gewu plugin open-link 'gewu://plugin/install?repo=official&id=paper&version=0.2.0'
```

Deep links accept only `repo=official`, `id`, `version`, and `channel`; local
paths, tokens, and runtime executables are never accepted.

Overrides for a mirror or a test key:

```text
GEWU_MARKETPLACE_BASE_URL     index directory URL
GEWU_MARKETPLACE_PUBLIC_KEY   base64 raw Ed25519 public key
GEWU_MARKETPLACE_ASSET_ORIGIN rewrite only scheme/host/port of artifact URLs
```

The default official catalog is public:

```text
https://plugin.regulogen.cn/index.v1.json
```

Override the base URL for a mirror, and `GEWU_MARKETPLACE_ASSET_ORIGIN` when only
artifact downloads need a different origin. The artifact path and its SHA-256
stay authoritative, so a rewritten URL cannot change content.

The internal Gitea repository remains the publishing source. On a machine that
cannot reach the public origin, point the base URL at a local mirror serving the
same signed bytes.

## From a local package directory

```sh
gewu plugin install /path/to/package
gewu plugin list --json
gewu plugin check tachyon
gewu plugin check tachyon --probe --workspace /path/to/workspace --settings settings.json
```

`install` copies the package into a host-owned generation and records it in
`~/.gewu/plugins/state.json`. It does not execute the application.

For source installation, follow the pinned application's guide to stage the
adapter and bind its runtime **before** this command. `plugin install` does not
discover or download dependencies, and the host starts adapters with a minimal
environment. Use a compatible existing runtime first; build an isolated one
only for missing dependencies. Do not compile the runtime's applications from
source unless that is explicitly needed.

For Browser, its source tree supplies both the SDK helper and a local staging
command (run from the pinned checkout's `browser/` directory):

```sh
python3 scripts/stage.py --out /absolute/path/to/new-staged-package \
  --python /absolute/path/to/venv/bin/python
gewu plugin install /absolute/path/to/new-staged-package
gewu plugin check gewu-browser --version 0.1.2 --probe \
  --workspace /absolute/path/to/workspace --session installation-check
```

Pass `--browsers /absolute/path/to/cache` to staging when using a non-default
Playwright cache. This binds host-local paths in the launcher, not in a
redistributable release. The source handoff must stop with a specific blocker
if a different plugin's pinned guide provides no supported runtime-binding path.
After CLI installation restart a running GeWu, open the plugin and verify one
real page/document/render/export operation. `check --probe` alone verifies the
adapter lifecycle, not the final application output.

## From downloaded release archives

For offline release verification:

```sh
gewu plugin install-archive release.json package.zip runtime.zip --workspace /path/to/workspace
```

This performs the same digest, archive, manifest, and runtime probe checks as a
marketplace install.

## Configuration notes

Marketplace-installed plugins do not need a `package` path in `config.json`,
and an entry may omit `package` entirely to use the active generation. This is
the normal way to give a marketplace plugin its settings:

```json
{
  "plugins": {
    "tachyon": {
      "enabled": true,
      "settings": {
        "dataset": "my_project",
        "service": {"kind": "managed", "endpoint": "http://127.0.0.1:8321"}
      }
    }
  }
}
```

An explicit `package` path still wins and keeps legacy development settings:

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

Uninstall removes the plugin's entry from the default `~/.gewu/config.json`
only when that file is strict JSON, saving a backup first. JSONC files and custom
configuration paths are retained; remove their plugin entry manually if needed.

The workspace-wide enable state and a session's own choice are separate. The
Preview plugin manager's *Enable/Disable in this session* writes a per-session
override that wins until you choose *Follow global setting*; a globally
disabled plugin therefore stays openable from one session without re-enabling
it everywhere. `gewu plugin enable|disable` changes the installation default
used by a new GeWu process, not a running one.

An invalid installed package is shown as a blocked entry with its error instead
of disappearing from the panel.

## Application context and Agent calls

GeWu keeps the current application context in the host: workspace, session,
plugin instance, bound resource, focused view and generation. A typed plugin
tool such as `tachyon.render` uses that context automatically. The generic
`plugin_call` gateway uses the same resolver, so an asynchronous call also
follows the focused ready view when the caller does not provide `viewId`.

The host returns a bounded `appContext` receipt with instance and view
observability. Published view selections use the same structured envelope when
they are included in the next Agent prompt; package paths, runtime details,
service headers and view assets are never injected into the model context.

Use `plugin_describe` and `plugin_call` for discovery, debugging and generic
fallbacks. Stable typed tools are the normal Agent surface because they avoid
making the Agent manage instance and view identifiers for every domain action.

## Linux execution baseline

The release runtime carries the user-space pieces that a clean server may
lack: a matching Python/Node interpreter, application libraries, a minimal
fontconfig database and (for the Pixi packer) a CA bundle. A clean host still
needs a supported glibc (Linux x86_64 2.28+) and, when a remote GeWu server is
used, the plugin runs on that server rather than on the browser's computer.

The repository's `scripts/check-plugin-clean-install.py` verifies signed
artifacts with a fresh HOME, hostile PATH entries for common package managers,
and no network besides loopback. `--download-first` starts a loopback mirror
that supports byte ranges, installs with no pre-filled artifact cache, then
stops the mirror before the real-operation check. `--linux-rootfs` runs the same
check inside an extracted clean userspace. This is a development/release gate,
not a user prerequisite. Verify macOS on actual hardware separately; a Linux
pass is not evidence for macOS or a different libc/SELinux policy.
