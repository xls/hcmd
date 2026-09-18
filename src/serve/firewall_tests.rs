//! Finding the firewall from a laid-out machine, and reading ufw's rules.

use super::*;

/// A machine in a temp dir: `units`, `etc` and `run` under one root.
fn machine(tag: &str) -> (std::path::PathBuf, Places) {
    let root = std::env::temp_dir().join(format!("hcmd-firewall-{tag}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let places = Places {
        units: root.join("units"),
        etc: root.join("etc"),
        run: root.join("run"),
    };
    std::fs::create_dir_all(&places.units).expect("units");
    std::fs::create_dir_all(places.etc.join("ufw")).expect("etc");
    std::fs::create_dir_all(&places.run).expect("run");
    (root, places)
}

/// The rules ufw writes on an Omarchy desktop, with LocalSend and ssh open.
const OMARCHY_RULES: &str = "\
### tuple ### allow tcp 53317 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp --dport 53317 -j ACCEPT

### tuple ### limit tcp 22 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp --dport 22 -m conntrack --ctstate NEW -m recent --set
-A ufw-user-input -p tcp --dport 22 -j ufw-user-limit-accept

### tuple ### allow tcp 80,443 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp -m multiport --dports 80,443 -j ACCEPT

### tuple ### deny tcp 8081 0.0.0.0/0 any 0.0.0.0/0 in
-A ufw-user-input -p tcp --dport 8081 -j DROP
";

#[test]
fn ufw_rules_say_which_ports_are_open() {
    assert!(ufw_allows(OMARCHY_RULES, 53317), "a plain allow");
    assert!(
        ufw_allows(OMARCHY_RULES, 22),
        "a rate-limited allow still accepts"
    );
    assert!(ufw_allows(OMARCHY_RULES, 443), "one of a multiport list");
    assert!(!ufw_allows(OMARCHY_RULES, 8081), "a deny is not an allow");
    assert!(
        !ufw_allows(OMARCHY_RULES, 8080),
        "an unmentioned port is closed"
    );
}

#[test]
fn an_active_ufw_is_found_through_systemd_and_its_rules_are_read() {
    let (root, places) = machine("ufw");
    std::fs::write(places.units.join("invocation:ufw.service"), b"").expect("link");
    std::fs::write(places.etc.join("ufw/user.rules"), OMARCHY_RULES).expect("rules");
    assert_eq!(
        detect_in(&places, 8080),
        Some(Report {
            firewall: Firewall::Ufw,
            allowed: Some(false)
        })
    );
    assert_eq!(
        detect_in(&places, 53317),
        Some(Report {
            firewall: Firewall::Ufw,
            allowed: Some(true)
        })
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn ufw_enabled_in_its_conf_counts_even_without_systemd_and_unreadable_rules_are_unknown() {
    let (root, places) = machine("ufw-conf");
    std::fs::write(
        places.etc.join("ufw/ufw.conf"),
        b"# ufw\nENABLED=yes\nLOGLEVEL=low\n",
    )
    .expect("conf");
    assert_eq!(
        detect_in(&places, 8080),
        Some(Report {
            firewall: Firewall::Ufw,
            allowed: None
        })
    );
    std::fs::write(places.etc.join("ufw/ufw.conf"), b"ENABLED=no\n").expect("conf");
    assert_eq!(detect_in(&places, 8080), None, "ufw installed but off");
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn firewalld_nftables_and_iptables_are_found_and_ufw_wins_when_both_are_on() {
    let (root, places) = machine("others");
    std::fs::create_dir_all(places.run.join("firewalld")).expect("run dir");
    assert_eq!(
        detect_in(&places, 80).map(|r| r.firewall),
        Some(Firewall::Firewalld)
    );
    std::fs::remove_dir_all(places.run.join("firewalld")).expect("rm");
    std::fs::write(places.units.join("invocation:nftables.service"), b"").expect("link");
    assert_eq!(
        detect_in(&places, 80).map(|r| r.firewall),
        Some(Firewall::Nftables)
    );
    std::fs::remove_file(places.units.join("invocation:nftables.service")).expect("rm");
    std::fs::write(places.units.join("invocation:ip6tables.service"), b"").expect("link");
    assert_eq!(
        detect_in(&places, 80).map(|r| r.firewall),
        Some(Firewall::Iptables)
    );
    // ufw drives iptables underneath; it is the one to name.
    std::fs::write(places.units.join("invocation:ufw.service"), b"").expect("link");
    assert_eq!(
        detect_in(&places, 80).map(|r| r.firewall),
        Some(Firewall::Ufw)
    );
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn nothing_found_is_said_as_nothing_found() {
    let (root, places) = machine("none");
    assert_eq!(detect_in(&places, 8080), None);
    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn the_dialog_lines_name_the_firewall_and_show_the_command_only_when_needed() {
    let open = Report {
        firewall: Firewall::Ufw,
        allowed: Some(true),
    };
    assert_eq!(
        lines(Some(open), 8080),
        vec!["firewall: ufw is on and allows tcp 8080"]
    );
    let shut = Report {
        firewall: Firewall::Ufw,
        allowed: Some(false),
    };
    assert_eq!(
        lines(Some(shut), 8080),
        vec![
            "firewall: ufw is on and blocks tcp 8080",
            "  sudo ufw allow 8080/tcp"
        ]
    );
    let unknown = Report {
        firewall: Firewall::Firewalld,
        allowed: None,
    };
    assert_eq!(
        lines(Some(unknown), 8080),
        vec![
            "firewall: firewalld is on - to let tcp 8080 through:",
            "  sudo firewall-cmd --add-port=8080/tcp"
        ]
    );
    assert_eq!(
        Firewall::Iptables.allow_command(8080),
        "sudo iptables -I INPUT -p tcp --dport 8080 -j ACCEPT"
    );
    if cfg!(target_os = "linux") {
        assert_eq!(lines(None, 8080), vec!["firewall: none found"]);
    }
}
