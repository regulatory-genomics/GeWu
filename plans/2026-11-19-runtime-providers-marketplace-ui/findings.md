# Findings

## Verified facts

- The current `runtime.v1` manifest requires `launcher`, `probe`, and
  `capabilities`, but it describes a packed runtime rather than an acquisition
  strategy — evidence: `gewu-protocol/schema/runtime.v1.schema.json`.
- The current `release.v1` requires at least one runtime and requires every
  runtime reference to contain both an artifact and a manifest — evidence:
  `gewu-protocol/schema/release.v1.schema.json`.
- The current marketplace client verifies the detached Ed25519 signature before
  parsing/caching the index and verifies artifact byte count and SHA-256 before
  storing an artifact — evidence:
  `gewu-server/src/plugins/marketplace.rs`.
- The current public marketplace page is a placeholder that tells users to use
  the GeWu Marketplace tab — evidence:
  `/data3/litian/projects/gewu-marketplace/plugin/web/index.html`.
- The public origin already serves the signed catalog and release assets, so a
  static site can share the same origin without a new backend — evidence:
  `/data3/litian/projects/gewu-marketplace/deploy/serve-marketplace.py` and the
  existing `https://plugin.regulogen.cn` deployment.
- Existing host lifecycle code already has transactional package/runtime
  installation and generation state, which should be extended rather than
  replaced — evidence: `gewu-server/src/plugins/lifecycle.rs`,
  `gewu-server/src/plugins/state.rs`, and `gewu-server/src/plugins/runtime.rs`.

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
