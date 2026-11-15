//! MCP contract: the real binary, a real stdio handshake, no database touched.
//!
//! The child gets a syntactically valid but unreachable DSN, so the process starts without a
//! Postgres and still must answer `initialize` and `tools/list` correctly.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};

use serde_json::{Value, json};

const UNREACHABLE_DSN: &str = "postgresql://contract:contract@127.0.0.1:1/none";

struct Client {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
}

impl Client {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_phylactery-mcp"))
            .env("PHYLACTERY_DSN", UNREACHABLE_DSN)
            .env("PHYLACTERY_SETTINGS", "/nonexistent/settings.json")
            .env("PHYLACTERY_LOG", "off")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn phylactery-mcp");
        let stdin = child.stdin.take().expect("stdin");
        let stdout = BufReader::new(child.stdout.take().expect("stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, payload: Value) {
        self.stdin
            .write_all(format!("{payload}\n").as_bytes())
            .expect("write");
        self.stdin.flush().expect("flush");
    }

    fn read(&mut self) -> Value {
        let mut line = String::new();
        self.stdout.read_line(&mut line).expect("read");
        assert!(!line.trim().is_empty(), "empty response");
        serde_json::from_str(&line).expect("response is JSON")
    }

    fn initialize(&mut self) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-03-26",
                "capabilities": {},
                "clientInfo": { "name": "contract", "version": "0" }
            }
        }));
        let response = self.read();
        self.send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }

    fn tools(&mut self) -> Vec<Value> {
        self.send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}));
        self.read()["result"]["tools"]
            .as_array()
            .expect("tools array")
            .clone()
    }

    fn call(&mut self, name: &str, arguments: Value) -> Value {
        self.send(json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments }
        }));
        self.read()["result"].clone()
    }

    fn stop(mut self) {
        drop(self.stdin);
        let status = self.child.wait().expect("wait");
        assert!(status.success(), "server exited with {status}");
    }
}

#[test]
fn handshake_reports_the_phylactery_identity() {
    let mut client = Client::start();
    let init = client.initialize();
    assert_eq!(init["result"]["serverInfo"]["name"], "phylactery");
    assert_eq!(init["result"]["capabilities"]["tools"], json!({}));
    assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
    client.stop();
}

#[test]
fn tool_surface_is_exactly_the_documented_tools() {
    let mut client = Client::start();
    client.initialize();
    let mut names: Vec<String> = client
        .tools()
        .iter()
        .map(|tool| tool["name"].as_str().unwrap_or_default().to_string())
        .collect();
    names.sort();
    assert_eq!(
        names,
        [
            "adopt",
            "agents",
            "ask",
            "backfill",
            "backup",
            "conflicts",
            "diary",
            "doctor",
            "entity",
            "extract",
            "forget",
            "get",
            "history",
            "lesson",
            "list",
            "maintain",
            "manifest",
            "merge",
            "migrate",
            "mind_log",
            "mind_tail",
            "patch",
            "recall",
            "reembed",
            "related",
            "remember",
            "replace",
            "save",
            "search",
            "stats",
            "verify"
        ]
    );
    client.stop();
}

#[test]
fn every_tool_carries_a_description_and_a_schema() {
    let mut client = Client::start();
    client.initialize();
    for tool in client.tools() {
        let name = tool["name"].as_str().unwrap_or_default();
        let description = tool["description"].as_str().unwrap_or_default();
        assert!(!description.is_empty(), "tool {name} has no description");
        assert!(
            tool["inputSchema"].is_object(),
            "tool {name} has no input schema"
        );
    }
    client.stop();
}

#[test]
fn search_and_save_declare_their_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let schema_of = |name: &str| -> Value {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| tool["inputSchema"].clone())
            .unwrap_or(Value::Null)
    };

    let search = schema_of("search");
    let required = search["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["query"]);
    assert!(search["properties"]["limit"].is_object());

    let save = schema_of("save");
    assert!(save["properties"]["text"].is_object());
    assert!(save["properties"]["importance"].is_object());

    client.stop();
}

/// Pull the `code` out of an `isError` tool result.
fn refusal_code(result: &Value) -> String {
    assert_eq!(result["isError"], true, "not a refusal: {result}");
    let text = result["content"][0]["text"].as_str().unwrap_or_default();
    let body: Value = serde_json::from_str(text).expect("refusal body is JSON");
    body["code"].as_str().unwrap_or_default().to_string()
}

#[test]
fn get_declares_its_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let get = tools
        .iter()
        .find(|tool| tool["name"] == "get")
        .expect("get is registered");
    let required = get["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["id"]);
    assert!(get["inputSchema"]["properties"]["table"].is_object());
    client.stop();
}

#[test]
fn get_refuses_bad_input_before_touching_the_database() {
    let mut client = Client::start();
    client.initialize();
    let bad_id = client.call("get", json!({ "id": "not-an-id!" }));
    assert_eq!(refusal_code(&bad_id), "invalid_id");
    let bad_table = client.call("get", json!({ "id": "d7d4f4cc", "table": "pg_catalog" }));
    assert_eq!(refusal_code(&bad_table), "unsupported_table");
    client.stop();
}

#[test]
fn forget_declares_the_general_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let forget = tools
        .iter()
        .find(|tool| tool["name"] == "forget")
        .expect("forget is registered");
    let required = forget["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["id"], "table is optional now");
    for property in ["table", "agent_id", "dry_run", "confirm", "force"] {
        assert!(
            forget["inputSchema"]["properties"][property].is_object(),
            "forget lacks {property}"
        );
    }
    client.stop();
}

