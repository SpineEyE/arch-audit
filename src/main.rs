use crate::enums::Status;
use atty::Stream;
use clap::{load_yaml, App};
use curl::easy::Easy;
use itertools::Itertools;
use log::{debug, info};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::btree_map::Entry::{Occupied, Vacant};
use std::collections::BTreeMap;
use std::default::Default;
use std::process::exit;
use std::str;

mod avg;
mod enums;

const WEBSITE: &str = "https://security.archlinux.org";
const ROOT_DIR: &str = "/";
const DB_PATH: &str = "/var/lib/pacman/";

#[derive(Default)]
struct Options {
    format: Option<String>,
    quiet: u64,
    upgradable_only: bool,
    show_testing: bool,
}

fn main() {
    env_logger::init();

    let yaml = load_yaml!("cli.yml");
    let args = App::from_yaml(yaml).get_matches();

    let options = Options {
        format: {
            match args.value_of("format") {
                Some(f) => Some(f.to_string()),
                None => None,
            }
        },
        quiet: args.occurrences_of("quiet"),
        upgradable_only: args.is_present("upgradable"),
        show_testing: args.is_present("testing"),
    };

    let mut avgs = String::new();
    {
        info!("Downloading AVGs...");
        let avgs_url = format!("{}/issues/all.json", WEBSITE);

        let mut easy = Easy::new();
        easy.fail_on_error(true)
            .expect("curl::Easy::fail_on_error failed");
        easy.follow_location(true)
            .expect("curl::Easy::follow_location failed");
        easy.url(&avgs_url).expect("curl::Easy::url failed");
        let mut transfer = easy.transfer();
        transfer
            .write_function(|data| {
                avgs.push_str(str::from_utf8(data).expect("str conversion failed"));
                Ok(data.len())
            })
            .expect("write_function failed");
        match transfer.perform() {
            Ok(_) => {}
            Err(_) => {
                println!(
                    "Cannot fetch data from {}, please check your network connection!",
                    WEBSITE
                );
                exit(1)
            }
        };
    }

    let pacman = match args.value_of("dbpath") {
        Some(path) => {
            alpm::Alpm::new(ROOT_DIR, path).expect("alpm::Alpm::new with custom dbpath failed")
        }
        None => alpm::Alpm::new(ROOT_DIR, DB_PATH).expect("alpm::Alpm::new failed"),
    };
    let db = pacman.localdb();

    let mut cves: BTreeMap<String, Vec<_>> = BTreeMap::new();
    {
        let json: Value = serde_json::from_str(&avgs).expect("serde_json::from_str failed");

        for avg in json.as_array().expect("Value::as_array failed") {
            let packages = avg["packages"]
                .as_array()
                .expect("Value::as_array failed")
                .iter()
                .map(|s| s.as_str().expect("Value::as_str failed").to_string())
                .collect::<Vec<_>>();

            if !package_is_installed(&db, &packages) {
                continue;
            }

            let info = to_avg(avg);

            if info.status != enums::Status::NotAffected {
                for p in packages {
                    match cves.entry(p) {
                        Occupied(c) => c.into_mut(),
                        Vacant(c) => c.insert(vec![]),
                    }
                    .push(info.clone());
                }
            }
        }
    }

    let mut affected_avgs: BTreeMap<String, Vec<_>> = BTreeMap::new();
    for (pkg, avgs) in cves {
        for avg in &avgs {
            if system_is_affected(&db, &pkg, avg) {
                match affected_avgs.entry(pkg.clone()) {
                    Occupied(c) => c.into_mut(),
                    Vacant(c) => c.insert(vec![]),
                }
                .push(avg.clone());
            }
        }
    }

    let merged = merge_avgs(&affected_avgs);
    print_avgs(&options, &merged);
}

