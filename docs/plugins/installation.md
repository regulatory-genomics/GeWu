# Install an application plugin

Build or obtain the application's versioned GeWu package, then run:

```sh
gewu plugin install /path/to/package
gewu plugin list
gewu plugin check paper
gewu plugin check paper --probe --workspace /path/to/test-workspace --settings settings.json
```

Install/check without `--probe` only inspect files; they do not start the app or need a model. The installer prints the installed package directory. Add it to the server's existing `--llm-config` file:

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

Merge this into your existing config. Relative package paths resolve against the config file. `workspaceSettings` can replace `settings` for selected workspace IDs. The optional `serviceOrigins` list permits specific non-loopback HTTP origins for registered service routes; literal loopback addresses can be used for locally managed services. The default `pluginHost` limits can be changed with `startupMs`, `callMs`, `shutdownMs`, `cleanupMs`, and `stderrBytes`.

In the web GUI, **Applications → Services → Start** opens an instance in the selected workspace. Resource-scoped apps ask for a binding type and a workspace-relative path. `check --probe` accepts the equivalent `--resource workspace:///path --resource-type app.kind`. **Close view** closes a panel without stopping a running tool; **Stop** shuts down its instance. Use **Cancel** to request cancellation of one call. An interrupted or unknown outcome should be checked in the application before retrying a write. The editor refuses stale revisions; reopen the file to read its current contents.

To roll back, stop the instance, change the configured package path to the desired installed version, and restart the server. The Services panel can enable or disable configured packages until restart; disabling stops all of that plugin's instances. To disable a plugin persistently, set `enabled` to `false`; `gewu plugin remove paper --version 0.1.0` deletes that package only. Existing plugins may depend on application runtimes such as Tectonic or Manim; the application handles its own dependency checks during `--probe`.
