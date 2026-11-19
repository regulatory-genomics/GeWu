---
status: active
created: 2026-11-19
---

# Runtime Providers and Public Marketplace UI

## Goal

Allow GeWu plugins to be installed without forcing every user to download a
packed runtime. A release may reuse a compatible runtime already installed by
the user, let an Agent acquire a reproducible managed runtime from a signed
machine-readable plan, or fall back to the existing bundled runtime. Preserve
the current `plugin.v1` stdio protocol, transactional host-owned activation,
signature/digest verification, rollback, and offline behavior. At the same
time, turn `https://plugin.regulogen.cn` into a polished read-only Marketplace
site that presents releases, capabilities, runtime choices, compatibility,
provenance, install commands, and GeWu deep links.

## Non-goals

- Do not remove or weaken bundled runtime support.
- Do not change the `plugin.v1` adapter protocol or plugin domain logic.
- Do not let catalog text execute arbitrary shell commands or mutate user
  environments without a typed provider and Host verification.
- Do not add accounts, payments, server-side installation, telemetry, or a
  Marketplace backend in this phase.
- Do not make the public website responsible for installing a plugin directly;
  it may launch a `gewu://` link, show commands, and link to artifacts.
- Do not migrate every existing release at once. Existing release/index
  contracts remain readable while the new contract is introduced.

## Constraints

- The GeWu Host remains the installation authority and owns stores,
  generations, active pointers, probes, rollback, disable, and uninstall.
- Runtime acquisition plans, lockfiles, package metadata, and downloaded
  artifacts must be signature/digest protected before activation.
- Existing `release.v1`, `runtime.v1`, and `marketplace.v1` releases remain
  installable; new runtime-provider releases use an explicit versioned
  extension and the client accepts both old and new forms.
- External runtimes are never trusted by path or version alone. They must pass
  the plugin-specific probe and have their resolved executable, package/native
  versions, target, source, and probe result recorded.
- Managed environments are created in a GeWu-owned content-addressed store;
  installing a plugin must not modify the user's system Python, conda, or uv
  environment.
- Agent output must be structured and bounded. Human-readable instructions
  are documentation only; executable acquisition is represented by typed
  providers such as `external`, `uv`, `micromamba`, and `bundled`.
- The public site is static and can be served by the existing Cloudflare
  tunnel. It must work if the catalog contains zero, one, or many releases and
  must not expose signing keys or private configuration.

## Design

### Runtime selection

Introduce a `gewu.runtime-plan/v1` document and a versioned release extension
for runtime options. A release can advertise one or more target-specific
providers:

1. `external`: discover and probe a user-provided executable or known local
   environment;
2. `managed`: create a Host-owned environment using a typed provider and a
   signed lock artifact, with Westlake PyPI/conda mirrors configurable;
3. `bundled`: download and unpack the existing runtime artifact.

The default policy is `auto`, ordered as `external`, `managed`, then
`bundled`. Users can select `prefer-external`, `prefer-managed`, or
`bundled-only`. A successful resolution produces a runtime identity record with
the provider, executable, target, resolved package/native versions, lock hash,
source URLs, and probe result. The identity is attached to the active plugin
generation and is reused until invalidated.

The plan contains requirements, discovery hints, typed provider parameters,
lock/artifact references, launcher/probe paths, and human-readable setup
documentation. It does not contain unrestricted shell snippets. Provider
implementations perform validation, dry-run, bounded execution, cancellation,
and structured errors.

### Release and compatibility

Keep old release records unchanged. Add a versioned release/index extension
that permits package-only releases and runtime options without the current
`runtimes[].artifact` requirement. The parser, publisher, CLI, GUI, and public
site render both legacy bundled releases and provider-based releases. Existing
Paper and any future bundled releases continue through the current path.

### Agent/Host boundary

Expose Agent-facing actions for `runtime.discover`, `runtime.probe`,
`runtime.plan`, `runtime.install`, and `runtime.use`. Each action supports
JSON output, `--dry-run`, non-interactive execution, timeout, bounded logs, and
typed errors with suggested next actions. The Agent chooses among compatible
providers and explains the plan; the Host performs all writes and activation.

### Public Marketplace UI

Add a dependency-free static site under the marketplace repository. It reads
the signed index metadata and release records from the same origin, with no
private API. The page includes:

- branded header and concise explanation of GeWu Marketplace;
- responsive search and channel/platform filters;
- release cards with plugin name, version, capabilities, permissions,
  compatibility, release notes, source commit, and package size;
- runtime badges for local reuse, managed install, and bundled fallback;
- an install button using the existing `gewu://plugin/install?...` deep link;
- copyable CLI command and package/runtime download links;
- a runtime guide explaining `auto`, local runtime reuse, managed setup, and
  bundled fallback;