/// Converts a JSON to an `avg::AVG`
fn to_avg(data: &Value) -> avg::AVG {
    avg::AVG {
        issues: data["issues"]
            .as_array()
            .expect("Value::as_array failed")
            .iter()
            .map(|s| s.as_str().expect("Value::as_str failed").to_string())
            .collect(),
        fixed: match data["fixed"].as_str() {
            Some(s) => Some(s.to_string()),
            None => None,
        },
        severity: data["severity"]
            .as_str()
            .expect("Value::as_str failed")
            .to_string()
            .parse::<enums::Severity>()
            .expect("parse::<Severity> failed"),
        status: data["status"]
            .as_str()
            .expect("Value::as_str failed")
            .to_string()
            .parse::<enums::Status>()
            .expect("parse::<Status> failed"),
    }
}

#[test]
fn test_to_avg() {
    let json: Value = serde_json::from_str(
        "{\"issues\": [\"CVE-1\", \"CVE-2\"], \"fixed\": \"1.0\", \
         \"severity\": \"High\", \"status\": \"Not affected\"}",
    )
    .expect("serde_json::from_str failed");

    let avg1 = to_avg(&json);
    assert_eq!(2, avg1.issues.len());
    assert_eq!(Some("1.0".to_string()), avg1.fixed);
    assert_eq!(enums::Severity::High, avg1.severity);
    assert_eq!(enums::Status::NotAffected, avg1.status);

    let json: Value = serde_json::from_str(
        "{\"issues\": [\"CVE-1\"], \"fixed\": null, \
         \"severity\": \"Low\", \"status\": \"Vulnerable\"}",
    )
    .expect("serde_json::from_str failed");

    let avg2 = to_avg(&json);
    assert_eq!(1, avg2.issues.len());
    assert_eq!(None, avg2.fixed);
    assert_eq!(enums::Severity::Low, avg2.severity);
    assert_eq!(enums::Status::Vulnerable, avg2.status);
}

/// Given a package and an `avg::AVG`, returns true if the system is affected
fn system_is_affected(db: &alpm::Db, pkg: &str, avg: &avg::AVG) -> bool {
    match db.pkg(pkg) {
        Ok(v) => {
            info!(
                "Found installed version {} for package {}",
                v.version(),
                pkg
            );
            match avg.fixed {
                Some(ref version) => {
                    info!("Comparing with fixed version {}", version);
                    let cmp = alpm::vercmp(v.version().to_string(), version.clone());
                    if let Ordering::Less = cmp {
                        return true;
                    }
                }
                None => return true,
            };
        }
        Err(_) => debug!("Package {} not installed", pkg),
    }

    false
}

#[test]
fn test_system_is_affected() {
    let pacman = alpm::Alpm::new(ROOT_DIR, DB_PATH).expect("Alpm::new failed");
    let db = pacman.localdb();

    let avg1 = avg::AVG {
        issues: vec!["CVE-1".to_string(), "CVE-2".to_string()],
        fixed: Some("1.0.0".to_string()),
        severity: enums::Severity::Unknown,
        status: enums::Status::Unknown,
    };

    assert_eq!(false, system_is_affected(&db, &"pacman".to_string(), &avg1));

    let avg2 = avg::AVG {
        issues: vec!["CVE-1".to_string(), "CVE-2".to_string()],
        fixed: Some("7.0.0".to_string()),
        severity: enums::Severity::Unknown,
        status: enums::Status::Unknown,
    };

    assert!(system_is_affected(&db, &"pacman".to_string(), &avg2));
}

/// Given a list of package names, returns true when at least one is installed
fn package_is_installed(db: &alpm::Db, packages: &[String]) -> bool {
    for pkg in packages {
        match db.pkg(pkg.as_str()) {
            Ok(_) => {
                info!("Package {} is installed", pkg);
                return true;
            }
            Err(_) => debug!("Package {} not installed", pkg),
        }
    }
    false
}

