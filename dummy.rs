use hiero_sdk::{AccountId, Client, PrivateKey, Signature};
use std::str::FromStr;

fn main() {
    let pk = PrivateKey::from_str("0xd119fd7cf99e73bb82f20811fe54fd7976e715185d234fba588c4c46309ab2e1").unwrap();
    let sig = pk.sign(b"hello world");
    let sig_bytes = sig.to_bytes();
    println!("sig len: {}", sig_bytes.len());
}
