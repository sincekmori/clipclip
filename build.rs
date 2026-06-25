use std::env;

fn main() {
    // Only relevant when the `opus` feature pulls in libopusenc + vendored
    // libopus. Without it this build script does nothing.
    if env::var_os("CARGO_FEATURE_OPUS").is_none() {
        return;
    }

    // libopusenc statically references libopus, and cargo's default static-lib
    // bundling links the two archives in an order single-pass linkers can't
    // resolve ("undefined reference" to `ope_*` / `opus_*`). Force both to be
    // linked with `+whole-archive`, so every member — and thus every symbol — is
    // pulled in regardless of order.
    //
    // Crucially, `rustc-link-lib` directives PROPAGATE to downstream binaries
    // (unlike `rustc-link-arg`, which only affects this crate's own targets), so
    // a project that depends on clipclip and enables the `opus` feature links
    // without any extra build-script or `.cargo/config.toml` steps.
    println!("cargo:rustc-link-lib=static:+whole-archive=opusenc");
    println!("cargo:rustc-link-lib=static:+whole-archive=opus");
}
