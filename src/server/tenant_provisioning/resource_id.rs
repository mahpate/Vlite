//! design-tenant-provisioning EPIC-008: the strict grammar of the ARM
//! resource IDs a deployment profile may name and the ARM adapter may
//! address. An ID is appended verbatim to the management endpoint, so
//! anything outside it (another provider or type, an extra segment, a
//! query, fragment, escape or dot segment) is refused.

/// The resource types a tenant deployment profile may name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Type {
    ContainerApp,
    ManagedEnvironment,
    /// An environment's storage definition
    /// (`managedEnvironments/{env}/storages/{name}`).
    Storage,
}

/// The live singleton's resource names: never a tenant's to create or
/// change (`deploy/azure-container-apps/ha/templates.ps1` refuses them too).
const SINGLETON: [&str; 3] = ["vlite-app", "vlite-env", "vlite-data"];

/// The type and name of a strict
/// `/subscriptions/{guid}/resourceGroups/{group}/providers/Microsoft.App/…`
/// ID of a supported type. Fixed segments compare ASCII case-insensitively,
/// as ARM's do.
pub(super) fn parse(id: &str) -> Option<(Type, &str)> {
    if id.len() > 512 {
        return None;
    }
    let s: Vec<&str> = id.split('/').collect();
    let fixed = |i: usize, want: &str| s.get(i).is_some_and(|v| v.eq_ignore_ascii_case(want));
    let scoped = s.first() == Some(&"")
        && fixed(1, "subscriptions")
        && s.get(2).is_some_and(|v| guid(v))
        && fixed(3, "resourceGroups")
        && s.get(4).is_some_and(|v| group(v))
        && fixed(5, "providers")
        && fixed(6, "Microsoft.App");
    if !scoped {
        return None;
    }
    match s.len() {
        9 if fixed(7, "containerApps") && app_name(s[8]) => Some((Type::ContainerApp, s[8])),
        9 if fixed(7, "managedEnvironments") && name(s[8]) => {
            Some((Type::ManagedEnvironment, s[8]))
        }
        11 if fixed(7, "managedEnvironments")
            && name(s[8])
            && fixed(9, "storages")
            && name(s[10]) =>
        {
            Some((Type::Storage, s[10]))
        }
        _ => None,
    }
}

/// Whether `name` is one of the live singleton's resources.
pub(super) fn singleton(name: &str) -> bool {
    SINGLETON.iter().any(|s| s.eq_ignore_ascii_case(name))
}

fn guid(v: &str) -> bool {
    v.len() == 36
        && v.bytes().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// A resource group name: the ASCII subset of ARM's rule (1-90 letters,
/// digits, `-_.()`), not ending in a period.
fn group(v: &str) -> bool {
    (1..=90).contains(&v.len())
        && v.bytes()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.()".contains(&c))
        && !v.ends_with('.')
}

/// A container app name (ARM naming rules): 2-32 lowercase letters, digits
/// and hyphens, starting with a letter and ending with a letter or digit.
/// `--` separates a revision's suffix from its app, so it is refused too.
fn app_name(v: &str) -> bool {
    let b = v.as_bytes();
    (2..=32).contains(&b.len())
        && b[0].is_ascii_lowercase()
        && b[b.len() - 1] != b'-'
        && b.iter()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'-')
        && !v.contains("--")
}

/// Any other Microsoft.App name (no published rule): 1-64 ASCII letters,
/// digits, `-_.`, starting and ending with a letter or digit, so never a
/// dot segment.
fn name(v: &str) -> bool {
    let b = v.as_bytes();
    (1..=64).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
        && b.iter()
            .all(|c| c.is_ascii_alphanumeric() || b"-_.".contains(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCOPE: &str = "/subscriptions/00000000-0000-0000-0000-000000000000/resourceGroups/rg-1/providers/Microsoft.App";

    #[test]
    fn accepts_the_three_supported_types_case_insensitively() {
        let ok = |id: &str, kind, name| assert_eq!(parse(id), Some((kind, name)), "{id}");
        ok(
            &format!("{SCOPE}/containerApps/t-writer-1"),
            Type::ContainerApp,
            "t-writer-1",
        );
        ok(
            &format!("{SCOPE}/managedEnvironments/Env_1"),
            Type::ManagedEnvironment,
            "Env_1",
        );
        ok(
            &format!("{SCOPE}/managedEnvironments/env/storages/data.1"),
            Type::Storage,
            "data.1",
        );
        ok(
            "/SUBSCRIPTIONS/00000000-0000-0000-0000-000000000000/RESOURCEGROUPS/rg/PROVIDERS/microsoft.app/CONTAINERAPPS/w1",
            Type::ContainerApp,
            "w1",
        );
    }

    #[test]
    fn refuses_every_other_shape() {
        let bad = |id: &str| assert_eq!(parse(id), None, "{id}");
        // Each case changes one part of an otherwise valid ID.
        let valid = format!("{SCOPE}/containerApps/w1");
        assert!(parse(&valid).is_some());
        let swap = |from: &str, to: &str| valid.replacen(from, to, 1);
        bad("");
        bad(&valid[1..]);
        bad(&format!("/{valid}"));
        bad(&format!("{valid}/"));
        bad(&format!("{valid}/revisions/r1"));
        bad(&swap("00000000-0000-0000-0000-000000000000", "s"));
        bad(&swap(
            "00000000-0000-0000-0000-00000000000",
            "g0000000-0000-0000-0000-00000000000",
        ));
        bad(&swap("rg-1", "g."));
        bad(&swap("rg-1", "g:1"));
        bad(&swap("rg-1", &"g".repeat(91)));
        bad(&swap("Microsoft.App", "Microsoft.Storage"));
        bad(&swap("containerApps", "jobs"));
        for name in [
            "W1", "1w", "w", "w-", "a--b", "w1?x=1", "w1#f", "w%31", "w 1", "w\u{e9}1",
        ] {
            bad(&swap("/w1", &format!("/{name}")));
        }
        bad(&swap("/w1", &format!("/{}", "a".repeat(33))));
        let env = format!("{SCOPE}/managedEnvironments/e/storages/s1");
        assert!(parse(&env).is_some());
        bad(&env.replace("/e/", "/../"));
        bad(&env.replace("/s1", "/.x"));
        bad(&env.replace("storages", "certificates"));
        bad(&format!("{SCOPE}/managedEnvironments/{}", "e".repeat(65)));
        bad(&format!("{SCOPE}/containerApps/{}", "w".repeat(600)));
    }

    #[test]
    fn names_only_the_live_singleton() {
        for name in ["vlite-app", "VLITE-ENV", "vlite-data"] {
            assert!(singleton(name), "{name}");
        }
        for name in ["vlite-app-2", "t-vlite-app", "vlite"] {
            assert!(!singleton(name), "{name}");
        }
    }
}
