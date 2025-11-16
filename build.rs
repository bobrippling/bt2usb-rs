fn main() {
    println!("cargo::rerun-if-changed=memory.x");
    println!("cargo::rerun-if-env-changed=DEFMT_LOG");

    //panic!("build.rs, DEFMT_LOG is {:?}", std::env::var("DEFMT_LOG"));
}
