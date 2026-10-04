fn main() {
    let args = std::env::args_os().collect::<Vec<_>>();
    if args.get(1).is_some_and(|arg| arg == "__execution") {
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(args.len() == 4, "invalid private execution invocation");
            agl_daemon::run_execution_helper(
                args[2]
                    .to_str()
                    .ok_or_else(|| anyhow::anyhow!("helper mode is not UTF-8"))?,
                std::path::Path::new(&args[3]),
            )
        })();
        if let Err(error) = result {
            eprintln!("agl execution helper: {error:#}");
            std::process::exit(126);
        }
        return;
    }
    agl_cli::run_cli();
}
