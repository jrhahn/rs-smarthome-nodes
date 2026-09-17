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
    println!("cargo:rerun-if-env-changed=NTP_SERVER");

    // `FW_VERSION` is what the node publishes about itself and what an update
    // offer is matched against: `<node>-<commit>`, e.g. `kueche-425e2c4`. The
    // node half is not decoration -- `ota::is_for_node` refuses an image built
    // for another room, which is the over-the-air answer to the wrong image
    // that ran on the terrasse board for eleven hours on 2026-09-17.
    //
    // A working tree with uncommitted changes is marked, because an image whose
    // version names a commit it does not match is worse than one that admits it
    // is unidentified: the whole point is that the string answers "which code
    // is this?" without anyone having to remember.
    let node = std::env::var("NODE").unwrap_or_else(|_| "terrasse".into());
    println!("cargo:rustc-env=FW_VERSION={node}-{}", commit());
    // A new commit changes the version, so the stamp has to be re-evaluated.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
}

/// The short commit, plus `-dirty` when the tree does not match it. `unknown`
/// when there is no git to ask -- a tarball build, say, which is not something
/// to fail over.
fn commit() -> String {
    let Some(sha) = git(&["rev-parse", "--short=7", "HEAD"]) else {
        return "unknown".into();
    };
    match git(&["status", "--porcelain", "--untracked-files=no"]) {
        Some(changes) if !changes.is_empty() => format!("{sha}-dirty"),
        _ => sha,
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}
