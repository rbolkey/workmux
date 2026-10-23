//! The policy an OpenShell project sandbox gets: workmux's base policy with
//! the project's policy file merged on top (ticket 21).
//!
//! OpenShell takes one policy file and fills in missing top-level sections
//! from its default, so the merged file always carries a full
//! `filesystem_policy`. Filesystem lists are a union (Landlock grants add
//! up); network rules are a map union in which a clash is refused.

use anyhow::{Result, anyhow, bail};
use serde_yaml::{Mapping, Value};

/// workmux's base policy.
pub const BASE: &str = include_str!("base-policy.yaml");

const FILESYSTEM: &str = "filesystem_policy";
const NETWORK: &str = "network_policies";
const LISTS: [&str; 2] = ["read_only", "read_write"];

fn parse(text: &str, what: &str) -> Result<Mapping> {
    match serde_yaml::from_str::<Value>(text)
        .map_err(|error| anyhow!("{what} does not parse: {error}"))?
    {
        Value::Mapping(map) => Ok(map),
        Value::Null => Ok(Mapping::new()),
        _ => bail!("{what} is not a mapping"),
    }
}

fn key(name: &str) -> Value {
    Value::String(name.to_string())
}

/// The policy for a sandbox whose project's policy file reads `user`.
pub fn effective(user: Option<&str>) -> Result<Value> {
    merge(BASE, user.unwrap_or(""))
}

/// `user` merged on top of `base`.
pub fn merge(base: &str, user: &str) -> Result<Value> {
    let mut merged = parse(base, "workmux's base policy")?;
    let user = parse(user, "the project's policy file")?;
    for (name, value) in user {
        match name.as_str() {
            Some(FILESYSTEM) => merge_filesystem(&mut merged, value)?,
            Some(NETWORK) => merge_network(&mut merged, value)?,
            _ => {
                merged.insert(name, value);
            }
        }
    }
    if let Some(Value::Mapping(filesystem)) = merged.get_mut(key(FILESYSTEM)) {
        for list in LISTS {
            if let Some(Value::Sequence(paths)) = filesystem.get_mut(key(list)) {
                let mut seen = Vec::new();
                paths.retain(|path| {
                    let new = !seen.contains(path);
                    seen.push(path.clone());
                    new
                });
            }
        }
    }
    Ok(Value::Mapping(merged))
}

fn merge_filesystem(merged: &mut Mapping, user: Value) -> Result<()> {
    let Value::Mapping(user) = user else {
        bail!("{FILESYSTEM} in the project's policy file is not a mapping");
    };
    let filesystem = merged
        .entry(key(FILESYSTEM))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    let Value::Mapping(filesystem) = filesystem else {
        bail!("{FILESYSTEM} in workmux's base policy is not a mapping");
    };
    for (name, value) in user {
        let is_list = name.as_str().is_some_and(|name| LISTS.contains(&name));
        match (is_list, filesystem.get_mut(&name), value) {
            (true, Some(Value::Sequence(paths)), Value::Sequence(more)) => paths.extend(more),
            (_, _, value) => {
                filesystem.insert(name, value);
            }
        }
    }
    Ok(())
}

fn merge_network(merged: &mut Mapping, user: Value) -> Result<()> {
    let Value::Mapping(user) = user else {
        bail!("{NETWORK} in the project's policy file is not a mapping");
    };
    let network = merged
        .entry(key(NETWORK))
        .or_insert_with(|| Value::Mapping(Mapping::new()));
    let Value::Mapping(network) = network else {
        bail!("{NETWORK} in workmux's base policy is not a mapping");
    };
    for (name, rule) in user {
        if network.contains_key(&name) {
            bail!(
                "network rule {} in the project's policy file clashes with workmux's base policy; rename it",
                name.as_str().unwrap_or("?")
            );
        }
        network.insert(name, rule);
    }
    Ok(())
}

