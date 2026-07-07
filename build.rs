fn main() {
    // The firmware bakes Wi-Fi credentials in at compile time via
    // `option_env!("SSID")` / `option_env!("PASSWORD")`. Without these hints,
    // cargo would not rebuild when the values change (e.g. after editing .env),
    // silently keeping stale credentials in the binary.
    println!("cargo:rerun-if-env-changed=SSID");
    println!("cargo:rerun-if-env-changed=PASSWORD");
}
