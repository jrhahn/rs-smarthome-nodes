fn main() {
    // The firmware bakes Wi-Fi credentials in at compile time via
    // `option_env!("SSID")` / `option_env!("PASSWORD")`. Without these hints,
    // cargo would not rebuild when the values change (e.g. after editing .env),
    // silently keeping stale credentials in the binary.
    // `NODE` picks which sensors/topics/power profile this image is built for
    // (see `src/node.rs`), so a changed value must force a rebuild too.
    println!("cargo:rerun-if-env-changed=NODE");
    println!("cargo:rerun-if-env-changed=MQTT_BROKER");
    println!("cargo:rerun-if-env-changed=SSID");
    println!("cargo:rerun-if-env-changed=PASSWORD");
    println!("cargo:rerun-if-env-changed=MQTT_USER");
    println!("cargo:rerun-if-env-changed=MQTT_PASSWORD");
}
