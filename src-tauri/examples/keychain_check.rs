//! Diagnostic: round-trips a throwaway item through the login Keychain using
//! the exact code the app uses on sign-in.
//!
//!     cd src-tauri
//!     cargo run --example keychain_check
//!
//! Nothing secret is written: the item is created, read back and deleted.

fn main() {
    let report = printables_offline::auth::keychain_selftest();

    println!("service : {}", report.service);
    println!("available: {}", report.available);
    println!("writable : {}", report.writable);
    println!("readable : {}", report.readable);
    if let Some(err) = &report.error {
        println!("error    : {err}");
    }

    if report.healthy() {
        println!("\nOK — this build may store the Printables session in the login Keychain.");
    } else {
        println!("\nNOT OK — the app will fall back to a 0600 file and say so in Preferences.");
        std::process::exit(1);
    }
}
