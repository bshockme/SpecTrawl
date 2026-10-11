//! Lists the devices the Airspy backend can see.
//!
//!     cargo run --release -p engine --example list_devices

use sdr_core::Backend;

fn main() {
    let b = backend_airspy::AirspyBackend::new();
    match b.enumerate() {
        Ok(v) => {
            for d in v {
                println!("{}\t{}\t{}", d.backend, d.label, d.serial);
            }
        }
        Err(e) => println!("airspy: {e}"),
    }
}
