#![windows_subsystem = "windows"]

fn main() -> anyhow::Result<()> {
    // Always run with administrator rights so windows owned by elevated
    // processes can be managed. If not yet elevated, an elevated copy is
    // launched (UAC) and this instance exits before it touches the
    // single-instance mutex or any state.
    if repodeck::windowing::elevation::relaunch_as_admin_if_needed() {
        return Ok(());
    }
    repodeck::app::run()
}
