fn main() {
    // Compile C shim for libibverbs inline functions (ibv_post_send, ibv_poll_cq)
    cc::Build::new().file("src/shim.c").compile("duvm_ibv_shim");

    // Link against libibverbs and librdmacm
    println!("cargo:rustc-link-lib=ibverbs");
    println!("cargo:rustc-link-lib=rdmacm");
}
