// The dashboard is embedded from ../dashboard/dist. Make sure the folder exists so the
// server compiles (and serves a hint page) before the dashboard has been built.
fn main() {
    let dist = std::path::Path::new("../dashboard/dist");
    if !dist.exists() {
        std::fs::create_dir_all(dist).expect("create dashboard/dist");
    }
    println!("cargo:rerun-if-changed=../dashboard/dist");
}
