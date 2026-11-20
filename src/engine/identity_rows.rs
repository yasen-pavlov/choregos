//! Actionless reporting of the bus model's existing identity decisions.
use super::Engine;
use crate::{
    action::{IdentityDetail, IdentityKind, IdentityPair},
    cec::{
        bus::IdentityChange,
        frame::{Frame, physical_address},
        opcode::{DEVICE_VENDOR_ID, SET_OSD_NAME},
    },
};

struct IdentityRow {
    role: Option<String>,
    detail: IdentityDetail,
}

pub(super) struct IdentityRows {
    replaced: Option<IdentityRow>,
    moved: Option<IdentityRow>,
}

impl Engine {
    pub(super) fn capture_identity_changes(
        &self,
        frame: &Frame,
        change: IdentityChange,
        moved_from: Option<u8>,
    ) -> IdentityRows {
        let replaced = (change == IdentityChange::Replace)
            .then(|| self.identity_row(frame.from, frame.from, IdentityKind::Replaced, frame));
        let moved = if let Some(previous) = moved_from {
            Some(self.identity_row(previous, frame.from, IdentityKind::LaMove, frame))
        } else if change == IdentityChange::Physical {
            Some(self.identity_row(frame.from, frame.from, IdentityKind::PaChange, frame))
        } else {
            None
        };
        IdentityRows { replaced, moved }
    }

    fn identity_row(&self, from: u8, to: u8, kind: IdentityKind, frame: &Frame) -> IdentityRow {
        let old = &self.bus.entries[&from];
        let detail = IdentityDetail {
            kind,
            la: IdentityPair { from, to },
            pa: IdentityPair {
                from: old.physical.map(physical_address),
                to: None,
            },
            vendor: (kind == IdentityKind::Replaced && frame.opcode == Some(DEVICE_VENDOR_ID))
                .then(|| IdentityPair {
                    from: format!("{:06X}", old.vendor.expect("replacement has an old vendor")),
                    to: String::new(),
                }),
            osd: (kind == IdentityKind::Replaced && frame.opcode == Some(SET_OSD_NAME)).then(
                || IdentityPair {
                    from: old.osd.clone().expect("replacement has an old OSD"),
                    to: String::new(),
                },
            ),
        };
        IdentityRow {
            role: self.role_at(from),
            detail,
        }
    }

    fn role_at(&self, la: u8) -> Option<String> {
        self.identity
            .roles
            .iter()
            .find(|(_, address)| **address == la)
            .map(|(role, _)| role.clone())
    }

    pub(super) fn record_identity_changes(&mut self, changes: IdentityRows, la: u8) {
        for mut row in changes.replaced.into_iter().chain(changes.moved) {
            let new = &self.bus.entries[&la];
            row.detail.pa.to = new.physical.map(physical_address);
            if let Some(vendor) = &mut row.detail.vendor {
                vendor.to = format!(
                    "{:06X}",
                    new.vendor.expect("vendor reply learns the new vendor")
                );
            }
            if let Some(osd) = &mut row.detail.osd {
                osd.to
                    .clone_from(new.osd.as_ref().expect("OSD reply learns the new OSD"));
            }
            let reason = if row.detail.kind == IdentityKind::Replaced {
                "identity_replaced"
            } else {
                row.role = row.role.or_else(|| self.role_at(la));
                "identity_moved"
            };
            self.record(row.role, None, None, reason, vec![], None);
            self.decisions
                .last_mut()
                .expect("recorded identity")
                .identity = Some(Box::new(row.detail));
        }
    }
}
