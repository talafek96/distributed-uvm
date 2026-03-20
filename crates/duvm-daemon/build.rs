fn main() {
    // Compile the C helper for userfaultfd ioctls
    // (Rust's variadic ioctl binding has ABI issues on aarch64)
    cc::Build::new()
        .file("src/uffd_helper.c")
        .compile("uffd_helper");
}