- generated/expiry metadata and a link to the signed catalog;
- accessible keyboard navigation, visible focus states, reduced-motion support,
  readable contrast, and a mobile layout.

The website does not claim that a browser verified Ed25519. Cryptographic
verification remains in GeWu Host; the site exposes the signed snapshot and
links to its signature for provenance.

## Acceptance gates

- [x] `cargo test --offline -p gewu-protocol -p gewu-runtime -p gewu-server` -> protocol 41+2+5, runtime, install/runtime/state tests pass.
- [x] `./target/debug/gewu plugin runtime discover --json manimble` -> returns structured candidates with source, target, and availability/probe status.
- [x] `./target/debug/gewu plugin marketplace-install manimble --dry-run` -> returns the selected typed provider and target without writing files.
- [x] A failed external probe is rejected with a typed reason and leaves `active.json` unchanged (`plugin_install.rs`).
- [ ] A managed runtime install creates a Host-owned generation, passes the real plugin probe, records runtime identity, and survives restart/rollback.
- [x] A legacy bundled Paper release still installs, probes, and activates from the public v2 index.
- [x] `python3 -m pytest -q publisher/tests` -> 4 passed, including provider-only and bad-plan cases.
- [x] `node --check site/app.js` -> no syntax errors.
- [x] Serving the public mirror and opening `https://plugin.regulogen.cn/` -> Marketplace UI renders Paper; browser reports no console errors.
- [x] `curl -fsS https://plugin.regulogen.cn/index.v1.json` + detached-signature verification -> public catalog matches the signed publisher output.
- [x] The deep link and copied CLI command contain the correct repository, plugin id, version, channel, and no development paths.
- [ ] A fresh HOME can install a provider-based release using a managed runtime, while an offline machine can still install a bundled release from cache.

## Tasks

### Task 1 — Define versioned runtime-plan and release contracts

- Files: `gewu-protocol/schema/runtime-plan.v1.schema.json` (create),
  `gewu-protocol/schema/release.v2.schema.json` (create),
  `gewu-protocol/schema/marketplace.v2.schema.json` (create or extend the
  compatibility union), Rust protocol release/runtime types, and fixtures under
  `gewu-protocol/tests/fixtures/` (modify/create).
- Change: Define typed provider options, target requirements, lock/artifact
  references, discovery/probe metadata, runtime policies, and runtime identity
  records. Keep old v1 parsing and add explicit v2 acceptance rather than
  silently changing v1 semantics.
- Verify: `cargo test --offline -p gewu-protocol` -> legacy fixtures pass and
  new valid/invalid runtime-plan and provider-release fixtures are accepted or
  rejected as specified.

### Task 2 — Implement runtime provider resolution and identity records

- Files: `gewu-runtime/src/lib.rs`, `gewu-runtime/src/specialist.rs`,
  `gewu-server/src/plugins/runtime.rs`, `gewu-server/src/plugins/state.rs`,
  `gewu-server/src/plugins/package.rs` (modify), and
  `gewu-server/tests/plugin_runtime.rs` (modify).
- Change: Add provider discovery, external executable probing, typed managed
  provider planning, provider priority selection, dry-run results, and a
  persisted runtime identity. Reuse the existing artifact store and generation
  model; do not mutate user environments.
- Verify: `cargo test --offline -p gewu-server --test plugin_runtime` -> finds a
  compatible fake/existing runtime, rejects a bad probe, returns a dry-run plan,
  persists identity, and leaves the user environment unchanged.

### Task 3 — Integrate providers with transactional install and lifecycle

- Files: `gewu-server/src/plugins/lifecycle.rs`,
  `gewu-server/src/plugins/mod.rs`, `gewu-server/src/plugins/actions.rs`,
  `gewu-server/src/plugins/marketplace.rs`, and lifecycle/action tests under
  `gewu-server/tests/` (modify).
- Change: Extend install, activation, doctor, rollback, disable, enable, and
  uninstall to resolve a provider before activation. Add clear states for
  `candidate`, `probe_failed`, `active`, and `rollback_available`; retain the
  existing bundled path for v1 releases.
- Verify: `cargo test --offline -p gewu-server --test plugin_marketplace --test plugin_marketplace_actions --test plugin_runtime` -> package-only/provider and bundled installs both activate transactionally, failed probes do not change `active.json`, and rollback restores the prior runtime.

### Task 4 — Add structured CLI and Agent-facing runtime actions

- Files: `gewu/src/plugin.rs`, `gewu-server/src/plugins/actions.rs`, command
  help/JSON schemas near the CLI implementation, and CLI/action tests (modify).
