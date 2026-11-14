# Findings

- The current GUI receives the root catalog through `Info`, renders a ROOT selector only when more than one root exists, and switches roots through `switch_workspace`.
- The current server already canonicalizes configured roots and applies `PathEnforcement` inside the selected root.
- A browser-native folder picker is the wrong primitive for a remote GeWu server: it grants access to the browser machine, not the server host.
- Runtime roots need to be session-scoped and persisted by ID/path; they must not be added to the model tool surface.
- Runtime IDs use `runtime:<canonical-path>` so no new database column or global mutable catalog is required; server-side resolution validates the directory again on reconnect.
- Full end-to-end verification exposed a separate propagation bug: `create_agent` discarded `ProfileInputs.working_directory` and rebuilt tools with `std::env::current_dir()`. The fix keeps the supplied session root, with a regression test executing `bash pwd` in a temporary root.
