//! cli utilities
use std::io::Write;

use serde::Deserialize;
use termcolor::{Color, ColorChoice, ColorSpec, StandardStream, WriteColor};
use wasmcloud_interface_testing::TestResult;

// structure for deserializing error results
#[derive(Deserialize)]
struct ErrorReport {
    error: String,
}

/// Prints test results (with handy color!) to the terminal
// NOTE(thomastaylor312): We are unwrapping all writing IO errors (which matches the behavior in the
// println! macro) and swallowing the color change errors as there isn't much we can do if they fail
// (and a color change isn't the end of the world). We may want to update this function in the
// future to return an io::Result
pub fn print_test_results(results: &[TestResult]) {
    let mut passed = 0u32;
    let total = results.len() as u32;
    // TODO(thomastaylor312): We can probably improve this a bit by using the `atty` crate to choose
    // whether or not to colorize the text
    let mut stdout = StandardStream::stdout(ColorChoice::Always);
    let mut green = ColorSpec::new();
    green.set_fg(Some(Color::Green));
    let mut red = ColorSpec::new();
    red.set_fg(Some(Color::Red));
    for test in results.iter() {
        if test.passed {
            let _ = stdout.set_color(&green);
            write!(&mut stdout, "Pass").unwrap();
            let _ = stdout.reset();
            writeln!(&mut stdout, ": {}", test.name).unwrap();
            passed += 1;
        } else {
            let error_msg = test
                .snap_data
                .as_ref()
                .map(|bytes| {
                    serde_json::from_slice::<ErrorReport>(bytes)
                        .map(|r| r.error)
                        .unwrap_or_default()
                })
                .unwrap_or_default();
            let _ = stdout.set_color(&red);
            write!(&mut stdout, "Fail").unwrap();
            let _ = stdout.reset();
            writeln!(&mut stdout, ": {}", error_msg).unwrap();
        }
    }
    let status_color = if passed == total { green } else { red };
    write!(&mut stdout, "Test results: ").unwrap();
    let _ = stdout.set_color(&status_color);
    writeln!(&mut stdout, "{}/{} Passed", passed, total).unwrap();
    // Reset the color settings back to what the user configured
    let _ = stdout.set_color(&ColorSpec::new());
    writeln!(&mut stdout, "").unwrap();
}
