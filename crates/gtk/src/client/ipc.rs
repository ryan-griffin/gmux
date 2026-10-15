//! Route client actions to the client-owned socket, independently of the daemon.
use gmux_client::ipc;
use serde_json::Value;

pub fn call(body: impl serde::Serialize) -> anyhow::Result<Value> {
    let mut body = serde_json::to_value(body)?;
    let method = body["method"].as_str().unwrap_or("").to_owned();
    let client = matches!(
        method.as_str(),
        "quit"
            | "_state"
            | "new"
            | "split"
            | "close"
            | "close-tab"
            | "rename"
            | "remove-tab-name"
            | "list"
            | "read"
            | "send"
            | "notify"
            | "launch"
            | "workspace-add"
            | "workspace-edit"
            | "workspace-remove"
            | "move"
            | "focus"
            | "filter"
            | "windows"
            | "new-window"
            | "detach"
            | "renderer-info"
            | "search-tabs"
            | "screenshot"
            | "resize"
            | "action"
            | "_desktop-save"
            | "_desktop-tab"
    );
    let path = ipc::companion_path(".ui");
    if client && let Some(reply) = ipc::call_if_running(&path, &body)? {
        return Ok(reply);
    }
    if matches!(
        method.as_str(),
        "workspace-add" | "workspace-edit" | "workspace-remove" | "move"
    ) {
        anyhow::bail!("Workspace organization requires a running GTK app");
    }
    anyhow::ensure!(
        !matches!(
            method.as_str(),
            "quit"
                | "launch"
                | "focus"
                | "screenshot"
                | "windows"
                | "new-window"
                | "detach"
                | "renderer-info"
                | "search-tabs"
                | "resize"
                | "action"
                | "filter"
                | "_desktop-save"
                | "_desktop-tab"
        ),
        "Command requires a running GTK app"
    );
    let endpoint = if let Some(connection) = body
        .as_object_mut()
        .and_then(|body| body.remove("connection"))
    {
        let name = connection
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("Invalid connection name"))?;
        if name != "local" {
            Some(ipc::Endpoint::remote(name.to_owned(), None))
        } else {
            None
        }
    } else {
        None
    };
    if method == "new" {
        let mut command: super::control::Command = serde_json::from_value(body)?;
        if let super::control::Command::New { workspace, .. } = &command {
            anyhow::ensure!(
                !matches!(workspace, Some(Some(_))),
                "Workspace organization requires a running GTK app"
            );
        }
        command.resolve_directory(endpoint.is_none())?;
        body = serde_json::to_value(
            command
                .into_execution()
                .map_err(|_| anyhow::anyhow!("Command requires a running GTK app"))?,
        )?;
    }
    if let Some(endpoint) = endpoint {
        return endpoint.call(body);
    }
    gmux_client::connections::call(body)
}
