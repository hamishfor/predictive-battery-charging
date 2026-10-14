//! Calls to pbcd over D-Bus, through busctl so pbcctl needs no D-Bus crate.

use anyhow::{bail, Context, Result};

const DAEMON: &str = "org.pbc.Daemon1";
const PATH: &str = "/org/pbc/Daemon1";

/// Run `busctl --json=short <verb> pbcd <member> <args>` and parse its JSON output.
fn busctl(user_bus: bool, verb: &str, member: &str, args: &[&str]) -> Result<serde_json::Value> {
    let mut c = std::process::Command::new("busctl");
    if user_bus {
        c.arg("--user");
    }
    c.args(["--json=short", verb, DAEMON, PATH, DAEMON, member]);
    c.args(args);
    let out = c.output().context("running busctl")?;
    if !out.status.success() {
        bail!("pbcd: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if out.stdout.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    Ok(serde_json::from_slice(&out.stdout)?)
}

/// Call a pbcd method. Returns the first string reply, or "" for methods with none.
/// `args` is a busctl signature followed by values, e.g. ["sb", "quiet_wake", "true"].
pub fn call(user_bus: bool, method: &str, args: &[&str]) -> Result<String> {
    // {"type":"s","data":["<json>"]}
    let v = busctl(user_bus, "call", method, args)?;
    Ok(v["data"][0].as_str().unwrap_or_default().to_string())
}

/// Read a string property of pbcd, such as "Version".
pub fn property(user_bus: bool, name: &str) -> Result<String> {
    // {"type":"s","data":"<value>"}
    let v = busctl(user_bus, "get-property", name, &[])?;
    Ok(v["data"].as_str().unwrap_or_default().to_string())
}

pub fn status(user_bus: bool) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&call(user_bus, "StatusJson", &[])?)?)
}
