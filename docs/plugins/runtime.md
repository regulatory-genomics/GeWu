# Packed runtime ABI

A runtime archive contains:

```text
runtime.json
bin/launch
bin/probe
<engine payload>
```

`runtime.json` declares the plugin, semantic version, target OS/architecture/
libc, launcher, probe, and capabilities. GeWu selects an exact compatible
target before activation and runs `bin/probe` in the staged generation.

The adapter receives only these host-owned variables:

```text
GEWU_PLUGIN_ROOT
GEWU_RUNTIME_ROOT
GEWU_WORKSPACE_ROOT
GEWU_RUNTIME_TARGET
```

Application settings describe user choices such as a dataset or document; they
do not contain development-machine executable paths in marketplace releases.
Legacy local packages may still accept their old executable settings while they
are being migrated.
