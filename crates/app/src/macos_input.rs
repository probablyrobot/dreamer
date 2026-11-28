//! macOS: keys held down repeat, as in every terminal.
//!
//! By default macOS shows the accent picker when a letter key is held
//! (press and hold), instead of repeating it. A terminal wants the repeat,
//! so Dreamer turns press and hold off for itself, the way iTerm, kitty
//! and winit do: `ApplePressAndHoldEnabled = NO` in the registration
//! domain of its own user defaults. That domain lives only in this
//! process and is consulted last, so nothing is written to disk and a
//! value the user sets for Dreamer (`defaults write <id>
//! ApplePressAndHoldEnabled -bool YES`) still wins.
//!
//! It must run before the first window takes key input.

#[allow(unsafe_code)]
pub(crate) fn repeat_held_keys() {
    use objc2::runtime::AnyObject;
    use objc2_foundation::{NSDictionary, NSNumber, NSString, NSUserDefaults};

    let key = NSString::from_str("ApplePressAndHoldEnabled");
    let no = NSNumber::new_bool(false);
    let value: &AnyObject = &no;
    let defaults = NSDictionary::<NSString, AnyObject>::from_slices(&[&*key], &[value]);
    // SAFETY: the dictionary maps an NSString key to an NSNumber, the
    // property-list types `registerDefaults:` requires.
    unsafe {
        NSUserDefaults::standardUserDefaults().registerDefaults(&defaults);
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn press_and_hold_is_off_for_dreamer() {
        super::repeat_held_keys();
        let key = objc2_foundation::NSString::from_str("ApplePressAndHoldEnabled");
        let defaults = objc2_foundation::NSUserDefaults::standardUserDefaults();
        // A value set for the test runner itself would win; nothing sets one.
        assert!(!defaults.boolForKey(&key), "held keys must repeat");
        assert!(defaults.objectForKey(&key).is_some(), "registered");
    }
}
