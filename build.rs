fn main() {
    // The firmware bakes Wi-Fi credentials in at compile time via
    // `option_env!("SSID")` / `option_env!("PASSWORD")`. Without these hints,
    // cargo would not rebuild when the values change (e.g. after editing .env),
    // silently keeping stale credentials in the binary.
    // `FLAVOR` picks the sensors and power profile, `NODE_ID` / `NODE_NAME` the
    // identity (see `src/node.rs`), so a changed value must force a rebuild too.
    println!("cargo:rerun-if-env-changed=FLAVOR");
    println!("cargo:rerun-if-env-changed=NODE_ID");
    println!("cargo:rerun-if-env-changed=NODE_NAME");
    println!("cargo:rerun-if-env-changed=NODE");

    // `NODE=kueche` is how every image was built until flavors replaced the
    // room table. Ignoring it would build a `climate` board that publishes as
    // `climate` -- a plausible image under the wrong topics, the one mistake
    // this build exists to refuse. Say what replaced it instead.
    if let Ok(node) = std::env::var("NODE") {
        panic!(
            "NODE={node} is gone: set FLAVOR=<flavor> NODE_ID={node} (and NODE_NAME), \
             or use the deployment's build script"
        );
    }
    println!("cargo:rerun-if-env-changed=MQTT_BROKER");
    println!("cargo:rerun-if-env-changed=SSID");
    println!("cargo:rerun-if-env-changed=PASSWORD");
    println!("cargo:rerun-if-env-changed=MQTT_USER");
    println!("cargo:rerun-if-env-changed=MQTT_PASSWORD");
    println!("cargo:rerun-if-env-changed=NTP_SERVER");

    // `FW_VERSION` is what the node publishes about itself and what an update
    // offer is matched against: `<id>-<release>-<commit>`, e.g.
    // `kueche-0.2.0-425e2c4`. The release is `version` from `Cargo.toml`, so
    // the string answers "which release is this board on" without a checkout;
    // the commit still names the exact build. The id half is not decoration --
    // `ota::is_for_node` refuses an image built for another board, which is the
    // over-the-air answer to the wrong image that ran on the outdoor board for
    // eleven hours on 2026-09-17. The id is `NODE_ID`, falling back to the
    // flavor, exactly as `node::BUILT_AS` resolves it.
    //
    // A working tree with uncommitted changes is marked, because an image whose
    // version names a commit it does not match is worse than one that admits it
    // is unidentified: the whole point is that the string answers "which code
    // is this?" without anyone having to remember.
    let node = std::env::var("NODE_ID")
        .or_else(|_| std::env::var("FLAVOR"))
        .unwrap_or_else(|_| "climate".into());
    let release = std::env::var("CARGO_PKG_VERSION").unwrap_or_default();
    println!("cargo:rustc-env=FW_VERSION={node}-{release}-{}", commit());
    // A new commit changes the version, so the stamp has to be re-evaluated.
    println!("cargo:rerun-if-changed=.git/HEAD");
    println!("cargo:rerun-if-changed=.git/index");
    println!("cargo:rerun-if-changed=Cargo.toml");
}

/// The short commit, plus `-dirty` when the tree does not match it -- last, next
/// to the commit it qualifies. `unknown`
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
