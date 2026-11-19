fn main() {
    slint_build::compile("ui/app-window.slint").expect("Slint UI compilation failed");

    #[cfg(windows)]
    {
        let mut res = winresource::WindowsResource::new();
        res.set_icon("../../assets/icon.ico");
        res.compile().expect("failed to embed Windows resources");
    }
}
