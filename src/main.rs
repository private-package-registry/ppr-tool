fn main() {
    // Metadata is walked recursively (nuspec conversion, canonical JSON, serialisation) and may be nested as
    // deep as the registry allows; the main thread's stack is 1 MiB on Windows, so the work gets its own.
    let work = std::thread::Builder::new().stack_size(ppr_tool::WORK_STACK).spawn(|| ppr_tool::run(std::env::args_os()));
    let code = work.ok().and_then(|thread| thread.join().ok()).unwrap_or(1);
    std::process::exit(code);
}