#[test]
fn test_package_is_installed() {
    let pacman = alpm::Alpm::new(ROOT_DIR, DB_PATH).expect("Alpm::new failed");
    let db = pacman.localdb();

    let packages = vec!["pacman".to_string(), "pac".to_string()];
    assert!(package_is_installed(&db, &packages));

    let packages = vec!["pac".to_string()];
    assert_eq!(false, package_is_installed(&db, &packages));
}

/// Merge a list of `avg::AVG` into a single `avg::AVG` using major version as version
fn merge_avgs(cves: &BTreeMap<String, Vec<avg::AVG>>) -> BTreeMap<String, avg::AVG> {
    let mut avgs: BTreeMap<String, avg::AVG> = BTreeMap::new();
    for (pkg, list) in cves.iter() {
        let mut avg_issues = vec![];
        let mut avg_fixed: Option<String> = None;
        let mut avg_severity = enums::Severity::Unknown;
        let mut avg_status = enums::Status::Unknown;

        for a in list.iter() {
            avg_issues.append(&mut a.issues.clone());

            match avg_fixed.clone() {
                Some(ref version) => {
                    if let Some(ref v) = a.fixed {
                        let cmp = alpm::vercmp(version.to_string(), v.to_string());
                        if let Ordering::Greater = cmp {
                            avg_fixed = a.fixed.clone();
                        }
                    }
                }
                None => avg_fixed = a.fixed.clone(),
            }

            if a.severity > avg_severity {
                avg_severity = a.severity;
            }

            if a.status > avg_status {
                avg_status = a.status;
            }
        }

        let avg = avg::AVG {
            issues: avg_issues,
            fixed: avg_fixed,
            severity: avg_severity,
            status: avg_status,
        };
        avgs.insert(pkg.to_string(), avg);
    }

    avgs
}

#[test]
fn test_merge_avgs() {
    let mut avgs: BTreeMap<String, Vec<_>> = BTreeMap::new();

    let avg1 = avg::AVG {
        issues: vec!["CVE-1".to_string(), "CVE-2".to_string()],
        fixed: Some("1.0.0".to_string()),
        severity: enums::Severity::Unknown,
        status: enums::Status::Fixed,
    };

    let avg2 = avg::AVG {
        issues: vec!["CVE-4".to_string(), "CVE-10".to_string()],
        fixed: Some("0.9.8".to_string()),
        severity: enums::Severity::High,
        status: enums::Status::Testing,
    };

    assert!(enums::Severity::Critical > enums::Severity::High);

    avgs.insert("package".to_string(), vec![avg1.clone(), avg2.clone()]);

    avgs.insert("package2".to_string(), vec![avg1, avg2]);

    let merged = merge_avgs(&avgs);

    assert_eq!(2, merged.len());
    assert_eq!(
        4,
        merged
            .get(&"package".to_string())
            .expect("'package' key not found")
            .issues
            .len()
    );
    assert_eq!(
        enums::Severity::High,
        merged
            .get(&"package".to_string())
            .expect("'package' key not found")
            .severity
    );
    assert_eq!(
        enums::Status::Testing,
        merged
            .get(&"package".to_string())
            .expect("'package' key not found")
            .status
    );
}

/// Print a list of `avg::AVG`
fn print_avgs(options: &Options, avgs: &BTreeMap<String, avg::AVG>) {
    let mut t = term::stdout().expect("term::stdout failed");
    for (pkg, avg) in avgs {
        match avg.fixed {
            Some(ref v) if avg.status != enums::Status::Vulnerable => {
                // Quiet option
                if options.quiet >= 1 {
                    write_with_colours(&mut *t, pkg, Some(avg.severity.to_color()), None);

                    if options.quiet == 1 {
                        write!(t, ">=").expect("term::write failed");
                        write_with_colours(&mut *t, v, Some(term::color::GREEN), None);
                    }
                } else {
                    match options.format {
                        Some(ref f) => {
                            print_avg_formatted(&mut *t, pkg, avg, v, options.show_testing, f);
                        }
                        None => {
                            print_avg_colored(&mut *t, pkg, avg, v, options.show_testing);
                        }
                    }
                }

                writeln!(t).expect("term::writeln failed");
            }
            _ => {
                if !options.upgradable_only {
                    if options.quiet > 0 {
                        write_with_colours(&mut *t, pkg, Some(avg.severity.to_color()), None);
                    } else {
                        match options.format {
                            Some(ref f) => {
                                print_avg_formatted(&mut *t, pkg, avg, "", options.show_testing, f);
                            }
                            None => {
                                print_avg_colored(&mut *t, pkg, avg, "", options.show_testing);
                            }
                        }

                        writeln!(t).expect("term::writeln failed");
                    }
                }
            }
        }
    }
}

