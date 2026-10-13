//! Calls to pbcd over D-Bus, through busctl so pbcctl needs no D-Bus crate.

use anyhow::{bail, Context, Result};

/// Call a pbcd method. Returns the first string reply, or "" for methods with none.
/// `args` is a busctl signature followed by values, e.g. ["sb", "quiet_wake", "true"].
pub fn call(user_bus: bool, method: &str, args: &[&str]) -> Result<String> {
    let mut c = std::process::Command::new("busctl");
    if user_bus {
        c.arg("--user");
    }
    c.args([
        "--json=short",
        "call",
        "org.pbc.Daemon1",
        "/org/pbc/Daemon1",
        "org.pbc.Daemon1",
        method,
    ]);
    c.args(args);
    let out = c.output().context("running busctl")?;
    if !out.status.success() {
        bail!("pbcd: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if out.stdout.is_empty() {
        return Ok(String::new());
    }
    // busctl --json gives {"type":"s","data":["<json>"]}
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(v["data"][0].as_str().unwrap_or_default().to_string())
}

/// Read a string property of pbcd, such as "Version".
pub fn property(user_bus: bool, name: &str) -> Result<String> {
    let mut c = std::process::Command::new("busctl");
    if user_bus {
        c.arg("--user");
    }
    c.args([
        "--json=short",
        "get-property",
        "org.pbc.Daemon1",
        "/org/pbc/Daemon1",
        "org.pbc.Daemon1",
        name,
    ]);
    let out = c.output().context("running busctl")?;
    if !out.status.success() {
        bail!("{}", String::from_utf8_lossy(&out.stderr).trim());
    }
    // busctl --json gives {"type":"s","data":"<value>"}
    let v: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    Ok(v["data"].as_str().unwrap_or_default().to_string())
}

pub fn status(user_bus: bool) -> Result<serde_json::Value> {
    Ok(serde_json::from_str(&call(user_bus, "StatusJson", &[])?)?)
}
