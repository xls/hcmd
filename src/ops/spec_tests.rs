//! The options, from the config.

use super::*;

#[test]
fn options_follow_the_ops_config() {
    let mut cfg = OpsConfig {
        confirm_overwrite: false,
        ..OpsConfig::default()
    };
    let opts = JobOptions::from_config(&cfg);
    assert_eq!(opts.conflict, Some(ConflictChoice::Overwrite));
    cfg.confirm_overwrite = true;
    assert_eq!(JobOptions::from_config(&cfg).conflict, None);
}