/// Prints "Package {pkg} is affected by {issues}. {severity}!" colored
fn print_avg_colored(
    t: &mut term::StdoutTerminal,
    pkg: &str,
    avg: &avg::AVG,
    version: &str,
    show_testing: bool,
) {
    // Bold package
    write!(t, "Package ").expect("term::write failed");
    write_with_colours(t, pkg, None, Some(term::Attr::Bold));
    // Normal "is affected by {issues}"
    write!(t, " is affected by {}. ", avg.issues.join(", ")).expect("term::write failed");
    // Colored severit
    write_with_colours(
        t,
        avg.severity.to_string().as_str(),
        Some(avg.severity.to_color()),
        None,
    );
    write!(t, "!").expect("term::write failed");

    if !version.is_empty() {
        if avg.status == enums::Status::Fixed {
            // Print: Update to {}!
            write!(t, " Update to ").expect("term::write failed");
            write_with_colours(t, version, Some(term::color::GREEN), Some(term::Attr::Bold));
            write!(t, "!").expect("term::write failed");
        } else if avg.status == enums::Status::Testing && show_testing {
            // Print: Update to {} from the testing repos!"
            write!(t, " Update to ").expect("term::write failed");
            write_with_colours(t, version, Some(term::color::GREEN), Some(term::Attr::Bold));
            write!(t, " from the testing repos!").expect("term::write failed");
        }
    }
}

/// Prints output formatted as the user wants
fn print_avg_formatted(
    t: &mut term::StdoutTerminal,
    pkg: &str,
    avg: &avg::AVG,
    version: &str,
    show_testing: bool,
    f: &str,
) {
    let mut chars = f.chars().peekable();

    loop {
        match chars.next() {
            Some('%') => match chars.peek() {
                Some('n') => {
                    write_with_colours(t, pkg, Some(avg.severity.to_color()), None);
                    chars.next();
                }
                Some('c') => {
                    write!(t, "{}", avg.issues.iter().join(",").as_str())
                        .expect("term::write failed");
                    chars.next();
                }
                Some('v') => {
                    if !version.is_empty()
                        && (avg.status == Status::Fixed
                            || (avg.status == Status::Testing && show_testing))
                    {
                        write_with_colours(
                            t,
                            version,
                            Some(term::color::GREEN),
                            Some(term::Attr::Bold),
                        );
                    }
                    chars.next();
                }
                Some(x) => {
                    debug!("Unknown placeholder {}", x);
                    write!(t, "%").expect("term::write failed");
                }
                None => {}
            },
            Some(x) => {
                write!(t, "{}", x).expect("term::write failed");
            }
            None => break,
        }
    }
}

fn write_with_colours(
    t: &mut term::StdoutTerminal,
    text: &str,
    color: Option<term::color::Color>,
    attribute: Option<term::Attr>,
) {
    if atty::is(Stream::Stdout) {
        if let Some(c) = color {
            t.fg(c).expect("term::fg failed");
        }

        if let Some(a) = attribute {
            t.attr(a).expect("term::attr failed");
        }
    }

    write!(t, "{}", text).expect("term::write failed");

    if atty::is(Stream::Stdout) {
        t.reset().expect("term::stdout failed");
    }
}