#[test]
fn forget_refuses_before_touching_the_database() {
    let mut client = Client::start();
    client.initialize();
    let bad = client.call("forget", json!({ "id": "not-a-uuid" }));
    assert_eq!(refusal_code(&bad), "invalid_id");
    let mind = client.call(
        "forget",
        json!({ "id": "00000000-0000-0000-0000-000000000000", "table": "mind" }),
    );
    assert_eq!(refusal_code(&mind), "append_only");
    client.stop();
}

#[test]
fn agents_and_list_declare_their_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let schema_of = |name: &str| -> Value {
        tools
            .iter()
            .find(|tool| tool["name"] == name)
            .map(|tool| tool["inputSchema"].clone())
            .unwrap_or(Value::Null)
    };
    let agents = schema_of("agents");
    assert!(agents["properties"]["include_empty"].is_object());
    assert!(agents["required"].as_array().is_none_or(Vec::is_empty));
    let list = schema_of("list");
    for property in [
        "circuit",
        "agent_id",
        "register",
        "importance_min",
        "since",
        "until",
        "has_vector",
        "episode_id",
        "thread_id",
        "order",
        "limit",
        "cursor",
    ] {
        assert!(
            list["properties"][property].is_object(),
            "list lacks {property}"
        );
    }
    assert!(list["required"].as_array().is_none_or(Vec::is_empty));
    client.stop();
}

#[test]
fn list_refuses_bad_filters_before_touching_the_database() {
    let mut client = Client::start();
    client.initialize();
    let cases = [
        (json!({ "circuit": "pg_catalog" }), "unsupported_table"),
        (json!({ "order": "random" }), "invalid_value"),
        (json!({ "since": "yesterday" }), "invalid_value"),
        (json!({ "cursor": "nope" }), "invalid_cursor"),
    ];
    for (arguments, code) in cases {
        let result = client.call("list", arguments.clone());
        assert_eq!(refusal_code(&result), code, "{arguments}");
    }
    client.stop();
}

#[test]
fn patch_declares_its_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let patch = tools
        .iter()
        .find(|tool| tool["name"] == "patch")
        .expect("patch is registered");
    let required = patch["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["id", "table"]);
    for property in [
        "importance",
        "memory_type",
        "source",
        "episode_id",
        "metadata",
    ] {
        assert!(
            patch["inputSchema"]["properties"][property].is_object(),
            "patch lacks {property}"
        );
    }
    client.stop();
}

#[test]
fn adopt_declares_its_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let adopt = tools
        .iter()
        .find(|tool| tool["name"] == "adopt")
        .expect("adopt is registered");
    let required = adopt["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["from_agent", "to_agent"]);
    for property in ["ids", "filter", "dry_run", "confirm"] {
        assert!(
            adopt["inputSchema"]["properties"][property].is_object(),
            "adopt lacks {property}"
        );
    }
    assert_eq!(
        adopt["inputSchema"]["properties"]["dry_run"]["default"], true,
        "dry_run must default to true"
    );
    client.stop();
}

#[test]
fn merge_declares_its_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let merge = tools
        .iter()
        .find(|tool| tool["name"] == "merge")
        .expect("merge is registered");
    let required = merge["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["keep_id", "drop_id", "table"]);
    for property in ["strategy", "dry_run", "confirm"] {
        assert!(
            merge["inputSchema"]["properties"][property].is_object(),
            "merge lacks {property}"
        );
    }
    assert_eq!(
        merge["inputSchema"]["properties"]["dry_run"]["default"], true,
        "dry_run must default to true"
    );
    assert_eq!(
        merge["inputSchema"]["properties"]["strategy"]["default"], "keep_text",
        "strategy must default to keep_text"
    );
    client.stop();
}

#[test]
fn history_declares_its_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let history = tools
        .iter()
        .find(|tool| tool["name"] == "history")
        .expect("history is registered");
    let required = history["inputSchema"]["required"]
        .as_array()
        .cloned()
        .unwrap_or_default()
        .iter()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect::<Vec<_>>();
    assert_eq!(required, ["id", "table"]);
    assert!(history["inputSchema"]["properties"]["limit"].is_object());
    assert_eq!(
        history["inputSchema"]["properties"]["limit"]["default"], 20,
        "limit must default to 20"
    );
    client.stop();
}

#[test]
fn reembed_declares_its_arguments() {
    let mut client = Client::start();
    client.initialize();
    let tools = client.tools();
    let reembed = tools
        .iter()
        .find(|tool| tool["name"] == "reembed")
        .expect("reembed is registered");
    assert!(
        reembed["inputSchema"]["required"]
            .as_array()
            .is_none_or(Vec::is_empty),
        "every reembed argument is optional"
    );
    for property in ["model", "circuit", "batch", "dry_run", "force"] {
        assert!(
            reembed["inputSchema"]["properties"][property].is_object(),
            "reembed lacks {property}"
        );
    }
    assert_eq!(
        reembed["inputSchema"]["properties"]["dry_run"]["default"], true,
        "dry_run must default to true"
    );
    assert_eq!(
        reembed["inputSchema"]["properties"]["batch"]["default"], 32,
        "batch must default to 32"
    );
    client.stop();
}
