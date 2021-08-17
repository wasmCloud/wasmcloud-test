//! cli utilities
#[cfg(not(target_arch = "wasm32"))]

use termion::{color, style};
use wasmcloud_interface_testing::{TestResult};
use serde::Deserialize;

// structure for deserializing error results
#[derive(Deserialize)]
struct ErrorReport {
    error: String,
}

/// print test results to console
pub fn print_test_results(results: &Vec<TestResult>) {
    let mut passed = 0u32;
    let total = results.len() as u32;
    for test in results.iter() {
        match test.pass {
            true => {
                println!(
                    "{} Pass {}: {}",
                    color::Fg(color::Green),
                    style::Reset,
                    &test.name
                );
                passed += 1;
            }
            false => {
                let error_msg = if let Some(bytes) = &test.snap_data {
                    if let Ok(report) = serde_json::from_slice::<ErrorReport>(&bytes) {
                        report.error.to_string()
                    } else {
                        "".to_string()
                    }
                } else {
                    "".to_string()
                };
                println!(
                    "{} Fail {}: {}  {}",
                    color::Fg(color::Red),
                    style::Reset,
                    &test.name,
                    &error_msg,
                );
            }
        }
    }
    let status_color = if passed == total {
        color::Fg(color::Green).to_string()
    } else {
        color::Fg(color::Red).to_string()
    };
    println!(
        "Test results: {}{}/{} Passed{}",
        status_color,
        passed,
        total,
        style::Reset
    );
}
