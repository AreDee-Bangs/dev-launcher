fn main() {
    // Short commit SHA of the current HEAD. Falls back to "unknown".
    let git_sha = std::process::Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".to_string());
    println!("cargo:rustc-env=GIT_SHA={git_sha}");

    // Remote origin URL — used at runtime to clone/pull the recipe store.
    let git_origin = std::process::Command::new("git")
        .args(["remote", "get-url", "origin"])
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_default();
    println!("cargo:rustc-env=GIT_ORIGIN_URL={git_origin}");

    let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap();
    println!("cargo:rustc-env=RECIPES_EMBEDDED_DIR={manifest_dir}/recipes");

    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=recipes/");
    println!("cargo:rerun-if-changed=.git/config");
}
