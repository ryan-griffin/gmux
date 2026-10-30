//! Route client actions to the client-owned socket, independently of the daemon.
use super::control::Request;
use gmux_client::ipc;
use serde_json::Value;

fn client_request(body: &Value) -> anyhow::Result<Option<Request>> {
    match Request::decode(body.clone()) {
        Ok(request) => Ok(Some(request)),
        Err(error) => {
            // Connection selection belongs to this client, not to the strict
            // daemon request. Validate execution fields without that selector.
            let mut execution = body.clone();
            if let Some(fields) = execution.as_object_mut() {
                fields.remove("connection");
            }
            if gmux_protocol::control::Request::decode(execution).is_ok() {
                Ok(None)
            } else {
                Err(error)
            }
        }
    }
}

pub fn call(body: impl serde::Serialize) -> anyhow::Result<Value> {
    let mut body = serde_json::to_value(body)?;
    let method = body["method"].as_str().unwrap_or("").to_owned();
    // Configuration requests own their connection field. It is not an
    // execution selector and must reach the configuration handler intact.
    if gmux_client::connections::handles(&method) {
        return gmux_client::connections::call(body);
    }
    let decoded = client_request(&body)?;
    let client = decoded.as_ref().is_some_and(Request::uses_ui);
    let path = gmux_platform::unix::ipc::companion_path(".ui");
    if client && let Some(reply) = gmux_platform::unix::ipc::call_if_running(&path, &body, None)? {
        return Ok(reply);
    }
    if let Some(message) = decoded.as_ref().and_then(Request::ui_requirement) {
        anyhow::bail!(message);
    }
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
        let Some(Request::Layout(mut command)) = decoded else {
            anyhow::bail!("Invalid new tab request");
        };
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_client_requests_fail_before_daemon_fallback() {
        for body in [
            json!({"method":"send", "text":false}),
            json!({"method":"split", "pane":"invalid"}),
            json!({"method":"resize", "width":false, "height":600}),
        ] {
            assert!(client_request(&body).is_err());
        }
        assert!(client_request(&json!({"method":"stop"})).unwrap().is_none());
        assert!(
            client_request(&json!({"method":"scrollback", "connection":"server", "max_bytes":0}))
                .unwrap()
                .is_none()
        );
        assert!(client_request(&json!({"method":"scrollback", "connection":"server", "max_bytes":0, "max_lines":null})).is_err());
        assert!(
            client_request(&json!({"method":"scrollback", "connection":"server", "unknown":true}))
                .is_err()
        );
        assert!(
            client_request(&json!({"method":"read"}))
                .unwrap()
                .is_some_and(|request| request.uses_ui())
        );
    }
}
