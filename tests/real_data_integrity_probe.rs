//! 実機の config.json / runtime.json を読み、integrity_service が何を見つけるかを
//! 出力するだけの検査。既定では走らない (`#[ignore]`)。
//!
//! 実行: cargo test --test real_data_integrity_probe -- --ignored --nocapture

use repodeck::application::integrity_service;
use repodeck::persistence::{config_store, runtime_store};
use repodeck::windowing::enumerate;

#[test]
#[ignore = "実機のデータと現在開いているウィンドウに依存する"]
fn probe_the_real_machine() {
    let data_dir = repodeck::app::local_app_data_dir().expect("data dir");
    let config = config_store::load(&data_dir)
        .expect("config")
        .unwrap()
        .config;
    let mut runtime = runtime_store::load(&data_dir);
    let live = enumerate::enumerate_top_level_windows(std::process::id()).expect("windows");

    println!("live windows = {}", live.len());
    for w in &live {
        println!(
            "  hwnd={} class={} title={}",
            w.hwnd, w.window_class, w.title
        );
    }
    println!("bindings before = {}", runtime.window_bindings.len());

    let report =
        integrity_service::check_and_repair(&config.worksets, &live, &mut runtime.window_bindings);

    println!("\n--- report ---");
    println!(
        "dropped={} pruned={}",
        report.dropped_bindings(),
        report.pruned_stale()
    );
    for a in &report.anomalies {
        println!("{a:?}");
    }
    println!("bindings after = {}", runtime.window_bindings.len());
}
