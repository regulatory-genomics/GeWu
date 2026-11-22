# Repair a Marketplace install

On the GeWu website, open Marketplace and choose **Install**. The installation
runs on the GeWu server. When it fails, the failure card shows a short stage and
cause. **View details** shows the full provider attempts, bounded log excerpt and
source links. **Copy repair info for Agent** copies a concise handoff instead:
the failure summary, commit-pinned sources, installation guide, host context,
recovery commands and acceptance checks. On HTTP or when clipboard permission is
denied, the page displays the same short handoff as selectable text.

Recovery commands name only providers declared for this host. Before the adapter
probe, inspect `plugin.json` for required settings and binding scope; a
resource-scoped plugin needs both `--resource` and `--resource-type`. The host
does not guess application-specific dependencies or document paths.

Run your agent on the GeWu server as the user who runs GeWu. Use an isolated
user-owned environment and the exact source and recipe revision in the report.
The adapter and runtime-recipe source index is
<https://github.com/regulatory-genomics/gewu-plugin>; a release should link the
pinned commit and plugin path from that repository. A release should also link
its installation guide. Application source remains in the linked upstream
repository, or in a published engine snapshot when the package contains local
engine changes.
The host-local redacted log is under
`~/.gewu/install-attempts/<attempt-id>/install.log`. CLI users can also
retrieve the report with `gewu plugin install-report <attempt-id> [--json]`.
Reports are local files; review the excerpt before sharing it externally.

If the release declares a compatible external runtime, probe the repaired
executable, register it with the version-pinned `gewu plugin marketplace-install
… --runtime external --executable …` command, and run `gewu plugin check …
--probe`. Restart the GeWu server after the CLI registration and open the plugin
in the website. Verify that an actual operation, such as a Manimble slide
render, completes. A missing external provider or native app is called out in
the report; the installer does not install system packages.
