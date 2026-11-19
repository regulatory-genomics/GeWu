# Findings

## Verified facts

- `runtime.v1` requires `launcher`, `probe`, and `capabilities`, but describes a
  packed runtime rather than an acquisition strategy — evidence:
  `gewu-protocol/schema/runtime.v1.schema.json`.
- `release.v1` requires at least one runtime and every runtime reference contains
  both an artifact and a manifest — evidence:
  `gewu-protocol/schema/release.v1.schema.json`.
- The marketplace client verifies the detached Ed25519 signature before
  parsing/caching the index and verifies artifact byte count and SHA-256 before
  storing an artifact — evidence:
  `gewu-server/src/plugins/marketplace.rs`.
- The old public marketplace page was a placeholder — evidence:
  `/data3/litian/projects/gewu-marketplace/plugin/web/index.html`.
- The public origin already serves the signed catalog and release assets, so a
  static site shares the same origin without a backend — evidence:
  `deploy/serve-marketplace.py`, `https://plugin.regulogen.cn`.
- An external runtime can reuse the existing activation path by synthesizing a
  runtime root (`runtime.json`, `bin/launch`, `bin/probe`, `bin/python3`):
  `cargo test -p gewu-server --test plugin_install` covers reuse and rollback on
  probe failure.
- conda-forge provides `manimpango 0.7.1` and `av`, while the Westlake PyPI
  mirror's `manimpango` sdist is truncated and lacks an `av` wheel; the managed
  Manimble plan therefore takes native Python packages from conda-forge and
  pure-Python packages from pip — evidence: `/tmp/manimble-managed-test*.log`.

## Implementation notes

- Wire contracts: `runtime-plan.v1`, `release.v2`, `marketplace.v2`; the Rust
  `Release` type accepts v1 and v2, and `MarketplaceIndex::select_release_version`
  no longer requires a bundled runtime.
- Providers: `external` (resolve env/command/override, probe, synthesize root),
  `managed` (micromamba prefix + pip, then synthesize root), `bundled`
  (unchanged archive path). Resolved identity is stored on the generation.
- CLI: `runtime discover|plan|probe`; `marketplace-install --runtime
  auto|external|managed|bundled --executable --dry-run`.
- Publisher: `build_index.py` validates v1/v2, provider plans, and emits
  `gewu.marketplace/v2` when any release is v2; `make_release.py` accepts
  `--runtime-plan`.
- Public UI: `site/index.html`, `site/app.js`, `site/styles.css`; verified in a
  real browser (1 card, filters, empty state, deep link) with no console errors.
- Manimble package now ships the engine (`engine/`) and the adapter resolves it
  from `GEWU_PLUGIN_ROOT/engine` when the runtime root has no engine copy.


## Decisions

| Decision | Rationale | Date |
| --- | --- | --- |
| Keep bundled runtime as a fallback | It preserves offline and low-dependency installation for users without a usable local environment. | 2026-11-19 |
| Add typed runtime providers instead of prose-only install instructions | Agents need deterministic discovery, bounded execution, structured errors, and provenance; arbitrary shell from catalog text is not reproducible. | 2026-11-19 |
| Preserve legacy v1 contracts and add an explicit provider-based extension | Existing Paper/Tachyon-era releases and offline caches must continue to install without migration. | 2026-11-19 |
| Host owns runtime writes and activation; Agent chooses and explains | This preserves signature, digest, probe, rollback, and user-data safety invariants. | 2026-11-19 |
| Use a dependency-free static public UI | The existing Cloudflare origin already serves static bytes; a browser-only catalog needs no backend or user account. | 2026-11-19 |
| Keep browser cryptographic authority in GeWu Host | Browser support for Ed25519 verification is not the installation trust boundary; the UI should display provenance, while Host verifies before installation. | 2026-11-19 |
| Use Manimble as the first provider-based migration | Manimble already has a working local environment and exposes the exact external/managed/bundled tradeoff this design targets. | 2026-11-19 |
| Use explicit `release.v2` and `marketplace.v2` contracts for provider-based releases | The current v1 schemas require bundled runtime artifacts and use strict additional-property validation; an explicit wire version keeps old caches and clients unambiguous. | 2026-11-19 |

## Rejected options

- Make every plugin ship a bundled runtime forever — rejected because large
  runtimes duplicate downloads, slow releases, and prevent users from reusing
  compatible or newer local installations.
- Give the Agent unrestricted Markdown shell instructions — rejected because
  execution becomes ambiguous, difficult to validate, and hard to roll back.
- Make the public website install plugins directly — rejected because browser
  installation would bypass the Host's signature, digest, probe, and lifecycle
  authority.
- Replace local runtimes with a remote execution service in this phase —
  rejected because it introduces workspace transfer, privacy, latency, and
  service-availability requirements before the local provider model is proven.

## Open questions

- Which managed provider should be the first-class default for each plugin
  family? Resolve during Task 6 using real Manimble installation and probe
  results; the initial implementation must support micromamba and may add uv
  only where native dependencies do not require conda packages.
