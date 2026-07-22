//! Spawns a [`LaunchSpec`]'s process to reopen a closed managed-window app
//! (PLAN.md §5.3 extension). The decision of *what* to launch is the pure
//! [`crate::application::launch_service`]; this module only performs the spawn.

use std::process::Command;

use crate::domain::workset::LaunchSpec;

/// Launches `spec`'s program with its args, fully detached from RepoDeck. The
/// spawned child is intentionally not waited on — it's a normal desktop app
/// with its own lifetime, and RepoDeck re-adopts its window by matching, not
/// by owning the process.
pub fn launch(spec: &LaunchSpec) -> std::io::Result<()> {
    let mut command = Command::new(&spec.program);
    command.args(&spec.args);
    // Detach: don't inherit RepoDeck's stdio, and let the child outlive us.
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // Spawn and immediately drop the handle; the child keeps running.
    let _child = command.spawn()?;
    Ok(())
}
