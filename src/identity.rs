use crate::{
    cec::{bus::BusModel, frame::parse_physical},
    config::DeviceConfig,
    state::Conflict,
};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Default)]
pub struct Resolution {
    pub roles: BTreeMap<String, u8>,
    pub conflicts: Vec<Conflict>,
    pub blocked: BTreeSet<String>,
}

pub fn resolve(devices: &[DeviceConfig], bus: &BusModel) -> Resolution {
    let mut result = Resolution::default();
    let mut by_la: BTreeMap<u8, Vec<String>> = BTreeMap::new();
    for device in devices {
        let matches = &device.matches;
        let addresses: Vec<_> =
            bus.entries
                .iter()
                .filter(|(la, entry)| {
                    **la != bus.bridge
                        && entry.verified
                        && matches.osd.as_ref().is_none_or(|osd| {
                            entry.osd.as_deref().map(str::trim) == Some(osd.trim())
                        })
                        && matches.vendor.as_ref().is_none_or(|vendor| {
                            entry.vendor == u32::from_str_radix(vendor, 16).ok()
                        })
                        && matches
                            .physical
                            .as_ref()
                            .is_none_or(|pa| entry.physical == parse_physical(pa))
                })
                .map(|(&la, _)| la)
                .collect();
        if addresses.len() > 1 {
            result.blocked.insert(device.role.clone());
            result.conflicts.push(Conflict {
                roles: vec![device.role.clone()],
                logical_addresses: addresses.clone(),
                reason: "role matches multiple logical addresses".into(),
            });
        }
        for la in addresses {
            by_la.entry(la).or_default().push(device.role.clone());
            result.roles.insert(device.role.clone(), la);
        }
    }
    for (la, roles) in by_la {
        if roles.len() > 1 {
            result.blocked.extend(roles.iter().cloned());
            result.conflicts.push(Conflict {
                roles,
                logical_addresses: vec![la],
                reason: "logical address matches multiple roles".into(),
            });
        }
    }
    result
        .roles
        .retain(|role, _| !result.blocked.contains(role));
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{cec::frame::Frame, config::Config};
    use std::time::Instant;
    #[test]
    fn ambiguous_roles_and_addresses_are_blocked() {
        let mut config = Config::parse(include_str!("../choregos.example.toml")).unwrap();
        let mut bus = BusModel::new(0, 10, crate::action::IdentifyQueries::ALL);
        for la in [4, 8] {
            bus.handle(
                &Frame::from_hex(la, 0, "474170706C65205456").unwrap(),
                Instant::now(),
                chrono::DateTime::UNIX_EPOCH,
                false,
            );
            bus.handle(
                &Frame::from_hex(la, 15, "870010FA").unwrap(),
                Instant::now(),
                chrono::DateTime::UNIX_EPOCH,
                false,
            );
        }
        assert!(resolve(&config.device, &bus).blocked.contains("streamer"));
        bus.entries.remove(&8);
        let mut duplicate = config.device[0].clone();
        duplicate.role = "duplicate".into();
        config.device.push(duplicate);
        let resolution = resolve(&config.device, &bus);
        assert_eq!(resolution.blocked.len(), 2);
        assert!(resolution.roles.is_empty());
    }
}
