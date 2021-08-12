pub mod provider_test;


#[macro_export]
macro_rules! check_eq {
    ($left:expr, $right:expr $(,)?) => ({
        match (&$left, &$right) {
            (left_val, right_val) => {
                if (left_val == right_val) {
                    Ok(true)
                } else {
                    Err(format!("check failed: `check_eq(left, right)`\n  left: `{:?}`\n right: `{:?}` {}:{}", $left, $right, std::file!(), std::line!{}))
                }
            }
        }
    });
}

#[macro_export]
macro_rules! check {
    ( $val:expr ) => ({
        if ($val) {
            Ok(true)
        } else {
            Err(format!("check failed: `{:?}' {}:{}", $val, std::file!(), std::line!{}))
        }
    });
}
