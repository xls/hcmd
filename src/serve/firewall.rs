//! Which host firewall is on, and what lets the share's port through it.
//!
//! The one reason a share is unreachable from the next machine, nearly every
//! time, is the host's own firewall: a desktop such as Omarchy ships ufw with
//! everything inbound denied. The dialog says so, names the firewall, and
//! shows the one command that opens the port - so the answer is on screen
//! rather than in a search.
//!
//! Nothing here runs a program. systemd links every active unit at
//! `/run/systemd/units/invocation:<unit>`, ufw writes its rules to a
//! world-readable file, firewalld keeps a runtime directory; that is enough
//! to know what is on and, for ufw, whether the port is already allowed. On
//! a system where none of that exists the answer is "not checked", never a
//! guess.

/// The firewalls recognised, in the order they are looked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Firewall {
    /// Uncomplicated Firewall: Ubuntu, Omarchy and most desktop Arch setups.
    Ufw,
    /// firewalld: Fedora, RHEL, openSUSE.
    Firewalld,
    /// A bare nftables service with its own ruleset.
    Nftables,
    /// A bare iptables service with its own ruleset.
    Iptables,
}

impl Firewall {
    /// The name as its users write it.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Ufw => "ufw",
            Self::Firewalld => "firewalld",
            Self::Nftables => "nftables",
            Self::Iptables => "iptables",
        }
    }

    /// What opens tcp `port`, typed as the user would type it.
    #[must_use]
    pub fn allow_command(self, port: u16) -> String {
        match self {
            Self::Ufw => format!("sudo ufw allow {port}/tcp"),
            Self::Firewalld => format!("sudo firewall-cmd --add-port={port}/tcp"),
            Self::Nftables => format!("add an accept rule for tcp dport {port} to the input chain"),
            Self::Iptables => format!("sudo iptables -I INPUT -p tcp --dport {port} -j ACCEPT"),
        }
    }
}

/// What was found: which firewall, and whether the port is known to be let
/// through it - `None` when that cannot be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Report {
    /// The firewall that is on.
    pub firewall: Firewall,
    /// Whether tcp `port` is already allowed, when the rules can be read.
    pub allowed: Option<bool>,
}

/// Where the looks are made: the systemd runtime directory and `/etc`.
/// Parameters so a test can lay out a machine in a temp dir.
#[derive(Debug, Clone)]
pub struct Places {
    /// `/run/systemd/units`.
    pub units: std::path::PathBuf,
    /// `/etc`.
    pub etc: std::path::PathBuf,
    /// `/run`.
    pub run: std::path::PathBuf,
}

impl Default for Places {
    fn default() -> Self {
        Self {
            units: "/run/systemd/units".into(),
            etc: "/etc".into(),
            run: "/run".into(),
        }
    }
}

/// The firewall that is on, looked for on this machine, or `None` when none
/// is found or this is not a system the look works on.
#[must_use]
pub fn detect(port: u16) -> Option<Report> {
    detect_in(&Places::default(), port)
}

/// [`detect`] against `places`.
#[must_use]
pub fn detect_in(places: &Places, port: u16) -> Option<Report> {
    let unit_active = |unit: &str| places.units.join(format!("invocation:{unit}")).exists();
    let ufw_conf = places.etc.join("ufw/ufw.conf");
    let ufw_on = unit_active("ufw.service")
        || std::fs::read_to_string(&ufw_conf)
            .map(|conf| conf.lines().any(|l| l.trim() == "ENABLED=yes"))
            .unwrap_or(false);
    if ufw_on {
        let allowed = std::fs::read_to_string(places.etc.join("ufw/user.rules"))
            .ok()
            .map(|rules| ufw_allows(&rules, port));
        return Some(Report {
            firewall: Firewall::Ufw,
            allowed,
        });
    }
    if unit_active("firewalld.service") || places.run.join("firewalld").is_dir() {
        return Some(Report {
            firewall: Firewall::Firewalld,
            allowed: None,
        });
    }
    if unit_active("nftables.service") {
        return Some(Report {
            firewall: Firewall::Nftables,
            allowed: None,
        });
    }
    if unit_active("iptables.service") || unit_active("ip6tables.service") {
        return Some(Report {
            firewall: Firewall::Iptables,
            allowed: None,
        });
    }
    None
}

/// Whether ufw's rules file accepts tcp on `port` inbound.
///
/// The file is the one ufw writes for `ufw allow`: a rule per line in
/// iptables syntax on the `ufw-user-input` chain. A single `--dport` and a
/// `--dports` list are both read; a port range is not, and reads as not
/// allowed, which errs towards showing the command.
#[must_use]
pub fn ufw_allows(rules: &str, port: u16) -> bool {
    let wanted = port.to_string();
    rules
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("-A ufw-user-input"))
        .filter(|line| line.contains("-p tcp") || !line.contains("-p "))
        .filter(|line| line.ends_with("-j ACCEPT") || line.ends_with("-j ufw-user-limit-accept"))
        .any(|line| {
            let words: Vec<&str> = line.split_whitespace().collect();
            words.windows(2).any(|pair| {
                matches!(pair, [flag, ports] if (*flag == "--dport" || *flag == "--dports")
                    && ports.split(',').any(|p| p == wanted))
            })
        })
}

/// The lines the dialog shows for `report` on `port`: what is on, and what
/// to type when the port is not known to be open.
#[must_use]
pub fn lines(report: Option<Report>, port: u16) -> Vec<String> {
    let Some(report) = report else {
        return vec![if cfg!(target_os = "linux") {
            "firewall: none found".to_string()
        } else {
            format!("firewall: not checked here - allow tcp {port} if the LAN cannot connect")
        }];
    };
    let name = report.firewall.name();
    match report.allowed {
        Some(true) => vec![format!("firewall: {name} is on and allows tcp {port}")],
        Some(false) => vec![
            format!("firewall: {name} is on and blocks tcp {port}"),
            format!("  {}", report.firewall.allow_command(port)),
        ],
        None => vec![
            format!("firewall: {name} is on - to let tcp {port} through:"),
            format!("  {}", report.firewall.allow_command(port)),
        ],
    }
}

#[cfg(test)]
#[path = "firewall_tests.rs"]
mod tests;
