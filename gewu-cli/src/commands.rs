pub(crate) struct Spec {
    pub cmd: Cmd,
    pub name: &'static str,
    pub help: &'static str,
    pub aliases: &'static [&'static str],
    pub idle: bool,
}

macro_rules! commands {
    ($( $cmd:ident, $name:literal, $help:literal, $idle:literal, [$($alias:literal),*]; )+) => {
        #[derive(Clone, Copy, Debug, PartialEq, Eq)]
        pub(crate) enum Cmd { $($cmd),+ }
        pub(crate) const COMMANDS: &[Spec] = &[$(Spec { cmd: Cmd::$cmd, name: $name, help: $help, idle: $idle, aliases: &[$($alias),*] }),+];
    };
}

commands! {
    Build,"/build","Switch to build mode",true,[];
    Plan,"/plan","Switch to planning mode",true,[];
    Model,"/model","Choose model and reasoning effort",true,[];
    Resume,"/resume","Open a saved conversation",false,["/sessions"];
    Settings,"/settings","Inspect and change session settings",false,[];
    Status,"/status","Inspect this session and workspace",false,[];
    Context,"/context","See remaining context tokens",false,[];
    Files,"/files","Browse and preview workspace files",false,[];
    Workflow,"/workflow","Design, save and run a workflow",false,[];
    Tasks,"/tasks","Inspect workflows and cancel a selected run",false,[];
    Usage,"/usage","View cumulative token and cache usage",false,[];
    Agent,"/agent","Choose an agent profile",true,[];
    Effort,"/effort","Set session reasoning effort",true,[];
    Tools,"/tools","List tools advertised by this agent",false,[];
    Pwd,"/pwd","Show the server working directory",false,[];
    Cd,"/cd","Select a server workspace directory",true,["/workspace"];
    Attach,"/attach","Queue a local file for the next message",true,[];
    Copy,"/copy","Copy the latest reply to the Mac clipboard",false,[];
    Export,"/export","Export the displayed transcript as Markdown",false,[];
    Revert,"/revert","Preview and revert conversation turns",true,[];
    Undo,"/undo","Restore the last reverted conversation",true,[];
    Subagents,"/subagents","Show this session's child agents",false,[];
    Todos,"/todos","Inspect this session's task checklist",false,[];
    Skills,"/skills","List loaded skills and their sources",false,[];
    Plugins,"/plugins","Install and manage local application plugins",false,[];
    Mcp,"/mcp","List loaded MCP tools and their servers",false,[];
    New,"/new","Start a chat without a workspace",false,[];
    Delete,"/delete","Select a saved conversation to delete",true,[];
    Help,"/help","Show all commands and shortcuts",false,[];
    Exit,"/exit","Leave the terminal client",false,[];
}

pub(crate) fn lookup(name: &str) -> Option<&'static Spec> {
    COMMANDS
        .iter()
        .find(|spec| spec.name == name || spec.aliases.contains(&name))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn all_names_are_unique_searchable_and_dispatchable() {
        let mut names = std::collections::HashSet::new();
        for spec in COMMANDS {
            for name in std::iter::once(&spec.name).chain(spec.aliases.iter()) {
                assert!(names.insert(name));
                assert_eq!(lookup(name).unwrap().cmd, spec.cmd);
                assert!(matches(name).iter().any(|(name, _)| *name == spec.name));
            }
        }
        assert!(matches("/token").iter().any(|(name, _)| *name == "/usage"));
        assert!(matches("/tmp/data").is_empty());
        assert!(matches("/model x").is_empty());
    }
}

pub(crate) fn matches(query: &str) -> Vec<(&'static str, &'static str)> {
    let Some(query) = query.strip_prefix('/') else {
        return vec![];
    };
    if query.chars().any(char::is_whitespace) || query.contains('/') {
        return vec![];
    }
    let query = query.to_ascii_lowercase();
    let mut hits = Vec::new();
    for spec in COMMANDS {
        let rank = if spec.name[1..].starts_with(&query)
            || spec
                .aliases
                .iter()
                .any(|name| name[1..].starts_with(&query))
        {
            0
        } else if spec.help.to_ascii_lowercase().contains(&query) {
            1
        } else {
            continue;
        };
        hits.push((rank, spec.name, spec.help));
    }
    hits.sort_by_key(|item| item.0);
    hits.into_iter()
        .map(|(_, name, help)| (name, help))
        .collect()
}