- Change: Add `runtime discover`, `runtime probe`, `runtime plan`,
  `runtime install`, and `runtime use` with JSON output, `--dry-run`,
  `--non-interactive`, `--timeout`, bounded diagnostics, and typed errors. The
  output must contain provider, target, next action, and provenance fields an
  Agent can pass directly to the next action.
- Verify: `cargo run --offline -p gewu -- plugin runtime --help` -> all actions
  and non-interactive flags are documented; malformed input, missing runtime,
  incompatible runtime, dry-run, and successful probe tests return stable JSON
  error/success shapes.

### Task 5 — Update publisher and release tooling for provider-based releases

- Files: `/data3/litian/projects/gewu-marketplace/publisher/build_index.py`,
  `publisher/make_release.py`, `publisher/verify_index.py`,
  `publisher/retarget.py`, `publisher/tests/test_publisher.py`, and new
  provider-based fixtures under `catalog/releases/` or `publisher/tests/`
  (modify/create).
- Change: Validate package-only releases, signed runtime plans, optional
  bundled fallback artifacts, target/provider compatibility, lock digests, and
  provenance. Continue publishing legacy bundled records unchanged.
- Verify: `python3 -m pytest -q publisher/tests` -> legacy Paper-style records,
  provider-only records, provider-plus-bundled records, bad locks, bad target
  data, and digest mismatches produce the expected results; a generated index
  passes detached signature verification.

### Task 6 — Publish Manimble with external and managed runtime options

- Files: `/data3/litian/public_project/Manimble/plugins/gewu/build-release.py`,
  `plugins/gewu/adapter.py`, `engine/requirements.txt`, a committed
  `runtime-plan.v1`/lock input in the Manimble repository, and the Manimble
  release record under `/data3/litian/projects/gewu-marketplace/catalog/releases/`
  (modify/create).
- Change: Make Manimble advertise reuse of a compatible Python environment,
  managed installation using the Westlake mirror with a PyPI fallback only for
  the known broken `manimpango` source, and optional bundled runtime. Define a
  real render/PPTX probe and record resolved versions and native-library checks.
- Verify: the existing working local environment passes the external probe;
  the managed environment installs and passes a real render plus PPTX export;
  `build-release.py` emits a small package, a valid provider plan, and an
  optional bundled runtime whose manifest/probe pass; publisher verification
  accepts the resulting release.

### Task 7 — Implement the public Marketplace UI

- Files: `/data3/litian/projects/gewu-marketplace/site/index.html` (create),
  `site/app.js` (create), `site/styles.css` (create),
  `packages/catalog-client/index.js` (modify if shared parsing is needed), and
  `site/README.md` (create).
- Change: Replace the placeholder public page with a responsive, accessible,
  dependency-free catalog UI. Render both release versions, provider badges,
  compatibility and provenance, install deep links, copyable CLI commands,
  artifact links, release notes, catalog freshness, and runtime guidance.
- Verify: `node --check site/app.js` -> success; a local static server plus
  `curl` -> the page loads, assets are same-origin, and no development paths or
  private configuration are present; browser smoke checks cover empty catalog,
  search/filter, mobile layout, keyboard focus, deep-link generation, and a
  provider-based release card.

### Task 8 — Make public deployment publish the UI and signed distribution atomically

- Files: `deploy/serve-marketplace.py`, `deploy/publish-public.py` (create),
  `deploy/README.md`, `.gitignore`, and the public mirror deployment layout
  (modify/create).
- Change: Copy the site and the verified signed index into the public mirror,
  preserve immutable release assets, reject incomplete publication, and expose
  only the intended static files through the Cloudflare tunnel. Keep index and
  signature updates coordinated so the UI cannot advertise an unpublished
  artifact.
- Verify: `python3 deploy/publish-public.py --root /data3/litian/gewu-marketplace-public --check` -> reports matching site/index/signature; `curl -fsS https://plugin.regulogen.cn/` -> UI 200; a path traversal probe and private-file probe -> 404.

### Task 9 — End-to-end migration, documentation, and release verification

- Files: `docs/plugins/installation.md`, `docs/plugins/marketplace.md`,
  marketplace `README.md`, provider/runtime docs, and end-to-end test scripts
  under `scripts/` (modify/create).
- Change: Document runtime policies, external environment registration,
  managed installation, mirror configuration, bundled fallback, provenance,
  rollback, and the public Marketplace UI. Publish one provider-based Manimble
  release while keeping existing Paper behavior and the withdrawn Tachyon
  listing absent.
- Verify: in a fresh HOME, search the public catalog, open the UI, install
  Manimble through external or managed resolution, run its real probe, restart
  GeWu, inspect `doctor`, perform rollback, and confirm the public signed index
  and UI show only actually published releases.
