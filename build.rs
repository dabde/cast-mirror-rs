fn main() {
    // screencapturekit's Swift bridge loads the runtime from here; a dependency's link args don't reach the binary.
    println!("cargo:rustc-link-arg=-Wl,-rpath,/usr/lib/swift");
}
