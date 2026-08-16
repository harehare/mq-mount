fn main() {
    #[cfg(windows)]
    if std::env::var_os("CARGO_FEATURE_MOUNT").is_some() {
        winfsp::build::winfsp_link_delayload();
    }
}
