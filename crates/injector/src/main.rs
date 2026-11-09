//! `bedrock-inject <dll> <process name>`: load a DLL into a running process.
//!
//! Exit codes: 2 usage, then those of [`bedrock_inject::inject`]: 3 process not found,
//! 4 OpenProcess failed, 5 writing the path failed, 6 the remote thread failed to start,
//! 7 LoadLibraryW returned null, 8 loading timed out.

#[cfg(windows)]
fn main() {
    let arguments: Vec<String> = std::env::args().skip(1).collect();
    let [dll, process] = arguments.as_slice() else {
        eprintln!("usage: bedrock-inject <dll path> <process name>");
        std::process::exit(2);
    };
    let result = bedrock_inject::find_process(process)
        .ok_or((3, format!("{process} is not running")))
        .and_then(|pid| bedrock_inject::inject(dll, pid).map(|()| pid));
    match result {
        Ok(pid) => println!("loaded {dll} into {process} (pid {pid})"),
        Err((code, message)) => {
            eprintln!("bedrock-inject: {message}");
            std::process::exit(code);
        }
    }
}

#[cfg(not(windows))]
fn main() {
    eprintln!("bedrock-inject runs only on Windows or Wine");
    std::process::exit(2);
}
