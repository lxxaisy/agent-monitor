#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    if !agent_monitor_lib::cli() {
        agent_monitor_lib::run();
    }
}
