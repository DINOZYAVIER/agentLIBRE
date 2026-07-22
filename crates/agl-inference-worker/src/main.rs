fn main() {
    #[cfg(target_os = "linux")]
    let status = if agl_inference_worker::run_from_inherited_channel().is_ok() {
        0
    } else {
        125
    };

    #[cfg(not(target_os = "linux"))]
    let status = 125;

    std::process::exit(status);
}
