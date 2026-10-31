//! Launcher stub. Packed as the PE image of a polyglot file, it is what
//! Windows runs: it reads its own executable, extracts the slice for this
//! host into `%TEMP%`, runs it with the same arguments and exits with its
//! exit code. The logic is host-agnostic, so it is tested on Unix too.

mod launch;

fn main() {
    #[cfg(windows)]
    launch::forward_ctrl_c_to_child();
    let code = launch::run_main_with(
        std::env::current_exe(),
        std::env::args_os().skip(1).collect(),
        &unibin_core::extract::current_host(),
        &std::env::temp_dir(),
        &mut std::io::stderr(),
    );
    std::process::exit(code);
}
