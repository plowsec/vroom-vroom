#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
mod guest;

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
fn main() {
    guest::main();
}
#[cfg(not(all(target_os = "linux", target_arch = "x86_64")))]
fn main() {
    eprintln!("reference init supports Linux x86_64 only");
    std::process::exit(2);
}
