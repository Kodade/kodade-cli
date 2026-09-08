use std::{
    fs,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

#[test]
fn guide_is_local_even_with_remote_and_stale_inherited_context() {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let root = std::env::temp_dir().join(format!("kodade-guide-{}-{nonce}", std::process::id()));
    fs::create_dir_all(root.join(".config/kodade-cli")).unwrap();
    fs::write(
        root.join(".config/kodade-cli/config.toml"),
        "invalid toml ]",
    )
    .unwrap();
    let output = Command::new(env!("CARGO_BIN_EXE_kodade-cli"))
        .args(["--remote", "unreachable.invalid", "agent", "guide"])
        .env("HOME", &root)
        .env("USERPROFILE", &root)
        .env("XDG_RUNTIME_DIR", root.join("runtime"))
        .env("XDG_STATE_HOME", root.join("state"))
        .env("KODADE_SESSION", "invalid/session")
        .env("KODADE_SOCKET", root.join("missing.sock"))
        .env("PATH", &root)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stderr.is_empty());
    let guide = String::from_utf8(output.stdout).unwrap();
    assert!(guide.starts_with(concat!("Ködade CLI ", env!("CARGO_PKG_VERSION"))));
    assert!(guide.contains("automation guide 1"));
    assert!(guide.contains("--wait --timeout 120 --json"));
    assert!(!root.join("runtime").exists());
    assert!(!root.join("state").exists());
    fs::remove_dir_all(root).unwrap();
}
