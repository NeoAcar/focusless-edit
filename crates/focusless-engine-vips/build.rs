fn main() {
    #[cfg(target_os = "windows")]
    {
        println!("cargo:rustc-link-lib=libglib-2.0");
        println!("cargo:rustc-link-lib=libgobject-2.0");
    }
}
