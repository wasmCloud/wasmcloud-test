#[cfg(not(target_arch = "wasm32"))]
pub mod provider_test;

#[cfg(not(target_arch = "wasm32"))]
pub mod cli;

// re-export testing interface
pub use wasmcloud_interface_testing as testing;

// re-export regex and nkeys
pub use nkeys;
pub use regex;

// these macros are supported on all build targets (wasm32 et. al.)

/// check that the two expressions are equal, returning RpcError if they are not
#[macro_export]
macro_rules! check_eq {
    ($left:expr, $right:expr $(,)?) => {{
        match (&$left, &$right) {
            (left_val, right_val) => {
                if (left_val == right_val) {
                    Ok(true)
                } else {
                    Err(format!(
                        "check failed: `check_eq(left, right)`\n  left: `{:?}`\n right: `{:?}` \
                         {}:{}",
                        $left,
                        $right,
                        std::file!(),
                        std::line! {}
                    ))
                }
            }
        }
    }};
}

/// check that the condition is true, returning RpcError if it is false
#[macro_export]
macro_rules! check {
    ( $val:expr ) => {{
        if ($val) {
            Ok(true)
        } else {
            Err(format!(
                "check failed: `{:?}' {}:{}",
                $val,
                std::file!(),
                std::line! {}
            ))
        }
    }};
}

/// given a list of regex patterns for test cases, run all tests
/// that match any of the patterns.
/// The order of the test runs is based on the order of patterns.
/// Tests are run at most once, even if they match more than one pattern.
#[macro_export]
macro_rules! run_selected {
    ( $opt:expr, $($tname:ident),* $(,)? ) => {{
        let mut unique = std::collections::BTreeSet::new();
        let all_tests = vec![".*".to_string()];
        let pats : &Vec<String> = $opt.patterns.as_ref();
        let mut results: Vec<TestResult> = Vec::new();

        // Each test case regex (pats) is checked against all test names (tname).
        // This would be simpler to use a RegexSet, but then the tests would
        // always be run in the same order - these run in the order of the patterns.
        for tc_exp in pats.iter() {
            let pattern = tc_exp.as_str();
            let re = match $crate::regex::Regex::new(pattern) {
                Ok(re) => re,
                Err(e) => {
                    let error = RpcError::Other(format!(
                        "invalid regex spec '{}' for test case: {}",
                        pattern, e
                    ));
                    results.push(("TestCase", Err::<(),RpcError>(error)).into());
                    break;
                }
            };
            $(
            let name = stringify!($tname);
            if re.is_match(name) {
                // run it if it hasn't been run before
                if unique.insert(name) {
                    let tr:TestResult = (name, $tname($opt).await).into();
                    results.push(tr);
                }
            }
            )*
        }
        results
    }};
}
