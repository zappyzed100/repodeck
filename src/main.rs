#![windows_subsystem = "windows"]

fn main() -> anyhow::Result<()> {
    // Always run with administrator rights so windows owned by elevated
    // processes can be managed. If not yet elevated, an elevated copy is
    // launched (UAC) and this instance exits before it touches the
    // single-instance mutex or any state.
    // Test-control mode (`REPODECK_TEST_CONTROL`) skips self-elevation so the
    // real-machine soak-test harness can stop/start RepoDeck between config
    // sizes without a UAC prompt each time. The test windows it drives are
    // medium-integrity, so a medium RepoDeck can manage them fine.
    if std::env::var_os("REPODECK_TEST_CONTROL").is_none()
        && repodeck::windowing::elevation::relaunch_as_admin_if_needed()
    {
        return Ok(());
    }
    repodeck::app::run()
}