/// Filesystem grants `current` has that `desired` takes away: paths no
/// longer granted, or no longer writable. OpenShell cannot remove a
/// create-time grant, so these need a recreate.
pub fn narrowed(current: &Value, desired: &Value) -> Vec<String> {
    let paths = |policy: &Value, list: &str| -> Vec<String> {
        policy
            .get(FILESYSTEM)
            .and_then(|filesystem| filesystem.get(list))
            .and_then(Value::as_sequence)
            .map(|paths| {
                paths
                    .iter()
                    .filter_map(|path| path.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    };
    let (desired_ro, desired_rw) = (paths(desired, "read_only"), paths(desired, "read_write"));
    let mut narrowed = Vec::new();
    for path in paths(current, "read_write") {
        if !desired_rw.contains(&path) {
            narrowed.push(path);
        }
    }
    for path in paths(current, "read_only") {
        if !desired_ro.contains(&path) && !desired_rw.contains(&path) && !narrowed.contains(&path)
        {
            narrowed.push(path);
        }
    }
    narrowed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(policy: &Value, section: &str, name: &str) -> Vec<String> {
        serde_yaml::from_value(policy[section][name].clone()).unwrap()
    }

    #[test]
    fn without_a_policy_file_the_sandbox_gets_the_base_policy() {
        let policy = effective(None).unwrap();

        assert_eq!(
            list(&policy, FILESYSTEM, "read_write"),
            ["/tmp", "/dev/null", "/dev/pts", "/dev/ptmx"]
        );
        assert!(list(&policy, FILESYSTEM, "read_only").contains(&"/nix".to_string()));
        assert_eq!(policy["landlock"]["compatibility"], "best_effort");
    }

    #[test]
    fn a_filesystem_section_adds_to_the_base_grants_instead_of_replacing_them() {
        let policy = effective(Some(
            "filesystem_policy:\n  read_only: [/opt/tools, /usr]\n  read_write: [/data]\n",
        ))
        .unwrap();

        assert_eq!(
            list(&policy, FILESYSTEM, "read_write"),
            ["/tmp", "/dev/null", "/dev/pts", "/dev/ptmx", "/data"]
        );
        assert_eq!(
            list(&policy, FILESYSTEM, "read_only"),
            ["/bin", "/usr", "/lib", "/proc", "/dev/urandom", "/etc", "/var/log", "/nix", "/opt/tools"]
        );
        assert_eq!(policy[FILESYSTEM]["include_workdir"], true);
    }

    #[test]
    fn the_projects_network_rules_and_other_sections_are_kept() {
        let policy = effective(Some(
            "network_policies:\n  github:\n    endpoints:\n      - { host: github.com, port: 443 }\nlandlock:\n  compatibility: hard_requirement\n",
        ))
        .unwrap();

        assert_eq!(policy[NETWORK]["github"]["endpoints"][0]["host"], "github.com");
        assert_eq!(policy["landlock"]["compatibility"], "hard_requirement");
    }

    #[test]
    fn a_network_rule_clashing_with_the_base_is_refused() {
        let error = merge(
            "network_policies:\n  github: {}\n",
            "network_policies:\n  github: {}\n",
        )
        .unwrap_err();

        assert!(error.to_string().contains("network rule github"), "{error}");
    }

    #[test]
    fn an_unreadable_policy_file_is_an_error() {
        assert!(effective(Some("[a, b]")).is_err());
        assert!(effective(Some("filesystem_policy: [")).is_err());
    }

    #[test]
    fn removing_or_write_protecting_a_granted_path_is_a_narrowing() {
        let current = effective(Some(
            "filesystem_policy:\n  read_only: [/opt]\n  read_write: [/data, /cache]\n",
        ))
        .unwrap();
        let desired = effective(Some(
            "filesystem_policy:\n  read_only: [/cache, /more]\n",
        ))
        .unwrap();

        assert_eq!(narrowed(&current, &desired), ["/data", "/cache", "/opt"]);
        assert!(narrowed(&current, &current).is_empty());
        // Widening is applied in place.
        assert!(narrowed(&effective(None).unwrap(), &current).is_empty());
    }
}
